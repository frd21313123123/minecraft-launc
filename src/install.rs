use std::collections::HashMap;
use std::fs::{self, File};
use std::io::copy;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zip::ZipArchive;

use crate::download::{self, download_file, download_json, ProgressFn};
use crate::error::LauncherError;
use crate::models::{AssetIndex, Library, VersionEntry, VersionInfo, VersionJson, VersionManifest};
use crate::paths::{assets_dir, ensure_dirs, game_dir, libraries_dir, natives_dir, versions_dir};
use crate::rules::{library_applies, native_classifier};

const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const RESOURCES_URL: &str = "https://resources.download.minecraft.net";
const ASSET_DOWNLOAD_CONCURRENCY: usize = 16;
const ASSET_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const ASSET_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

#[derive(Debug)]
struct AssetDownload {
    name: String,
    hash: String,
    size: u64,
    url: String,
    dest: PathBuf,
}

struct AssetProgressState {
    completed_bytes: u64,
    completed_files: usize,
    in_flight: Vec<u64>,
    in_flight_bytes: u64,
    displayed_bytes: u64,
    last_emit: Instant,
}

pub fn fetch_versions(include_snapshots: bool) -> Result<Vec<VersionInfo>, LauncherError> {
    let client = download::http_client()?;
    let manifest: VersionManifest = download_json(&client, VERSION_MANIFEST_URL)?;
    let mut out = Vec::new();
    for v in manifest.versions {
        if v.version_type == "release" {
            out.push(VersionInfo {
                id: v.id.clone(),
                version_type: v.version_type.clone(),
                url: v.url.clone(),
                label: v.id.clone(),
            });
        } else if include_snapshots && v.version_type == "snapshot" {
            out.push(VersionInfo {
                id: v.id.clone(),
                version_type: v.version_type.clone(),
                url: v.url.clone(),
                label: format!("{} (snapshot)", v.id),
            });
        }
    }
    Ok(out)
}

pub fn installed_versions() -> Vec<String> {
    let dir = versions_dir();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_version_installed(&name) {
            ids.push(name);
        }
    }
    ids.sort();
    ids.reverse();
    ids
}

pub fn is_version_installed(version_id: &str) -> bool {
    is_version_installed_at(version_id, &versions_dir(), &assets_dir())
}

fn is_version_installed_at(version_id: &str, versions_root: &Path, assets_root: &Path) -> bool {
    let path = versions_root.join(version_id);
    let json = path.join(format!("{version_id}.json"));
    if !json.is_file() {
        return false;
    }

    let Ok(data) = fs::read_to_string(&json) else {
        return false;
    };
    let Ok(version) = serde_json::from_str::<VersionJson>(&data) else {
        return false;
    };

    // Loader-версии используют runtime родительской vanilla-версии и могут не
    // иметь собственного jar или assetIndex.
    if version.inherits_from.is_some() {
        return true;
    }

    if !path.join(format!("{version_id}.jar")).is_file() {
        return false;
    }

    version_assets_complete(&version, assets_root)
}

fn version_assets_complete(version: &VersionJson, assets_root: &Path) -> bool {
    let Some(asset_index) = &version.asset_index else {
        return true;
    };
    let index_path = assets_root
        .join("indexes")
        .join(format!("{}.json", asset_index.id));
    let Ok(data) = fs::read_to_string(index_path) else {
        return false;
    };
    let Ok(index) = serde_json::from_str::<AssetIndex>(&data) else {
        return false;
    };

    index.objects.values().all(|object| {
        let Ok(path) = asset_object_path(assets_root, &object.hash) else {
            return false;
        };
        fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.len() == object.size)
            .unwrap_or(false)
    })
}

fn find_version_entry(
    client: &reqwest::blocking::Client,
    version_id: &str,
) -> Result<VersionEntry, LauncherError> {
    let manifest: VersionManifest = download_json(client, VERSION_MANIFEST_URL)?;
    manifest
        .versions
        .into_iter()
        .find(|v| v.id == version_id)
        .ok_or_else(|| LauncherError::VersionNotFound(version_id.to_string()))
}

pub fn load_version_json(version_id: &str) -> Result<VersionJson, LauncherError> {
    load_version_json_merged(version_id, 0)
}

fn load_version_json_raw(version_id: &str) -> Result<VersionJson, LauncherError> {
    let path = versions_dir()
        .join(version_id)
        .join(format!("{version_id}.json"));
    if !path.is_file() {
        return Err(LauncherError::VersionNotFound(version_id.to_string()));
    }
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|e| LauncherError::Parse(e.to_string()))
}

/// Загружает version.json и рекурсивно сливает `inheritsFrom` (Forge/NeoForge).
fn load_version_json_merged(version_id: &str, depth: u8) -> Result<VersionJson, LauncherError> {
    if depth > 6 {
        return Err(LauncherError::Parse(
            "Слишком глубокое наследование version.json".into(),
        ));
    }
    let child = load_version_json_raw(version_id)?;
    let Some(parent_id) = child.inherits_from.clone() else {
        return Ok(child);
    };
    // Родитель должен быть установлен (vanilla).
    let parent = load_version_json_merged(&parent_id, depth + 1)?;
    Ok(merge_versions(parent, child))
}

fn merge_versions(parent: VersionJson, mut child: VersionJson) -> VersionJson {
    // libraries: parent first, child overrides by exact name (иначе дубли gson и т.п.
    // роняют BootstrapLauncher: Duplicate key …jar).
    child.libraries = merge_libraries(parent.libraries, std::mem::take(&mut child.libraries));

    if child.main_class.is_empty() {
        child.main_class = parent.main_class;
    }
    if child.downloads.is_none() {
        child.downloads = parent.downloads;
    }
    if child.asset_index.is_none() {
        child.asset_index = parent.asset_index;
    }
    if child.assets.is_none() {
        child.assets = parent.assets;
    }
    if child.java_version.is_none() {
        child.java_version = parent.java_version;
    }
    if child.minecraft_arguments.is_none() {
        child.minecraft_arguments = parent.minecraft_arguments;
    }

    // arguments: merge game/jvm lists (parent then child — как у Mojang launcher)
    child.arguments = match (parent.arguments, child.arguments) {
        (None, c) => c,
        (p, None) => p,
        (Some(p), Some(c)) => {
            let game = match (p.game, c.game) {
                (None, g) => g,
                (g, None) => g,
                (Some(mut a), Some(b)) => {
                    a.extend(b);
                    Some(a)
                }
            };
            let jvm = match (p.jvm, c.jvm) {
                (None, g) => g,
                (g, None) => g,
                (Some(mut a), Some(b)) => {
                    a.extend(b);
                    Some(a)
                }
            };
            Some(crate::models::Arguments { game, jvm })
        }
    };

    // jar: если у child нет своего jar-имени — берём parent id для client jar
    if child.jar.is_none() {
        child.jar = parent.jar.or(Some(parent.id));
    }

    // inheritsFrom больше не нужен после merge
    child.inherits_from = None;
    child
}

/// Слияние библиотек parent+child: одинаковые `name` — побеждает child, порядок сохраняется.
fn merge_libraries(parent: Vec<Library>, child: Vec<Library>) -> Vec<Library> {
    use std::collections::HashMap;
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Library> = Vec::with_capacity(parent.len() + child.len());
    for lib in parent {
        if let Some(&i) = index.get(&lib.name) {
            out[i] = lib;
        } else {
            index.insert(lib.name.clone(), out.len());
            out.push(lib);
        }
    }
    for lib in child {
        if let Some(&i) = index.get(&lib.name) {
            out[i] = lib;
        } else {
            index.insert(lib.name.clone(), out.len());
            out.push(lib);
        }
    }
    out
}

/// Путь к client jar с учётом `jar` / inheritsFrom.
pub fn client_jar_path(version: &VersionJson) -> PathBuf {
    let jar_id = version.jar.as_deref().unwrap_or(version.id.as_str());
    // jar может указывать на id родительской версии
    let primary = versions_dir().join(jar_id).join(format!("{jar_id}.jar"));
    if primary.exists() {
        return primary;
    }
    // fallback: jar рядом с version id
    versions_dir()
        .join(&version.id)
        .join(format!("{}.jar", version.id))
}

/// Распаковать natives для версии (нужно loader-версиям после installer).
pub fn ensure_natives_for_version(version_id: &str) -> Result<(), LauncherError> {
    let version = load_version_json(version_id)?;
    let nat_dir = natives_dir(version_id);
    // Если уже есть dll/so — ок
    if nat_dir.is_dir() {
        if let Ok(mut it) = fs::read_dir(&nat_dir) {
            if it.next().is_some() {
                return Ok(());
            }
        }
    }
    fs::create_dir_all(&nat_dir)?;
    for lib in &version.libraries {
        if library_applies(lib) {
            extract_natives_if_needed(lib, &nat_dir)?;
        }
    }
    Ok(())
}

pub fn install_version(
    version_id: &str,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<(), LauncherError> {
    ensure_dirs()?;
    let client = download::http_client()?;

    progress(0, 1, &format!("Получение метаданных {version_id}…"));

    let entry = find_version_entry(&client, version_id)?;
    let version_dir = versions_dir().join(version_id);
    fs::create_dir_all(&version_dir)?;

    let version_json_path = version_dir.join(format!("{version_id}.json"));
    download_file(
        &client,
        &entry.url,
        &version_json_path,
        None,
        Some(&progress),
        "version.json",
    )?;

    if cancel.load(Ordering::Relaxed) {
        return Err(LauncherError::Other("Отменено".into()));
    }

    let version: VersionJson = {
        let data = fs::read_to_string(&version_json_path)?;
        serde_json::from_str(&data).map_err(|e| LauncherError::Parse(e.to_string()))?
    };

    // Client jar (у NeoForge/Forge может отсутствовать — jar от inheritsFrom)
    if let Some(downloads) = &version.downloads {
        progress(0, 1, "Скачивание клиента…");
        let client_jar = version_dir.join(format!("{version_id}.jar"));
        download_file(
            &client,
            &downloads.client.url,
            &client_jar,
            downloads.client.sha1.as_deref(),
            Some(&progress),
            &format!("{version_id}.jar"),
        )?;
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(LauncherError::Other("Отменено".into()));
    }

    // Libraries
    let libs: Vec<&Library> = version
        .libraries
        .iter()
        .filter(|l| library_applies(l))
        .collect();

    let total_libs = libs.len() as u64;
    for (i, lib) in libs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(LauncherError::Other("Отменено".into()));
        }
        progress(
            i as u64,
            total_libs.max(1),
            &format!("Библиотека: {}", lib.name),
        );
        install_library(&client, lib, &progress)?;
    }

    // Natives extract
    progress(0, 1, "Распаковка natives…");
    let nat_dir = natives_dir(version_id);
    if nat_dir.exists() {
        let _ = fs::remove_dir_all(&nat_dir);
    }
    fs::create_dir_all(&nat_dir)?;

    for lib in &libs {
        extract_natives_if_needed(lib, &nat_dir)?;
    }

    // Assets
    if let Some(asset_index) = &version.asset_index {
        progress(0, 1, "Индекс ассетов…");
        let index_path = assets_dir()
            .join("indexes")
            .join(format!("{}.json", asset_index.id));
        download_file(
            &client,
            &asset_index.url,
            &index_path,
            asset_index.sha1.as_deref(),
            Some(&progress),
            "asset index",
        )?;

        let index: AssetIndex = {
            let data = fs::read_to_string(&index_path)?;
            serde_json::from_str(&data).map_err(|e| LauncherError::Parse(e.to_string()))?
        };

        download_assets(
            &client,
            &index,
            &assets_dir(),
            RESOURCES_URL,
            ASSET_DOWNLOAD_CONCURRENCY,
            &ASSET_RETRY_DELAYS,
            &progress,
            &cancel,
        )?;
    }

    progress(1, 1, &format!("Версия {version_id} готова"));
    let _ = game_dir(); // ensure path used
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn download_assets(
    client: &reqwest::blocking::Client,
    index: &AssetIndex,
    assets_root: &Path,
    resources_url: &str,
    concurrency: usize,
    retry_delays: &[Duration],
    progress: &ProgressFn,
    cancel: &Arc<AtomicBool>,
) -> Result<(), LauncherError> {
    let jobs = build_asset_downloads(index, assets_root, resources_url)?;
    let total_files = jobs.len();
    let total_bytes = jobs.iter().map(|job| job.size).sum::<u64>();
    let progress_total = total_bytes.max(1);

    if total_files == 0 {
        progress(1, 1, "Ассеты готовы");
        return Ok(());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled_error());
    }

    progress(0, progress_total, &format!("Ассеты: 0 / {total_files}"));

    let next_job = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None);
    let progress_state = Arc::new(Mutex::new(AssetProgressState {
        completed_bytes: 0,
        completed_files: 0,
        in_flight: vec![0; total_files],
        in_flight_bytes: 0,
        displayed_bytes: 0,
        last_emit: Instant::now(),
    }));
    let worker_count = concurrency.max(1).min(total_files);

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let worker_client = client.clone();
            let jobs = &jobs;
            let next_job = &next_job;
            let failed = &failed;
            let first_error = &first_error;
            let progress_state = progress_state.clone();

            scope.spawn(move || loop {
                if cancel.load(Ordering::Relaxed) || failed.load(Ordering::Acquire) {
                    break;
                }

                let index = next_job.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(index) else {
                    break;
                };

                match download_asset_with_retry(
                    &worker_client,
                    job,
                    retry_delays,
                    cancel,
                    &progress_state,
                    index,
                    total_files,
                    total_bytes,
                    progress,
                ) {
                    Ok(()) => {}
                    Err(error) => {
                        failed.store(true, Ordering::Release);
                        let mut slot = first_error
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        break;
                    }
                }
            });
        }
    });

    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled_error());
    }
    if let Some(error) = first_error
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        return Err(error);
    }

    Ok(())
}

fn build_asset_downloads(
    index: &AssetIndex,
    assets_root: &Path,
    resources_url: &str,
) -> Result<Vec<AssetDownload>, LauncherError> {
    let mut unique = HashMap::<String, AssetDownload>::new();
    let base_url = resources_url.trim_end_matches('/');

    for (name, object) in &index.objects {
        let dest = asset_object_path(assets_root, &object.hash)?;
        if let Some(existing) = unique.get(&object.hash) {
            if existing.size != object.size {
                return Err(LauncherError::Parse(format!(
                    "Одинаковый SHA-1 ассета имеет разные размеры: {}",
                    object.hash
                )));
            }
            continue;
        }

        if fs::metadata(&dest)
            .map(|metadata| metadata.is_file() && metadata.len() == object.size)
            .unwrap_or(false)
        {
            continue;
        }

        let prefix = &object.hash[..2];
        unique.insert(
            object.hash.clone(),
            AssetDownload {
                name: name.clone(),
                hash: object.hash.clone(),
                size: object.size,
                url: format!("{base_url}/{prefix}/{}", object.hash),
                dest,
            },
        );
    }

    let mut jobs: Vec<_> = unique.into_values().collect();
    // Starting large files first reduces the long tail once the queue is almost empty.
    jobs.sort_unstable_by(|left, right| {
        right
            .size
            .cmp(&left.size)
            .then_with(|| left.hash.cmp(&right.hash))
    });
    Ok(jobs)
}

fn asset_object_path(assets_root: &Path, hash: &str) -> Result<PathBuf, LauncherError> {
    let Some(prefix) = hash.get(..2) else {
        return Err(LauncherError::Parse(format!(
            "Некорректный SHA-1 ассета: {hash}"
        )));
    };
    Ok(assets_root.join("objects").join(prefix).join(hash))
}

#[allow(clippy::too_many_arguments)]
fn download_asset_with_retry(
    client: &reqwest::blocking::Client,
    job: &AssetDownload,
    retry_delays: &[Duration],
    cancel: &AtomicBool,
    progress_state: &Arc<Mutex<AssetProgressState>>,
    job_index: usize,
    total_files: usize,
    total_bytes: u64,
    progress: &ProgressFn,
) -> Result<(), LauncherError> {
    for attempt in 0..=retry_delays.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_error());
        }

        reset_asset_attempt(progress_state, job_index);
        let file_progress: ProgressFn = Arc::new({
            let progress_state = progress_state.clone();
            let progress = progress.clone();
            let job_size = job.size;
            move |done, _, _| {
                report_asset_chunk(
                    &progress_state,
                    job_index,
                    done.min(job_size),
                    total_files,
                    total_bytes,
                    &progress,
                );
            }
        });
        let result = download_file(
            client,
            &job.url,
            &job.dest,
            Some(&job.hash),
            Some(&file_progress),
            &job.name,
        );
        match result {
            Ok(()) => {
                complete_asset_progress(
                    progress_state,
                    job_index,
                    job.size,
                    total_files,
                    total_bytes,
                    progress,
                );
                return Ok(());
            }
            Err(error) => {
                reset_asset_attempt(progress_state, job_index);
                if retryable_asset_error(&error) && attempt < retry_delays.len() {
                    wait_for_retry(retry_delays[attempt], cancel)?;
                } else {
                    return Err(error);
                }
            }
        }
    }

    unreachable!("asset retry loop always returns")
}

fn retryable_asset_error(error: &LauncherError) -> bool {
    matches!(
        error,
        LauncherError::Network(_) | LauncherError::Checksum { .. }
    )
}

fn wait_for_retry(delay: Duration, cancel: &AtomicBool) -> Result<(), LauncherError> {
    let deadline = Instant::now() + delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_error());
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

fn reset_asset_attempt(state: &Mutex<AssetProgressState>, job_index: usize) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = state.in_flight[job_index];
    state.in_flight[job_index] = 0;
    state.in_flight_bytes = state.in_flight_bytes.saturating_sub(previous);
}

fn report_asset_chunk(
    state: &Mutex<AssetProgressState>,
    job_index: usize,
    downloaded_bytes: u64,
    total_files: usize,
    total_bytes: u64,
    progress: &ProgressFn,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = state.in_flight[job_index];
    state.in_flight[job_index] = downloaded_bytes;
    state.in_flight_bytes = state
        .in_flight_bytes
        .saturating_sub(previous)
        .saturating_add(downloaded_bytes);
    emit_asset_progress(&mut state, total_files, total_bytes, false, progress);
}

fn complete_asset_progress(
    state: &Mutex<AssetProgressState>,
    job_index: usize,
    downloaded_bytes: u64,
    total_files: usize,
    total_bytes: u64,
    progress: &ProgressFn,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = state.in_flight[job_index];
    state.in_flight[job_index] = 0;
    state.in_flight_bytes = state.in_flight_bytes.saturating_sub(previous);
    state.completed_bytes = state.completed_bytes.saturating_add(downloaded_bytes);
    state.completed_files += 1;
    let complete = state.completed_files == total_files;
    emit_asset_progress(&mut state, total_files, total_bytes, complete, progress);
}

fn emit_asset_progress(
    state: &mut AssetProgressState,
    total_files: usize,
    total_bytes: u64,
    force: bool,
    progress: &ProgressFn,
) {
    let actual = state
        .completed_bytes
        .saturating_add(state.in_flight_bytes)
        .min(total_bytes);
    state.displayed_bytes = state.displayed_bytes.max(actual);

    if force || state.last_emit.elapsed() >= ASSET_PROGRESS_INTERVAL {
        state.last_emit = Instant::now();
        let done = if force {
            total_bytes.max(1)
        } else {
            state.displayed_bytes
        };
        progress(
            done,
            total_bytes.max(1),
            &format!("Ассеты: {} / {total_files}", state.completed_files),
        );
    }
}

fn cancelled_error() -> LauncherError {
    LauncherError::Other("Отменено".into())
}

fn install_library(
    client: &reqwest::blocking::Client,
    lib: &Library,
    progress: &ProgressFn,
) -> Result<(), LauncherError> {
    if let Some(downloads) = &lib.downloads {
        if let Some(artifact) = &downloads.artifact {
            let path = artifact_path(lib, artifact.path.as_deref())?;
            if !artifact.url.is_empty() {
                download_file(
                    client,
                    &artifact.url,
                    &path,
                    artifact.sha1.as_deref(),
                    Some(progress),
                    &lib.name,
                )?;
            }
        }
        if let Some(classifier) = native_classifier(lib) {
            if let Some(classifiers) = &downloads.classifiers {
                if let Some(art) = classifiers.get(&classifier) {
                    let path = artifact_path(lib, art.path.as_deref())?;
                    if !art.url.is_empty() {
                        download_file(
                            client,
                            &art.url,
                            &path,
                            art.sha1.as_deref(),
                            Some(progress),
                            &format!("{}:{}", lib.name, classifier),
                        )?;
                    }
                }
            }
        }
    } else {
        // Maven: lib.url base или libraries.minecraft.net
        let rel = maven_relative(&lib.name)?;
        let base = lib
            .url
            .as_deref()
            .filter(|u| !u.is_empty())
            .unwrap_or("https://libraries.minecraft.net/");
        let base = if base.ends_with('/') {
            base.to_string()
        } else {
            format!("{base}/")
        };
        let url = format!("{base}{rel}");
        let dest = libraries_dir().join(PathBuf::from(
            rel.replace('/', std::path::MAIN_SEPARATOR_STR),
        ));
        if !dest.exists() {
            download_file(client, &url, &dest, None, Some(progress), &lib.name)?;
        }
    }
    Ok(())
}

fn artifact_path(lib: &Library, explicit: Option<&str>) -> Result<PathBuf, LauncherError> {
    if let Some(p) = explicit {
        return Ok(libraries_dir().join(p.replace('/', std::path::MAIN_SEPARATOR_STR)));
    }
    maven_path_from_name(&lib.name)
}

fn maven_relative(name: &str) -> Result<String, LauncherError> {
    // group:artifact:version[:classifier]
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        return Err(LauncherError::Parse(format!(
            "Некорректная библиотека: {name}"
        )));
    }
    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let file = if parts.len() >= 4 {
        format!("{artifact}-{version}-{}.jar", parts[3])
    } else {
        format!("{artifact}-{version}.jar")
    };
    Ok(format!("{group}/{artifact}/{version}/{file}"))
}

fn maven_path_from_name(name: &str) -> Result<PathBuf, LauncherError> {
    Ok(libraries_dir().join(maven_relative(name)?.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

fn extract_natives_if_needed(lib: &Library, natives_out: &Path) -> Result<(), LauncherError> {
    let Some(classifier) = native_classifier(lib) else {
        return Ok(());
    };

    // Новый формат: сама библиотека — natives jar (artifact, не classifiers).
    let modern_native = crate::rules::name_native_classifier(&lib.name).is_some();

    let jar_path = if modern_native {
        if let Some(downloads) = &lib.downloads {
            if let Some(artifact) = &downloads.artifact {
                artifact_path(lib, artifact.path.as_deref())?
            } else {
                maven_path_from_name(&lib.name)?
            }
        } else {
            maven_path_from_name(&lib.name)?
        }
    } else if let Some(downloads) = &lib.downloads {
        if let Some(classifiers) = &downloads.classifiers {
            if let Some(art) = classifiers.get(&classifier) {
                artifact_path(lib, art.path.as_deref())?
            } else {
                return Ok(());
            }
        } else if let Some(artifact) = &downloads.artifact {
            // Иногда natives лежат как artifact с classifier в path
            artifact_path(lib, artifact.path.as_deref())?
        } else {
            return Ok(());
        }
    } else {
        // name with classifier
        let parts: Vec<&str> = lib.name.split(':').collect();
        if parts.len() < 3 {
            return Ok(());
        }
        let rel = if parts.len() >= 4 {
            format!(
                "{}/{}/{}/{}-{}-{}.jar",
                parts[0].replace('.', "/"),
                parts[1],
                parts[2],
                parts[1],
                parts[2],
                parts[3]
            )
        } else {
            format!(
                "{}/{}/{}/{}-{}-{}.jar",
                parts[0].replace('.', "/"),
                parts[1],
                parts[2],
                parts[1],
                parts[2],
                classifier
            )
        };
        libraries_dir().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
    };

    if !jar_path.exists() {
        return Ok(());
    }

    let excludes = lib
        .extract
        .as_ref()
        .and_then(|e| e.exclude.as_ref())
        .cloned()
        .unwrap_or_else(|| vec!["META-INF/".into()]);

    let file = File::open(&jar_path)?;
    let mut archive = ZipArchive::new(file).map_err(|e| LauncherError::Other(e.to_string()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| LauncherError::Other(e.to_string()))?;
        let name = entry.name().to_string();
        if excludes.iter().any(|ex| name.starts_with(ex)) {
            continue;
        }
        if name.ends_with('/') {
            continue;
        }
        // Only extract native binaries
        let lower = name.to_lowercase();
        if !(lower.ends_with(".dll")
            || lower.ends_with(".so")
            || lower.ends_with(".dylib")
            || lower.ends_with(".jnilib"))
        {
            // Still extract all non-excluded for compatibility
        }

        let out_path = natives_out.join(Path::new(&name).file_name().unwrap_or_default());
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut outfile = File::create(&out_path)?;
        copy(&mut entry, &mut outfile)?;
    }

    Ok(())
}

/// Путь к jar библиотеки для classpath (без natives classifiers).
pub fn library_classpath_path(lib: &Library) -> Option<PathBuf> {
    if !library_applies(lib) {
        return None;
    }
    // Natives-only libraries (no artifact) shouldn't be on classpath sometimes
    if let Some(downloads) = &lib.downloads {
        if let Some(artifact) = &downloads.artifact {
            let p = artifact_path(lib, artifact.path.as_deref()).ok()?;
            if p.exists() {
                return Some(p);
            }
        }
        // If only natives, skip classpath
        return None;
    }
    maven_path_from_name(&lib.name).ok().filter(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AssetObject;
    use sha1::{Digest, Sha1};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::thread::JoinHandle;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "mine-launcher-assets-test-{}-{timestamp}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone)]
    struct Route {
        body: Vec<u8>,
        failures_before_success: usize,
        corrupt_before_success: usize,
        delay: Duration,
    }

    impl Route {
        fn ok(body: Vec<u8>, delay: Duration) -> Self {
            Self {
                body,
                failures_before_success: 0,
                corrupt_before_success: 0,
                delay,
            }
        }
    }

    struct TestServer {
        base_url: String,
        join: JoinHandle<()>,
        max_active: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
    }

    impl TestServer {
        fn finish(self) -> (usize, usize) {
            self.join.join().expect("test server thread");
            (
                self.max_active.load(Ordering::Relaxed),
                self.requests.load(Ordering::Relaxed),
            )
        }
    }

    fn spawn_test_server(routes: HashMap<String, Route>, expected_requests: usize) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set test server nonblocking");
        let address = listener.local_addr().expect("test server address");
        let routes = Arc::new(routes);
        let route_requests = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));

        let server_active = active.clone();
        let server_max_active = max_active.clone();
        let server_requests = requests.clone();
        let join = std::thread::spawn(move || {
            let started = Instant::now();
            let mut accepted = 0usize;
            let mut handlers = Vec::new();

            while accepted < expected_requests && started.elapsed() < Duration::from_secs(10) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted socket blocking");
                        let routes = routes.clone();
                        let route_requests = route_requests.clone();
                        let active = server_active.clone();
                        let max_active = server_max_active.clone();
                        let requests = server_requests.clone();
                        handlers.push(std::thread::spawn(move || {
                            serve_test_request(
                                stream,
                                &routes,
                                &route_requests,
                                &active,
                                &max_active,
                                &requests,
                            );
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept test request: {error}"),
                }
            }

            for handler in handlers {
                handler.join().expect("test request handler");
            }
        });

        TestServer {
            base_url: format!("http://{address}"),
            join,
            max_active,
            requests,
        }
    }

    fn serve_test_request(
        mut stream: TcpStream,
        routes: &HashMap<String, Route>,
        route_requests: &Mutex<HashMap<String, usize>>,
        active: &AtomicUsize,
        max_active: &AtomicUsize,
        requests: &AtomicUsize,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set request read timeout");
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).expect("read test request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }

        let request = String::from_utf8_lossy(&request);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request path")
            .to_string();
        let route = routes.get(&path).expect("known test route");
        let request_number = {
            let mut counts = route_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let count = counts.entry(path).or_default();
            *count += 1;
            *count
        };

        requests.fetch_add(1, Ordering::Relaxed);
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        update_max(max_active, current);
        std::thread::sleep(route.delay);

        let (status, body): (&str, &[u8]) = if request_number <= route.failures_before_success {
            ("500 Internal Server Error", b"retry")
        } else if request_number <= route.failures_before_success + route.corrupt_before_success {
            ("200 OK", b"corrupt")
        } else {
            ("200 OK", &route.body)
        };
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(headers.as_bytes())
            .expect("write test response headers");
        stream.write_all(body).expect("write test response body");
        active.fetch_sub(1, Ordering::SeqCst);
    }

    fn update_max(maximum: &AtomicUsize, candidate: usize) {
        let mut current = maximum.load(Ordering::Relaxed);
        while candidate > current {
            match maximum.compare_exchange_weak(
                current,
                candidate,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn hash_bytes(bytes: &[u8]) -> String {
        hex::encode(Sha1::digest(bytes))
    }

    fn object_route(hash: &str) -> String {
        format!("/{}/{}", &hash[..2], hash)
    }

    fn no_progress() -> ProgressFn {
        Arc::new(|_, _, _| {})
    }

    #[test]
    fn plans_unique_missing_and_wrong_size_assets() {
        let root = TestDir::new();
        let cached = b"cached asset".to_vec();
        let missing = b"missing asset".to_vec();
        let replacement = b"replacement asset".to_vec();
        let cached_hash = hash_bytes(&cached);
        let missing_hash = hash_bytes(&missing);
        let replacement_hash = hash_bytes(&replacement);

        let cached_path = asset_object_path(root.path(), &cached_hash).unwrap();
        fs::create_dir_all(cached_path.parent().unwrap()).unwrap();
        fs::write(&cached_path, &cached).unwrap();
        let replacement_path = asset_object_path(root.path(), &replacement_hash).unwrap();
        fs::create_dir_all(replacement_path.parent().unwrap()).unwrap();
        fs::write(&replacement_path, b"x").unwrap();

        let mut objects = HashMap::new();
        objects.insert(
            "cached".into(),
            AssetObject {
                hash: cached_hash,
                size: cached.len() as u64,
            },
        );
        let missing_object = AssetObject {
            hash: missing_hash.clone(),
            size: missing.len() as u64,
        };
        objects.insert("missing-a".into(), missing_object.clone());
        objects.insert("missing-b".into(), missing_object);
        objects.insert(
            "wrong-size".into(),
            AssetObject {
                hash: replacement_hash.clone(),
                size: replacement.len() as u64,
            },
        );

        let jobs =
            build_asset_downloads(&AssetIndex { objects }, root.path(), "https://assets.test")
                .unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            jobs.iter().filter(|job| job.hash == missing_hash).count(),
            1
        );
        assert!(jobs.iter().any(|job| job.hash == replacement_hash));
    }

    #[test]
    fn downloads_assets_concurrently_with_monotonic_progress() {
        let root = TestDir::new();
        let mut objects = HashMap::new();
        let mut routes = HashMap::new();

        for number in 0..8 {
            let body = format!("asset body {number}").repeat(1024).into_bytes();
            let hash = hash_bytes(&body);
            routes.insert(
                object_route(&hash),
                Route::ok(body.clone(), Duration::from_millis(125)),
            );
            objects.insert(
                format!("asset-{number}"),
                AssetObject {
                    hash,
                    size: body.len() as u64,
                },
            );
        }

        let server = spawn_test_server(routes, objects.len());
        let events = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let progress: ProgressFn = Arc::new({
            let events = events.clone();
            move |done, total, _| {
                events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((done, total));
            }
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let index = AssetIndex { objects };

        download_assets(
            &download::http_client().unwrap(),
            &index,
            root.path(),
            &server.base_url,
            4,
            &[],
            &progress,
            &cancel,
        )
        .unwrap();
        let (max_active, requests) = server.finish();

        assert_eq!(requests, 8);
        assert!((2..=4).contains(&max_active), "max active: {max_active}");
        for object in index.objects.values() {
            let path = asset_object_path(root.path(), &object.hash).unwrap();
            assert!(download::verify_sha1(&path, &object.hash).unwrap());
        }

        let events = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(events.len() >= 2);
        assert!(events.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        let &(done, total) = events.last().unwrap();
        assert_eq!(done, total);
    }

    #[test]
    fn retries_http_and_checksum_errors_then_replaces_stale_file() {
        let root = TestDir::new();
        let body = b"eventual valid asset".to_vec();
        let hash = hash_bytes(&body);
        let mut routes = HashMap::new();
        routes.insert(
            object_route(&hash),
            Route {
                body: body.clone(),
                failures_before_success: 1,
                corrupt_before_success: 1,
                delay: Duration::ZERO,
            },
        );
        let server = spawn_test_server(routes, 3);
        let mut objects = HashMap::new();
        objects.insert(
            "retry".into(),
            AssetObject {
                hash: hash.clone(),
                size: body.len() as u64,
            },
        );
        let index = AssetIndex { objects };
        let dest = asset_object_path(root.path(), &hash).unwrap();
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"stale").unwrap();

        download_assets(
            &download::http_client().unwrap(),
            &index,
            root.path(),
            &server.base_url,
            1,
            &[Duration::from_millis(1), Duration::from_millis(1)],
            &no_progress(),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let (_, requests) = server.finish();

        assert_eq!(requests, 3);
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!dest.with_extension("part").exists());
    }

    #[test]
    fn stops_dispatching_after_retries_are_exhausted() {
        let root = TestDir::new();
        let failed_body = b"largest asset always fails".repeat(4);
        let failed_hash = hash_bytes(&failed_body);
        let waiting_body = b"waiting asset".to_vec();
        let waiting_hash = hash_bytes(&waiting_body);
        let mut routes = HashMap::new();
        routes.insert(
            object_route(&failed_hash),
            Route {
                body: failed_body.clone(),
                failures_before_success: usize::MAX,
                corrupt_before_success: 0,
                delay: Duration::ZERO,
            },
        );
        routes.insert(
            object_route(&waiting_hash),
            Route::ok(waiting_body.clone(), Duration::ZERO),
        );
        let server = spawn_test_server(routes, 3);
        let mut objects = HashMap::new();
        objects.insert(
            "failed".into(),
            AssetObject {
                hash: failed_hash,
                size: failed_body.len() as u64,
            },
        );
        objects.insert(
            "waiting".into(),
            AssetObject {
                hash: waiting_hash.clone(),
                size: waiting_body.len() as u64,
            },
        );

        let result = download_assets(
            &download::http_client().unwrap(),
            &AssetIndex { objects },
            root.path(),
            &server.base_url,
            1,
            &[Duration::from_millis(1), Duration::from_millis(1)],
            &no_progress(),
            &Arc::new(AtomicBool::new(false)),
        );
        let (_, requests) = server.finish();

        assert!(matches!(result, Err(LauncherError::Network(_))));
        assert_eq!(requests, 3);
        assert!(!asset_object_path(root.path(), &waiting_hash)
            .unwrap()
            .exists());
    }

    #[test]
    fn cancellation_stops_before_dispatch() {
        let root = TestDir::new();
        let body = b"cancelled asset";
        let hash = hash_bytes(body);
        let mut objects = HashMap::new();
        objects.insert(
            "cancelled".into(),
            AssetObject {
                hash,
                size: body.len() as u64,
            },
        );
        let cancel = Arc::new(AtomicBool::new(true));

        let error = download_assets(
            &download::http_client().unwrap(),
            &AssetIndex { objects },
            root.path(),
            "http://127.0.0.1:1",
            4,
            &[],
            &no_progress(),
            &cancel,
        )
        .unwrap_err();

        assert!(matches!(error, LauncherError::Other(message) if message == "Отменено"));
    }

    #[test]
    fn incomplete_asset_cache_is_not_an_installed_version() {
        let root = TestDir::new();
        let versions_root = root.path().join("versions");
        let assets_root = root.path().join("assets");
        let version_id = "test-version";
        let version_dir = versions_root.join(version_id);
        fs::create_dir_all(&version_dir).unwrap();
        fs::create_dir_all(assets_root.join("indexes")).unwrap();
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            r#"{
                "id": "test-version",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {"id": "test-assets", "url": "https://assets.test/index.json"}
            }"#,
        )
        .unwrap();
        fs::write(version_dir.join(format!("{version_id}.jar")), b"jar").unwrap();

        let body = b"required asset";
        let hash = hash_bytes(body);
        fs::write(
            assets_root.join("indexes").join("test-assets.json"),
            format!(
                r#"{{"objects":{{"required":{{"hash":"{hash}","size":{}}}}}}}"#,
                body.len()
            ),
        )
        .unwrap();

        assert!(!is_version_installed_at(
            version_id,
            &versions_root,
            &assets_root
        ));
        let asset_path = asset_object_path(&assets_root, &hash).unwrap();
        fs::create_dir_all(asset_path.parent().unwrap()).unwrap();
        fs::write(&asset_path, body).unwrap();
        assert!(is_version_installed_at(
            version_id,
            &versions_root,
            &assets_root
        ));
        fs::write(&asset_path, b"wrong size").unwrap();
        assert!(!is_version_installed_at(
            version_id,
            &versions_root,
            &assets_root
        ));
    }
}
