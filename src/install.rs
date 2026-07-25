use std::fs::{self, File};
use std::io::copy;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use zip::ZipArchive;

use crate::download::{self, download_file, download_json, ProgressFn};
use crate::error::LauncherError;
use crate::models::{AssetIndex, Library, VersionEntry, VersionInfo, VersionJson, VersionManifest};
use crate::paths::{
    assets_dir, ensure_dirs, game_dir, libraries_dir, natives_dir, versions_dir,
};
use crate::rules::{library_applies, native_classifier};

const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const RESOURCES_URL: &str = "https://resources.download.minecraft.net";

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
        let jar = path.join(format!("{name}.jar"));
        let json = path.join(format!("{name}.json"));
        if jar.exists() && json.exists() {
            ids.push(name);
        }
    }
    ids.sort();
    ids.reverse();
    ids
}

pub fn is_version_installed(version_id: &str) -> bool {
    installed_versions().iter().any(|v| v == version_id)
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
    let path = versions_dir()
        .join(version_id)
        .join(format!("{version_id}.json"));
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|e| LauncherError::Parse(e.to_string()))
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

    // Client jar
    progress(0, 1, "Скачивание клиента…");
    let client_jar = version_dir.join(format!("{version_id}.jar"));
    download_file(
        &client,
        &version.downloads.client.url,
        &client_jar,
        version.downloads.client.sha1.as_deref(),
        Some(&progress),
        &format!("{version_id}.jar"),
    )?;

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
    progress(0, 1, "Индекс ассетов…");
    let index_path = assets_dir()
        .join("indexes")
        .join(format!("{}.json", version.asset_index.id));
    download_file(
        &client,
        &version.asset_index.url,
        &index_path,
        version.asset_index.sha1.as_deref(),
        Some(&progress),
        "asset index",
    )?;

    let index: AssetIndex = {
        let data = fs::read_to_string(&index_path)?;
        serde_json::from_str(&data).map_err(|e| LauncherError::Parse(e.to_string()))?
    };

    let objects: Vec<_> = index.objects.into_iter().collect();
    let total_assets = objects.len() as u64;
    for (i, (name, obj)) in objects.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(LauncherError::Other("Отменено".into()));
        }
        if i % 25 == 0 {
            progress(
                i as u64,
                total_assets.max(1),
                &format!("Ассеты: {name}"),
            );
        }
        let prefix = &obj.hash[..2];
        let dest = assets_dir().join("objects").join(prefix).join(&obj.hash);
        if dest.exists() {
            continue;
        }
        let url = format!("{RESOURCES_URL}/{prefix}/{}", obj.hash);
        download_file(
            &client,
            &url,
            &dest,
            Some(&obj.hash),
            None,
            name,
        )?;
    }

    // legacy virtual assets for very old versions use assets name "legacy" — skip virtual for now
    // modern game uses objects/ hash layout.

    progress(1, 1, &format!("Версия {version_id} готова"));
    let _ = game_dir(); // ensure path used
    Ok(())
}

fn install_library(
    client: &reqwest::blocking::Client,
    lib: &Library,
    progress: &ProgressFn,
) -> Result<(), LauncherError> {
    if let Some(downloads) = &lib.downloads {
        if let Some(artifact) = &downloads.artifact {
            let path = artifact_path(lib, artifact.path.as_deref())?;
            download_file(
                client,
                &artifact.url,
                &path,
                artifact.sha1.as_deref(),
                Some(progress),
                &lib.name,
            )?;
        }
        if let Some(classifier) = native_classifier(lib) {
            if let Some(classifiers) = &downloads.classifiers {
                if let Some(art) = classifiers.get(&classifier) {
                    let path = artifact_path(lib, art.path.as_deref())?;
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
    } else {
        // Legacy: construct maven path from name
        let rel = maven_relative(&lib.name)?;
        let url = format!("https://libraries.minecraft.net/{rel}");
        let dest = libraries_dir().join(PathBuf::from(rel.replace('/', std::path::MAIN_SEPARATOR_STR)));
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
        return Err(LauncherError::Parse(format!("Некорректная библиотека: {name}")));
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

    let jar_path = if let Some(downloads) = &lib.downloads {
        if let Some(classifiers) = &downloads.classifiers {
            if let Some(art) = classifiers.get(&classifier) {
                artifact_path(lib, art.path.as_deref())?
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    } else {
        // name with classifier
        let parts: Vec<&str> = lib.name.split(':').collect();
        if parts.len() < 3 {
            return Ok(());
        }
        let rel = format!(
            "{}/{}/{}/{}-{}-{}.jar",
            parts[0].replace('.', "/"),
            parts[1],
            parts[2],
            parts[1],
            parts[2],
            classifier
        );
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
