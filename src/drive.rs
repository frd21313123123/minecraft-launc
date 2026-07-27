//! Публичная папка Google Drive со сборками.
//!
//! Ожидаемый формат (когда появятся файлы):
//! - `builds.json` — каталог сборок (предпочтительно)
//! - либо отдельные `.zip` — каждая сборка = один архив
//!
//! Папка должна быть доступна «всем, у кого есть ссылка».

use std::fs::{self, File};
use std::io::{copy, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::download::{self, ProgressFn};
use crate::error::LauncherError;
use crate::paths::{
    builds_dir, ensure_dirs, instance_dir as paths_instance_dir, instance_game_dir,
    sanitize_build_id,
};

/// Папка со сборками на Google Drive.
pub const DRIVE_FOLDER_ID: &str = "1mEl5hfZqx5IUiS_gULZBz4v116YuHYtq";
const INSTALL_STATE_FILE: &str = ".minelauncher-state.json";
const INSTALL_STATE_SCHEMA: u32 = 1;

#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// Стабильный id (slug / имя файла без расширения).
    pub id: String,
    /// Отображаемое имя.
    pub name: String,
    /// ID файла на Drive.
    pub file_id: String,
    pub filename: String,
    pub size: Option<u64>,
    /// Время последнего изменения файла на Drive (Unix time в миллисекундах).
    pub modified_time_ms: Option<u64>,
    /// Базовая версия Minecraft из builds.json (если указана).
    pub minecraft: Option<String>,
}

impl BuildInfo {
    /// Стабильный идентификатор конкретной опубликованной ревизии архива.
    pub fn revision_id(&self) -> Result<String, LauncherError> {
        let modified_time_ms = self.modified_time_ms.ok_or_else(|| {
            LauncherError::Other(format!(
                "Google Drive не отдал время изменения сборки «{}». \
                 Без него нельзя безопасно проверить обновление.",
                self.name
            ))
        })?;
        let size = self.size.filter(|size| *size > 0).ok_or_else(|| {
            LauncherError::Other(format!(
                "Google Drive не отдал размер сборки «{}». \
                 Без него нельзя безопасно проверить обновление.",
                self.name
            ))
        })?;
        Ok(format!("{}:{modified_time_ms}:{size}", self.file_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildUpdateStatus {
    NotInstalled,
    Current,
    UpdateRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct InstalledBuildState {
    schema_version: u32,
    build_id: String,
    drive_file_id: String,
    drive_modified_time_ms: u64,
    archive_size: u64,
}

impl InstalledBuildState {
    fn from_build(build: &BuildInfo) -> Result<Self, LauncherError> {
        // Проверяем весь набор обязательных полей в одном месте.
        let _ = build.revision_id()?;
        Ok(Self {
            schema_version: INSTALL_STATE_SCHEMA,
            build_id: sanitize_build_id(&build.id),
            drive_file_id: build.file_id.clone(),
            drive_modified_time_ms: build.modified_time_ms.unwrap_or_default(),
            archive_size: build.size.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct BuildsManifest {
    builds: Vec<ManifestBuild>,
}

#[derive(Debug, Deserialize)]
struct ManifestBuild {
    id: Option<String>,
    name: String,
    #[serde(default)]
    file_id: String,
    #[serde(default)]
    filename: String,
    size: Option<u64>,
    /// Базовая версия Minecraft (если сборка — модпак поверх ванили).
    #[serde(default)]
    minecraft: Option<String>,
}

/// Метаданные внутри zip-сборки (`build.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct BuildMeta {
    #[allow(dead_code)]
    pub name: Option<String>,
    /// Версия Minecraft, которую нужно поставить (например `1.20.1`).
    pub minecraft: Option<String>,
    /// Кастомный id версии, если в архиве полный client package.
    pub version_id: Option<String>,
}

#[derive(Debug, Clone)]
struct DriveFile {
    id: String,
    name: String,
    mime: String,
    size: Option<u64>,
    modified_time_ms: Option<u64>,
}

pub fn folder_url() -> String {
    format!("https://drive.google.com/drive/folders/{DRIVE_FOLDER_ID}?usp=sharing")
}

pub fn direct_download_url(file_id: &str) -> String {
    format!("https://drive.google.com/uc?export=download&id={file_id}")
}

/// Список сборок из публичной папки Drive.
pub fn fetch_builds() -> Result<Vec<BuildInfo>, LauncherError> {
    let client = download::http_client()?;
    let files = list_folder_files(&client, DRIVE_FOLDER_ID)?;

    // Если есть builds.json — используем его как каталог.
    if let Some(manifest_file) = files
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("builds.json"))
    {
        match fetch_builds_manifest(&client, &manifest_file.id) {
            Ok(mut builds) => {
                // Подтянуть file_id и актуальные метаданные из самой папки Drive.
                enrich_manifest_builds(&mut builds, &files);
                builds.retain(|b| !b.file_id.is_empty());
                builds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                return Ok(builds);
            }
            Err(e) => {
                // Падаем на список zip, но не молчим о проблеме манифеста.
                eprintln!("builds.json: {e}");
            }
        }
    }

    let mut builds: Vec<BuildInfo> = files
        .into_iter()
        .filter(|f| {
            let lower = f.name.to_lowercase();
            lower.ends_with(".zip")
                && !f.mime.contains("folder")
                && !lower.eq("builds.json")
        })
        .map(|f| {
            let id = stem_id(&f.name);
            let name = pretty_name(&id);
            BuildInfo {
                id,
                name,
                file_id: f.id,
                filename: f.name,
                size: f.size,
                modified_time_ms: f.modified_time_ms,
                minecraft: None,
            }
        })
        .collect();

    builds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(builds)
}

fn enrich_manifest_builds(builds: &mut [BuildInfo], files: &[DriveFile]) {
    for build in builds {
        let file = if build.file_id.is_empty() {
            files.iter().find(|file| file.name == build.filename)
        } else {
            files.iter().find(|file| file.id == build.file_id)
        };
        if let Some(file) = file {
            if build.file_id.is_empty() {
                build.file_id = file.id.clone();
            }
            build.size = file.size.or(build.size);
            build.modified_time_ms = file.modified_time_ms;
        }
    }
}

fn fetch_builds_manifest(
    client: &reqwest::blocking::Client,
    file_id: &str,
) -> Result<Vec<BuildInfo>, LauncherError> {
    let bytes = download_drive_bytes(client, file_id, None, "builds.json")?;
    let text = String::from_utf8(bytes)
        .map_err(|e| LauncherError::Parse(format!("builds.json UTF-8: {e}")))?;
    let man: BuildsManifest = serde_json::from_str(&text)
        .map_err(|e| LauncherError::Parse(format!("builds.json: {e}")))?;

    Ok(man
        .builds
        .into_iter()
        .map(|b| {
            let filename = if b.filename.is_empty() {
                format!("{}.zip", slug(&b.name))
            } else {
                b.filename
            };
            let id = sanitize_build_id(
                &b.id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| stem_id(&filename)),
            );
            BuildInfo {
                id,
                name: b.name,
                file_id: b.file_id,
                filename,
                size: b.size,
                modified_time_ms: None,
                minecraft: b.minecraft,
            }
        })
        .collect())
}

/// Скачивает и распаковывает сборку в `instances/{id}/`.
/// Каждая сборка — отдельная папка; user-data (saves/options) сохраняется при переустановке.
/// Возвращает путь к инстансу и опциональные метаданные.
pub fn install_build(
    build: &BuildInfo,
    progress: ProgressFn,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(PathBuf, Option<BuildMeta>), LauncherError> {
    ensure_dirs()?;
    let install_state = InstalledBuildState::from_build(build)?;
    let client = download::http_client()?;
    let build_id = sanitize_build_id(&build.id);
    let operation_id = operation_id();
    let cache_zip = builds_dir().join(&build.filename);
    let candidate_zip = builds_dir().join(format!(".{build_id}-{operation_id}.download.zip"));
    let dest = paths_instance_dir(&build_id);
    let instances_parent = dest
        .parent()
        .ok_or_else(|| LauncherError::Other("Некорректный путь инстанса".into()))?;
    let staging = instances_parent.join(format!(".{build_id}-update-{operation_id}"));
    let backup = instances_parent.join(format!(".{build_id}-backup-{operation_id}"));
    let label = format!("Скачивание «{}»", build.name);
    progress(0, build.size.unwrap_or(0), &label);

    let result = (|| {
        let _ = fs::remove_file(&candidate_zip);
        let _ = fs::remove_file(candidate_zip.with_extension("part"));
        download_and_validate_build_archive(&client, build, &candidate_zip, &progress, cancel)?;

        let installed = install_archive_atomically(
            build,
            install_state,
            &candidate_zip,
            &dest,
            &staging,
            &backup,
            &progress,
            cancel,
        )?;

        // Кэш не участвует в принятии решения об обновлении. Сохраняем в нём
        // только уже проверенный архив новой ревизии.
        if cache_zip != candidate_zip {
            if cache_zip.exists() {
                let _ = fs::remove_file(&cache_zip);
            }
            if let Err(error) = fs::rename(&candidate_zip, &cache_zip) {
                eprintln!("Не удалось обновить кэш сборки: {error}");
            }
        }

        Ok(installed)
    })();

    let _ = fs::remove_file(&candidate_zip);
    let _ = fs::remove_file(candidate_zip.with_extension("part"));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
        if backup.exists() && !dest.exists() {
            let _ = fs::rename(&backup, &dest);
        }
    }
    result
}

fn download_and_validate_build_archive(
    client: &reqwest::blocking::Client,
    build: &BuildInfo,
    destination: &Path,
    progress: &ProgressFn,
    cancel: &AtomicBool,
) -> Result<(), LauncherError> {
    let label = format!("Скачивание обновления «{}»", build.name);
    download_drive_file_with_cancel(
        client,
        &build.file_id,
        destination,
        Some(progress),
        &label,
        build.size,
        Some(cancel),
    )?;

    if let Err(first_error) = validate_zip_archive(destination) {
        let _ = fs::remove_file(destination);
        let _ = fs::remove_file(destination.with_extension("part"));
        let retry_label = format!("Повторное скачивание «{}»", build.name);
        progress(0, build.size.unwrap_or(0), &retry_label);
        download_drive_file_with_cancel(
            client,
            &build.file_id,
            destination,
            Some(progress),
            &retry_label,
            build.size,
            Some(cancel),
        )?;
        validate_zip_archive(destination).map_err(|retry_error| {
            LauncherError::Other(format!(
                "Архив сборки повреждён даже после повторной загрузки \
                 ({first_error}; повторно: {retry_error}). Проверьте соединение и свободное место."
            ))
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn install_archive_atomically(
    build: &BuildInfo,
    install_state: InstalledBuildState,
    archive: &Path,
    destination: &Path,
    staging: &Path,
    backup: &Path,
    progress: &ProgressFn,
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<BuildMeta>), LauncherError> {
    validate_zip_archive(archive)?;
    let was_update = destination.exists();
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled_error());
    }

    let _ = fs::remove_dir_all(staging);
    let _ = fs::remove_dir_all(backup);
    fs::create_dir_all(staging)?;
    progress(0, 0, "Распаковка обновления…");

    let prepared = (|| {
        extract_zip(archive, staging)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_error());
        }

        let root = resolve_instance_root(staging);
        let game = ensure_isolated_game_dir(&root)?;
        copy_user_data_from_instance(destination, &game)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_error());
        }

        write_install_state(staging, &install_state)?;
        let meta = read_build_meta(staging);
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_error());
        }
        replace_instance_atomically(destination, staging, backup)?;
        Ok(meta)
    })();

    match prepared {
        Ok(meta) => {
            let action = if was_update {
                "обновлена"
            } else {
                "установлена"
            };
            progress(1, 1, &format!("Сборка «{}» {action}", build.name));
            Ok((destination.to_path_buf(), meta))
        }
        Err(error) => {
            let _ = fs::remove_dir_all(staging);
            Err(error)
        }
    }
}

fn replace_instance_atomically(
    destination: &Path,
    staging: &Path,
    backup: &Path,
) -> Result<(), LauncherError> {
    let had_existing = destination.exists();
    if had_existing {
        fs::rename(destination, backup)?;
    }

    if let Err(error) = fs::rename(staging, destination) {
        if had_existing {
            if let Err(rollback_error) = fs::rename(backup, destination) {
                return Err(LauncherError::Other(format!(
                    "Не удалось применить обновление ({error}) и восстановить старую сборку \
                     ({rollback_error}). Резервная копия: {}",
                    backup.display()
                )));
            }
        }
        return Err(LauncherError::Other(format!(
            "Не удалось применить обновление сборки: {error}"
        )));
    }

    if had_existing {
        if let Err(error) = fs::remove_dir_all(backup) {
            eprintln!(
                "Обновление установлено, но не удалось удалить резервную копию {}: {error}",
                backup.display()
            );
        }
    }
    Ok(())
}

fn operation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

pub fn instance_dir(build_id: &str) -> PathBuf {
    paths_instance_dir(build_id)
}

/// Каталог `--gameDir` для сборки (mods / saves / config).
/// Всегда внутри `instances/{id}/` — сборки не делят миры и моды.
pub fn build_game_dir(build_id: &str) -> PathBuf {
    let dir = instance_dir(build_id);
    if !dir.is_dir() {
        return instance_game_dir(build_id);
    }
    let root = resolve_instance_root(&dir);
    resolve_game_dir_for_root(&root)
}

fn resolve_game_dir_for_root(root: &Path) -> PathBuf {
    let mc = root.join("minecraft");
    if mc.is_dir() {
        return mc;
    }
    if root.join(".minecraft").is_dir() {
        return root.join(".minecraft");
    }
    // Zip положил mods/config прямо в корень инстанса.
    if root.join("mods").is_dir()
        || root.join("config").is_dir()
        || root.join("saves").is_dir()
        || root.join("options.txt").is_file()
    {
        return root.to_path_buf();
    }
    // Ещё не распаковано / пусто — канонический путь.
    mc
}

/// Создаёт `minecraft/` и при необходимости переносит туда mods/config из корня,
/// чтобы у каждой сборки был свой изолированный game dir.
fn ensure_isolated_game_dir(root: &Path) -> Result<PathBuf, LauncherError> {
    let game = resolve_game_dir_for_root(root);
    if game == root {
        // Контент лежит в корне — оставляем как есть (корень = game dir).
        fs::create_dir_all(root)?;
        return Ok(root.to_path_buf());
    }
    fs::create_dir_all(&game)?;
    // Если в корне остались типичные папки модпака — переносим в minecraft/.
    for name in [
        "mods",
        "config",
        "resourcepacks",
        "shaderpacks",
        "saves",
        "defaultconfigs",
        "options.txt",
        "optionsof.txt",
        "servers.dat",
    ] {
        let src = root.join(name);
        let dst = game.join(name);
        if src.exists() && src != dst && !dst.exists() {
            fs::rename(&src, &dst).or_else(|_| {
                if src.is_dir() {
                    copy_dir_all(&src, &dst)?;
                    let _ = fs::remove_dir_all(&src);
                } else {
                    if let Some(p) = dst.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::copy(&src, &dst)?;
                    let _ = fs::remove_file(&src);
                }
                Ok::<(), LauncherError>(())
            })?;
        }
    }
    Ok(game)
}

/// Что сохраняем при переустановке сборки (не трогаем моды/конфиги пака).
const USER_DATA_ENTRIES: &[&str] = &[
    "saves",
    "screenshots",
    "logs",
    "crash-reports",
    "options.txt",
    "optionsof.txt",
    "optionsshaders.txt",
    "servers.dat",
    "servers.dat_old",
    "usercache.json",
    "usernamecache.json",
    "command_history.txt",
    "hotbar.nbt",
    "realms_persistence.json",
];

fn copy_user_data_from_instance(instance: &Path, new_game: &Path) -> Result<(), LauncherError> {
    if !instance.is_dir() {
        return Ok(());
    }
    let root = resolve_instance_root(instance);
    let old_game = resolve_game_dir_for_root(&root);
    if !old_game.is_dir() {
        return Ok(());
    }

    for name in USER_DATA_ENTRIES {
        let src = old_game.join(name);
        if !src.exists() {
            continue;
        }
        let dst = new_game.join(name);
        // Миры и пользовательские настройки важнее значений по умолчанию
        // из нового архива. Остальные файлы сохраняем, только если пак их не принёс.
        let prefer_user = *name == "saves"
            || *name == "screenshots"
            || name.starts_with("options")
            || name.starts_with("servers");
        if dst.exists() && !prefer_user {
            continue;
        }
        if dst.exists() {
            if dst.is_dir() {
                fs::remove_dir_all(&dst)?;
            } else {
                fs::remove_file(&dst)?;
            }
        }
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), LauncherError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            if let Some(p) = to.parent() {
                fs::create_dir_all(p)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

pub fn is_build_installed(build_id: &str) -> bool {
    is_build_installed_at(&instance_dir(build_id))
}

fn is_build_installed_at(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let root = resolve_instance_root(dir);
    // Считаем установленной только если есть признаки реальной сборки.
    root.join("mmc-pack.json").is_file()
        || root.join("mods").is_dir()
        || root.join("minecraft").join("mods").is_dir()
        || root.join("build.json").is_file()
        || root.join("pack.json").is_file()
}

pub fn build_update_status(build: &BuildInfo) -> Result<BuildUpdateStatus, LauncherError> {
    build_update_status_at(build, &instance_dir(&build.id))
}

fn build_update_status_at(
    build: &BuildInfo,
    instance: &Path,
) -> Result<BuildUpdateStatus, LauncherError> {
    let expected = InstalledBuildState::from_build(build)?;
    if !is_build_installed_at(instance) {
        return Ok(BuildUpdateStatus::NotInstalled);
    }

    let state = read_install_state(instance);
    if state.as_ref() == Some(&expected) {
        Ok(BuildUpdateStatus::Current)
    } else {
        // Старые установки и повреждённые state-файлы обновляются один раз.
        Ok(BuildUpdateStatus::UpdateRequired)
    }
}

fn install_state_path(instance: &Path) -> PathBuf {
    instance.join(INSTALL_STATE_FILE)
}

fn read_install_state(instance: &Path) -> Option<InstalledBuildState> {
    let data = fs::read(install_state_path(instance)).ok()?;
    serde_json::from_slice(&data).ok()
}

fn write_install_state(instance: &Path, state: &InstalledBuildState) -> Result<(), LauncherError> {
    fs::create_dir_all(instance)?;
    let destination = install_state_path(instance);
    let temporary = instance.join(format!("{INSTALL_STATE_FILE}.part"));
    let data = serde_json::to_vec_pretty(state)
        .map_err(|error| LauncherError::Parse(format!("state сборки: {error}")))?;
    let mut file = File::create(&temporary)?;
    file.write_all(&data)?;
    file.flush()?;
    drop(file);
    if destination.exists() {
        fs::remove_file(&destination)?;
    }
    fs::rename(&temporary, &destination)?;
    Ok(())
}

pub fn read_build_meta(instance: &Path) -> Option<BuildMeta> {
    for name in ["build.json", "pack.json", "modpack.json"] {
        let p = instance.join(name);
        if p.exists() {
            if let Ok(data) = fs::read_to_string(&p) {
                if let Ok(m) = serde_json::from_str(&data) {
                    return Some(m);
                }
            }
        }
    }
    // Ищем build.json на уровень глубже (если zip с корневой папкой).
    if let Ok(entries) = fs::read_dir(instance) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(m) = read_build_meta(&p) {
                    return Some(m);
                }
            }
        }
    }
    None
}

/// Корень содержимого инстанса (с учётом одной вложенной папки в zip).
pub fn resolve_instance_root(instance: &Path) -> PathBuf {
    if instance.join("mmc-pack.json").is_file()
        || instance.join("mods").is_dir()
        || instance.join("config").is_dir()
        || instance.join("versions").is_dir()
        || instance.join("build.json").is_file()
        || instance.join("minecraft").is_dir()
    {
        return instance.to_path_buf();
    }
    if let Ok(entries) = fs::read_dir(instance) {
        let dirs: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        if dirs.len() == 1 {
            let inner = &dirs[0];
            if inner.join("mmc-pack.json").is_file()
                || inner.join("mods").is_dir()
                || inner.join("config").is_dir()
                || inner.join("versions").is_dir()
                || inner.join("build.json").is_file()
                || inner.join("minecraft").is_dir()
            {
                return inner.clone();
            }
        }
    }
    instance.to_path_buf()
}

fn list_folder_files(
    client: &reqwest::blocking::Client,
    folder_id: &str,
) -> Result<Vec<DriveFile>, LauncherError> {
    // 1) HTML публичной страницы папки (работает без API-ключа).
    let url = format!("https://drive.google.com/drive/folders/{folder_id}?usp=sharing");
    let html = client
        .get(&url)
        .header(reqwest::header::CACHE_CONTROL, "no-cache, no-store")
        .header(reqwest::header::PRAGMA, "no-cache")
        .header(
            reqwest::header::ACCEPT_LANGUAGE,
            "en-US,en;q=0.9,ru;q=0.8",
        )
        .send()
        .map_err(|e| LauncherError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| LauncherError::Network(e.to_string()))?
        .text()
        .map_err(|e| LauncherError::Network(e.to_string()))?;

    let mut files = parse_drive_html(&html);

    // 2) embeddedfolderview — запасной вариант.
    if files.is_empty() {
        let emb = format!("https://drive.google.com/embeddedfolderview?id={folder_id}");
        if let Ok(resp) = client.get(&emb).send() {
            if let Ok(text) = resp.text() {
                files = parse_embedded_folder(&text);
            }
        }
    }

    // Дедуп по id
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| seen.insert(f.id.clone()));
    Ok(files)
}

fn parse_drive_html(html: &str) -> Vec<DriveFile> {
    let mut files = Vec::new();

    // 1) Современный payload: window['_DRIVE_ivd'] = '\x5b\x5b\x5b\x22FILE_ID\x22,...'
    //    После декодирования: [["FILE_ID",["FOLDER_ID"],"name.zip","application/...",...
    files.extend(parse_drive_ivd(html));

    // 2) Старый JSON-подобный фрагмент: ["MyPack.zip",null,"application/zip",... ,"FILE_ID"]
    if files.is_empty() {
        if let Ok(re) = regex_lite::Regex::new(
            r#"\["([^"]+\.(?:zip|json|jar|txt|mrpack))",null,"(application/[^"]+|text/[^"]+)"[^]]*?,"([a-zA-Z0-9_-]{25,44})""#,
        ) {
            for cap in re.captures_iter(html) {
                let name = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let mime = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                let id = cap.get(3).map(|m| m.as_str()).unwrap_or("").to_string();
                if !name.is_empty() && !id.is_empty() && id != DRIVE_FOLDER_ID {
                    files.push(DriveFile {
                        id,
                        name: decode_js_string(&name),
                        mime,
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    // 3) DOM: data-id="..." data-tooltip="file.zip ..." / aria-label="..."
    if files.is_empty() {
        if let Ok(re) = regex_lite::Regex::new(
            r#"data-id="([a-zA-Z0-9_-]{25,44})"[^>]{0,400}?(?:data-tooltip|aria-label|aria-labelledby)="([^"]+)""#,
        ) {
            for cap in re.captures_iter(html) {
                let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let label = cap.get(2).map(|m| m.as_str()).unwrap_or("");
                if id == DRIVE_FOLDER_ID {
                    continue;
                }
                if let Some(name) = filename_from_label(label) {
                    files.push(DriveFile {
                        id,
                        name,
                        mime: String::new(),
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    // 4) DOM: aria-label / data-tooltip перед data-id (порядок атрибутов другой)
    if files.is_empty() {
        if let Ok(re) = regex_lite::Regex::new(
            r#"(?:data-tooltip|aria-label)="([^"]+)"[^>]{0,400}?data-id="([a-zA-Z0-9_-]{25,44})""#,
        ) {
            for cap in re.captures_iter(html) {
                let label = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                let id = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                if id == DRIVE_FOLDER_ID {
                    continue;
                }
                if let Some(name) = filename_from_label(label) {
                    files.push(DriveFile {
                        id,
                        name,
                        mime: String::new(),
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    // 5) ssk='...:FILE_ID-...' рядом с aria-label / title
    if files.is_empty() {
        if let Ok(re) = regex_lite::Regex::new(
            r#"aria-label="([^"]+)"[^>]{0,200}?ssk='[^']*:([a-zA-Z0-9_-]{25,44})-"#,
        ) {
            for cap in re.captures_iter(html) {
                let label = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                let id = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                if id == DRIVE_FOLDER_ID {
                    continue;
                }
                if let Some(name) = filename_from_label(label) {
                    files.push(DriveFile {
                        id,
                        name,
                        mime: String::new(),
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    files
}

/// Парсит `window['_DRIVE_ivd']` — основной источник списка файлов в публичной папке.
fn parse_drive_ivd(html: &str) -> Vec<DriveFile> {
    let mut files = Vec::new();

    // Ищем payload (одинарные кавычки вокруг \x.. строки).
    let Some(start_marker) = html.find("_DRIVE_ivd") else {
        return files;
    };
    let rest = &html[start_marker..];
    let Some(eq) = rest.find('=') else {
        return files;
    };
    let after_eq = rest[eq + 1..].trim_start();
    let quote = after_eq.chars().next().unwrap_or('\0');
    if quote != '\'' && quote != '"' {
        return files;
    }
    let body = &after_eq[1..];
    let Some(end) = body.find(quote) else {
        return files;
    };
    let encoded = &body[..end];
    let decoded = decode_js_hex_escapes(encoded);

    // ["FILE_ID",["FOLDER_ID"],"name.ext","mime/type",...,MODIFIED,CREATED,null,null,SIZE
    // id 25–44 символа; Drive отдаёт timestamps в миллисекундах.
    let Ok(re) = regex_lite::Regex::new(
        r#"\["([a-zA-Z0-9_-]{25,44})",\["([a-zA-Z0-9_-]{25,44})"\],"([^"]+\.(?:zip|json|jar|txt|mrpack))","([^"]*)"([^\]]{0,240})"#,
    ) else {
        return files;
    };
    let metadata_re = regex_lite::Regex::new(r#",(\d{10,16}),(\d{10,16}),null,null,(\d{1,})"#).ok();

    for cap in re.captures_iter(&decoded) {
        let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let parent = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let name = cap.get(3).map(|m| m.as_str()).unwrap_or("").to_string();
        let mime = cap
            .get(4)
            .map(|m| m.as_str().replace("\\/", "/"))
            .unwrap_or_default();
        let metadata = cap.get(5).map(|m| m.as_str()).unwrap_or("");
        let metadata = metadata_re
            .as_ref()
            .and_then(|metadata_re| metadata_re.captures(metadata));
        let modified_time_ms = metadata
            .as_ref()
            .and_then(|metadata| metadata.get(1))
            .and_then(|value| value.as_str().parse::<u64>().ok())
            .map(normalize_drive_timestamp);
        let size = metadata
            .as_ref()
            .and_then(|metadata| metadata.get(3))
            .and_then(|value| value.as_str().parse::<u64>().ok())
            .filter(|&size| size > 0);
        if id.is_empty() || id == DRIVE_FOLDER_ID || name.is_empty() {
            continue;
        }
        // parent обычно = id папки; если нет — всё равно берём файл.
        let _ = parent;
        files.push(DriveFile {
            id,
            name: decode_js_string(&name),
            mime,
            size,
            modified_time_ms,
        });
    }

    // Запасной паттерн без mime / folder: "FILE_ID" ... "name.zip"
    if files.is_empty() {
        if let Ok(re2) = regex_lite::Regex::new(
            r#""([a-zA-Z0-9_-]{25,44})"[^"]{0,80}"([^"]+\.(?:zip|json|jar|txt|mrpack))""#,
        ) {
            for cap in re2.captures_iter(&decoded) {
                let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let name = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                if id != DRIVE_FOLDER_ID && !name.is_empty() {
                    files.push(DriveFile {
                        id,
                        name: decode_js_string(&name),
                        mime: String::new(),
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    files
}

fn normalize_drive_timestamp(value: u64) -> u64 {
    if value < 100_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

fn parse_embedded_folder(html: &str) -> Vec<DriveFile> {
    let mut files = Vec::new();

    // Современный embeddedfolderview:
    // <div class="flip-entry" id="entry-FILEID" ...>
    //   <a href=".../file/d/FILEID/..."> ... <div class="flip-entry-title">name.zip</div>
    if let Ok(re) = regex_lite::Regex::new(
        r#"id="entry-([a-zA-Z0-9_-]{25,44})"[^>]*>[\s\S]{0,2500}?flip-entry-title">([^<]+)<"#,
    ) {
        for cap in re.captures_iter(html) {
            let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let name = cap
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            if id != DRIVE_FOLDER_ID && looks_like_build_file(&name) {
                files.push(DriveFile {
                    id,
                    name,
                    mime: String::new(),
                    size: None,
                    modified_time_ms: None,
                });
            }
        }
    }

    // Старый формат: <a href=".../file/d/ID/...">name</a>
    if files.is_empty() {
        if let Ok(re) = regex_lite::Regex::new(
            r#"/file/d/([a-zA-Z0-9_-]{25,44})/[^"]*"[^>]*>([^<]+)<"#,
        ) {
            for cap in re.captures_iter(html) {
                let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let name = cap
                    .get(2)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default();
                if looks_like_build_file(&name) {
                    files.push(DriveFile {
                        id,
                        name,
                        mime: String::new(),
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    // href + flip-entry-title рядом
    if files.is_empty() {
        if let Ok(re) = regex_lite::Regex::new(
            r#"/file/d/([a-zA-Z0-9_-]{25,44})/[^"]*"[\s\S]{0,2000}?flip-entry-title">([^<]+)<"#,
        ) {
            for cap in re.captures_iter(html) {
                let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let name = cap
                    .get(2)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default();
                if id != DRIVE_FOLDER_ID && looks_like_build_file(&name) {
                    files.push(DriveFile {
                        id,
                        name,
                        mime: String::new(),
                        size: None,
                        modified_time_ms: None,
                    });
                }
            }
        }
    }

    files
}

fn looks_like_build_file(name: &str) -> bool {
    let l = name.to_lowercase();
    l.ends_with(".zip")
        || l.ends_with(".json")
        || l.ends_with(".jar")
        || l.ends_with(".mrpack")
        || l == "builds.json"
}

/// Из подписи Drive («createA2.zip Compressed archive Shared») вытаскивает имя файла.
fn filename_from_label(label: &str) -> Option<String> {
    let label = label.trim();
    if looks_like_build_file(label) {
        return Some(label.to_string());
    }
    for part in label.split_whitespace() {
        if looks_like_build_file(part) {
            return Some(part.to_string());
        }
    }
    None
}

fn decode_js_string(s: &str) -> String {
    s.replace("\\u0026", "&")
        .replace("\\/", "/")
        .replace("\\\"", "\"")
}

/// Декодирует `\xNN` (и простые `\\`, `\/`) из JS-строки Drive payload.
fn decode_js_hex_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'x' | b'X' if i + 3 < bytes.len() => {
                    let h1 = bytes[i + 2] as char;
                    let h2 = bytes[i + 3] as char;
                    if let (Some(a), Some(b)) = (h1.to_digit(16), h2.to_digit(16)) {
                        out.push(char::from_u32((a << 4) | b).unwrap_or('?'));
                        i += 4;
                        continue;
                    }
                }
                b'u' | b'U' if i + 5 < bytes.len() => {
                    // \uXXXX
                    let hex = &s[i + 2..i + 6];
                    if let Ok(cp) = u32::from_str_radix(hex, 16) {
                        if let Some(ch) = char::from_u32(cp) {
                            out.push(ch);
                            i += 6;
                            continue;
                        }
                    }
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                    continue;
                }
                b'\\' | b'/' | b'\'' | b'"' => {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn stem_id(filename: &str) -> String {
    let name = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.to_string());
    slug(&name)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else if c.is_whitespace() {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    let out = out.trim_matches('-').to_string();
    sanitize_build_id(&out)
}

fn pretty_name(id: &str) -> String {
    id.replace(['-', '_'], " ")
}

/// Скачивает файл с Drive (с обработкой предупреждения о вирусах для больших файлов).
pub fn download_drive_file(
    client: &reqwest::blocking::Client,
    file_id: &str,
    dest: &Path,
    progress: Option<&ProgressFn>,
    label: &str,
    known_size: Option<u64>,
) -> Result<(), LauncherError> {
    download_drive_file_with_cancel(client, file_id, dest, progress, label, known_size, None)
}

fn download_drive_file_with_cancel(
    client: &reqwest::blocking::Client,
    file_id: &str,
    dest: &Path,
    progress: Option<&ProgressFn>,
    label: &str,
    known_size: Option<u64>,
    cancel: Option<&AtomicBool>,
) -> Result<(), LauncherError> {
    if is_cancelled(cancel) {
        return Err(cancelled_error());
    }
    if dest.exists() {
        let len = dest.metadata().map(|m| m.len()).unwrap_or(0);
        let size_matches = known_size
            .filter(|&size| size > 0)
            .map_or(true, |size| size == len);
        if len > 0 && size_matches {
            if let Some(cb) = progress {
                let total = known_size.unwrap_or(len).max(len);
                cb(total, total, label);
            }
            return Ok(());
        }

        // Нулевой или не совпадающий с ожидаемым размером файл — не кэш, а
        // след незавершённой загрузки.
        if dest.is_file() {
            fs::remove_file(dest)?;
        }
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let url = direct_download_url(file_id);
    let resp = client
        .get(&url)
        .header(reqwest::header::CACHE_CONTROL, "no-cache, no-store")
        .header(reqwest::header::PRAGMA, "no-cache")
        .send()
        .map_err(|e| LauncherError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| LauncherError::Network(e.to_string()))?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Большие файлы: HTML с формой подтверждения антивирусного предупреждения.
    if content_type.contains("text/html") {
        let html = resp
            .text()
            .map_err(|e| LauncherError::Network(e.to_string()))?;
        let size_from_html = extract_size_from_confirm_html(&html);

        // Новый формат Drive (2026): action ведёт на drive.usercontent.google.com,
        // а кроме confirm=t обязателен одноразовый uuid из hidden input.
        if let Some((action, params)) = extract_download_form(&html) {
            let resp2 = client
                .get(action)
                .query(&params)
                .send()
                .map_err(|e| LauncherError::Network(e.to_string()))?
                .error_for_status()
                .map_err(|e| LauncherError::Network(e.to_string()))?;
            return stream_drive_response(
                resp2,
                dest,
                progress,
                label,
                known_size.or(size_from_html),
                cancel,
            );
        }

        // Старый формат Drive: confirm-токен находился прямо в ссылке.
        if let Some(confirm) = extract_confirm_token(&html) {
            let url2 = format!(
                "https://drive.google.com/uc?export=download&confirm={confirm}&id={file_id}"
            );
            let resp2 = client
                .get(&url2)
                .send()
                .map_err(|e| LauncherError::Network(e.to_string()))?
                .error_for_status()
                .map_err(|e| LauncherError::Network(e.to_string()))?;
            return stream_drive_response(
                resp2,
                dest,
                progress,
                label,
                known_size.or(size_from_html),
                cancel,
            );
        }
        if html.contains("accounts.google.com") || html.contains("Sign in") {
            return Err(LauncherError::Other(
                "Нет доступа к файлу на Google Drive. Откройте доступ «всем по ссылке»."
                    .into(),
            ));
        }
        return Err(LauncherError::Other(
            "Google Drive вернул HTML вместо файла (проверьте доступ к папке).".into(),
        ));
    }

    stream_to_file(resp, dest, progress, label, known_size, cancel)
}

fn stream_drive_response(
    resp: reqwest::blocking::Response,
    dest: &Path,
    progress: Option<&ProgressFn>,
    label: &str,
    known_size: Option<u64>,
    cancel: Option<&AtomicBool>,
) -> Result<(), LauncherError> {
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("text/html") {
        let html = resp
            .text()
            .map_err(|e| LauncherError::Network(e.to_string()))?;
        if html.contains("accounts.google.com") || html.contains("Sign in") {
            return Err(LauncherError::Other(
                "Нет доступа к файлу на Google Drive. Откройте доступ «всем по ссылке»."
                    .into(),
            ));
        }
        return Err(LauncherError::Other(
            "Google Drive повторно вернул веб-страницу вместо архива.".into(),
        ));
    }
    stream_to_file(resp, dest, progress, label, known_size, cancel)
}

fn download_drive_bytes(
    client: &reqwest::blocking::Client,
    file_id: &str,
    progress: Option<&ProgressFn>,
    label: &str,
) -> Result<Vec<u8>, LauncherError> {
    // Маленькие файлы (builds.json) — через temp.
    let tmp_dir = builds_dir().join(".tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp = tmp_dir.join(format!("{file_id}.bin"));
    // После аварийного завершения здесь мог остаться старый builds.json.
    // Каталог перед запуском всегда должен читаться заново.
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(tmp.with_extension("part"));
    download_drive_file(client, file_id, &tmp, progress, label, None)?;
    let bytes = fs::read(&tmp)?;
    let _ = fs::remove_file(&tmp);
    Ok(bytes)
}

fn stream_to_file(
    mut resp: reqwest::blocking::Response,
    dest: &Path,
    progress: Option<&ProgressFn>,
    label: &str,
    known_size: Option<u64>,
    cancel: Option<&AtomicBool>,
) -> Result<(), LauncherError> {
    let header_total = resp.content_length().unwrap_or(0);
    let range_total = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range_total)
        .unwrap_or(0);
    // HTTP-размер надёжнее размера из каталога Drive. Последний используем
    // как fallback, когда сервер не прислал ни Content-Length, ни Content-Range.
    let response_total = header_total.max(range_total);
    let expected = if response_total > 0 {
        response_total
    } else {
        known_size.unwrap_or(0)
    };
    // total == 0 → размер неизвестен (не подставляем done, иначе UI всегда 100%).
    let total = response_total.max(known_size.unwrap_or(0));

    let tmp = dest.with_extension("part");
    let mut file = File::create(&tmp)?;
    let mut chunk = [0u8; 64 * 1024];
    let mut done = 0u64;
    let mut last_report = 0u64;
    const REPORT_EVERY: u64 = 256 * 1024; // ~0.25 МБ

    if let Some(cb) = progress {
        cb(0, total, label);
    }

    loop {
        if is_cancelled(cancel) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(cancelled_error());
        }
        let n = resp
            .read(&mut chunk)
            .map_err(|e| LauncherError::Network(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n])?;
        done += n as u64;
        if let Some(cb) = progress {
            if done - last_report >= REPORT_EVERY || (total > 0 && done >= total) {
                last_report = done;
                cb(done, total, label);
            }
        }
    }
    if is_cancelled(cancel) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(cancelled_error());
    }
    file.flush()?;
    drop(file);

    if expected > 0 && done != expected {
        let _ = fs::remove_file(&tmp);
        return Err(LauncherError::Network(format!(
            "загрузка «{label}» оборвалась: получено {done} из {expected} байт"
        )));
    }

    // финальный отчёт
    if let Some(cb) = progress {
        let end_total = if total > 0 { total } else { done };
        cb(done, end_total, label);
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

fn cancelled_error() -> LauncherError {
    LauncherError::Other("Отменено".into())
}

/// `Content-Range: bytes 0-1023/2048` или `bytes */2048`
fn parse_content_range_total(v: &str) -> Option<u64> {
    let slash = v.rfind('/')?;
    let total = v[slash + 1..].trim();
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

fn extract_size_from_confirm_html(html: &str) -> Option<u64> {
    // Иногда: " (123.4M) " / size: '123456'
    if let Ok(re) = regex_lite::Regex::new(r#"uc-name-size[^>]*>\s*\(([^)]+)\)"#) {
        if let Some(cap) = re.captures(html) {
            if let Some(s) = cap.get(1).map(|m| m.as_str().trim()) {
                if let Some(n) = parse_human_size(s) {
                    return Some(n);
                }
            }
        }
    }
    if let Ok(re) = regex_lite::Regex::new(r#"(?i)(?:content-length|size)["'\s:=]+(\d{4,})"#) {
        if let Some(cap) = re.captures(html) {
            if let Ok(n) = cap[1].parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim().replace(',', ".");
    let lower = s.to_lowercase();
    let (num, mult) = if let Some(rest) = lower.strip_suffix('g') {
        (rest.trim(), 1024u64 * 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix('m') {
        (rest.trim(), 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix('k') {
        (rest.trim(), 1024)
    } else if let Some(rest) = lower.strip_suffix("gb") {
        (rest.trim(), 1024u64 * 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix("mb") {
        (rest.trim(), 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix("kb") {
        (rest.trim(), 1024)
    } else {
        return s.parse().ok();
    };
    let f: f64 = num.parse().ok()?;
    Some((f * mult as f64) as u64)
}

fn extract_download_form(html: &str) -> Option<(reqwest::Url, Vec<(String, String)>)> {
    let lower = html.to_ascii_lowercase();
    let id_re =
        regex_lite::Regex::new(r#"(?i)\bid\s*=\s*["']download-form["']"#).ok()?;
    let id_match = id_re.find(html)?;
    let form_start = lower[..id_match.start()].rfind("<form")?;
    let tag_end = form_start + html[form_start..].find('>')? + 1;
    let form_end = tag_end + lower[tag_end..].find("</form>")?;
    let form_tag = &html[form_start..tag_end];
    let form_body = &html[tag_end..form_end];

    let action = decode_html_attribute(&html_attribute(form_tag, "action")?);
    let action = if action.starts_with("//") {
        format!("https:{action}")
    } else if action.starts_with('/') {
        format!("https://drive.google.com{action}")
    } else {
        action
    };
    let action = reqwest::Url::parse(&action).ok()?;
    if action.scheme() != "https"
        || !matches!(
            action.host_str(),
            Some("drive.google.com" | "drive.usercontent.google.com")
        )
    {
        return None;
    }

    let mut params = Vec::new();
    let body_lower = form_body.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative_start) = body_lower[offset..].find("<input") {
        let input_start = offset + relative_start;
        let Some(relative_end) = form_body[input_start..].find('>') else {
            break;
        };
        let input_end = input_start + relative_end + 1;
        let input_tag = &form_body[input_start..input_end];
        if let Some(name) = html_attribute(input_tag, "name") {
            let value = html_attribute(input_tag, "value").unwrap_or_default();
            params.push((
                decode_html_attribute(&name),
                decode_html_attribute(&value),
            ));
        }
        offset = input_end;
    }

    if !params.iter().any(|(name, _)| name == "id")
        || !params.iter().any(|(name, _)| name == "confirm")
    {
        return None;
    }
    Some((action, params))
}

fn html_attribute(tag: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?i)\b{name}\s*=\s*["']([^"']*)["']"#);
    let re = regex_lite::Regex::new(&pattern).ok()?;
    re.captures(tag)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
}

fn decode_html_attribute(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn extract_confirm_token(html: &str) -> Option<String> {
    // confirm=XXXX
    for key in ["confirm=", "confirm&amp;"] {
        if let Some(pos) = html.find(key) {
            let rest = &html[pos + key.len()..];
            let token: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !token.is_empty() && token != "t" {
                return Some(token);
            }
        }
    }
    None
}

fn validate_zip_archive(zip_path: &Path) -> Result<(), LauncherError> {
    let file = File::open(zip_path)?;
    let archive = ZipArchive::new(file).map_err(|e| LauncherError::Other(format!("ZIP: {e}")))?;
    if archive.is_empty() {
        return Err(LauncherError::Other("ZIP: архив пуст".into()));
    }
    Ok(())
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), LauncherError> {
    let file = File::open(zip_path)?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| LauncherError::Other(format!("ZIP: {e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| LauncherError::Other(e.to_string()))?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| LauncherError::Other("Некорректный путь в ZIP".into()))?
            .to_path_buf();
        let out_path = dest.join(&name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = File::create(&out_path)?;
            copy(&mut entry, &mut outfile)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("mine-launcher-{label}-{}", operation_id()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample_build(modified_time_ms: u64, size: u64) -> BuildInfo {
        BuildInfo {
            id: "stable-pack".into(),
            name: "Stable Pack".into(),
            file_id: "1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj".into(),
            filename: "stable-pack.zip".into(),
            size: Some(size),
            modified_time_ms: Some(modified_time_ms),
            minecraft: Some("1.21.1".into()),
        }
    }

    fn write_test_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, contents) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn parse_drive_ivd_modern_payload() {
        let html = r#"
        <script>window['_DRIVE_ivd'] = '\x5b\x5b\x5b\x221AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj\x22,\x5b\x221mEl5hfZqx5IUiS_gULZBz4v116YuHYtq\x22\x5d,\x22createA2.zip\x22,\x22application\/x-zip-compressed\x22,0,null,0,0,0,1784988932123,1784987343000,null,null,459433190,';</script>
        "#;
        let files = parse_drive_html(html);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj");
        assert_eq!(files[0].name, "createA2.zip");
        assert!(files[0].mime.contains("zip"));
        assert_eq!(files[0].size, Some(459433190));
        assert_eq!(files[0].modified_time_ms, Some(1784988932123));
    }

    #[test]
    fn manifest_build_is_enriched_even_when_file_id_is_already_set() {
        let mut builds = vec![BuildInfo {
            id: "stable-pack".into(),
            name: "Stable Pack".into(),
            file_id: "1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj".into(),
            filename: "stable-pack.zip".into(),
            size: Some(10),
            modified_time_ms: None,
            minecraft: None,
        }];
        let files = vec![DriveFile {
            id: builds[0].file_id.clone(),
            name: builds[0].filename.clone(),
            mime: "application/zip".into(),
            size: Some(500),
            modified_time_ms: Some(1784988932123),
        }];

        enrich_manifest_builds(&mut builds, &files);

        assert_eq!(builds[0].size, Some(500));
        assert_eq!(builds[0].modified_time_ms, Some(1784988932123));
    }

    #[test]
    fn update_status_uses_drive_revision_and_legacy_install_requires_update() {
        let temp = TestDirectory::new("revision");
        let instance = temp.path.join("instance");
        fs::create_dir_all(instance.join("minecraft").join("mods")).unwrap();
        let build = sample_build(1784988932123, 500);

        assert_eq!(
            build_update_status_at(&build, &instance).unwrap(),
            BuildUpdateStatus::UpdateRequired
        );

        let state = InstalledBuildState::from_build(&build).unwrap();
        write_install_state(&instance, &state).unwrap();
        assert_eq!(
            build_update_status_at(&build, &instance).unwrap(),
            BuildUpdateStatus::Current
        );

        let changed_time = sample_build(1784988932124, 500);
        assert_eq!(
            build_update_status_at(&changed_time, &instance).unwrap(),
            BuildUpdateStatus::UpdateRequired
        );

        let changed_file = BuildInfo {
            file_id: "1NEWFILEID000000000000000000000000".into(),
            ..build.clone()
        };
        assert_eq!(
            build_update_status_at(&changed_file, &instance).unwrap(),
            BuildUpdateStatus::UpdateRequired
        );
    }

    #[test]
    fn missing_required_drive_metadata_blocks_update_check() {
        let temp = TestDirectory::new("missing-metadata");
        let instance = temp.path.join("instance");
        let build = BuildInfo {
            modified_time_ms: None,
            ..sample_build(1784988932123, 500)
        };

        let error = build_update_status_at(&build, &instance)
            .unwrap_err()
            .to_string();
        assert!(error.contains("время изменения"));
    }

    #[test]
    fn atomic_update_replaces_pack_files_and_preserves_user_data() {
        let temp = TestDirectory::new("atomic-update");
        let destination = temp.path.join("stable-pack");
        let staging = temp.path.join(".stable-pack-update");
        let backup = temp.path.join(".stable-pack-backup");
        let archive = temp.path.join("update.zip");

        fs::create_dir_all(destination.join("minecraft").join("mods")).unwrap();
        fs::create_dir_all(destination.join("minecraft").join("config")).unwrap();
        fs::create_dir_all(destination.join("minecraft").join("saves").join("my-world")).unwrap();
        fs::write(
            destination.join("minecraft").join("mods").join("old.jar"),
            b"old mod",
        )
        .unwrap();
        fs::write(
            destination
                .join("minecraft")
                .join("config")
                .join("local.cfg"),
            b"old config",
        )
        .unwrap();
        fs::write(
            destination
                .join("minecraft")
                .join("saves")
                .join("my-world")
                .join("level.dat"),
            b"my world",
        )
        .unwrap();
        fs::write(
            destination.join("minecraft").join("options.txt"),
            b"user options",
        )
        .unwrap();

        write_test_zip(
            &archive,
            &[
                ("minecraft/mods/new.jar", b"new mod"),
                ("minecraft/config/pack.cfg", b"new config"),
                ("minecraft/saves/example/level.dat", b"example world"),
                ("minecraft/options.txt", b"pack options"),
            ],
        );
        let build = sample_build(1784988932124, fs::metadata(&archive).unwrap().len());
        let state = InstalledBuildState::from_build(&build).unwrap();
        let progress: ProgressFn = Arc::new(|_, _, _| {});
        let cancel = AtomicBool::new(false);

        install_archive_atomically(
            &build,
            state,
            &archive,
            &destination,
            &staging,
            &backup,
            &progress,
            &cancel,
        )
        .unwrap();

        assert!(!destination
            .join("minecraft")
            .join("mods")
            .join("old.jar")
            .exists());
        assert!(destination
            .join("minecraft")
            .join("mods")
            .join("new.jar")
            .is_file());
        assert!(!destination
            .join("minecraft")
            .join("config")
            .join("local.cfg")
            .exists());
        assert!(destination
            .join("minecraft")
            .join("config")
            .join("pack.cfg")
            .is_file());
        assert_eq!(
            fs::read(
                destination
                    .join("minecraft")
                    .join("saves")
                    .join("my-world")
                    .join("level.dat")
            )
            .unwrap(),
            b"my world"
        );
        assert_eq!(
            fs::read(destination.join("minecraft").join("options.txt")).unwrap(),
            b"user options"
        );
        assert_eq!(
            build_update_status_at(&build, &destination).unwrap(),
            BuildUpdateStatus::Current
        );
        assert!(!backup.exists());
    }

    #[test]
    fn invalid_archive_or_cancellation_keeps_existing_instance() {
        let temp = TestDirectory::new("atomic-failure");
        let destination = temp.path.join("stable-pack");
        let staging = temp.path.join(".stable-pack-update");
        let backup = temp.path.join(".stable-pack-backup");
        let archive = temp.path.join("broken.zip");
        fs::create_dir_all(destination.join("minecraft").join("mods")).unwrap();
        let old_mod = destination.join("minecraft").join("mods").join("old.jar");
        fs::write(&old_mod, b"old mod").unwrap();
        fs::write(&archive, b"not a zip").unwrap();

        let build = sample_build(1784988932124, 9);
        let state = InstalledBuildState::from_build(&build).unwrap();
        let progress: ProgressFn = Arc::new(|_, _, _| {});
        let cancel = AtomicBool::new(false);
        assert!(install_archive_atomically(
            &build,
            state.clone(),
            &archive,
            &destination,
            &staging,
            &backup,
            &progress,
            &cancel,
        )
        .is_err());
        assert_eq!(fs::read(&old_mod).unwrap(), b"old mod");

        let valid_archive = temp.path.join("valid.zip");
        write_test_zip(&valid_archive, &[("minecraft/mods/new.jar", b"new mod")]);
        let cancelled = AtomicBool::new(true);
        assert!(install_archive_atomically(
            &build,
            state,
            &valid_archive,
            &destination,
            &staging,
            &backup,
            &progress,
            &cancelled,
        )
        .is_err());
        assert_eq!(fs::read(&old_mod).unwrap(), b"old mod");
        assert!(!staging.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn parse_data_id_tooltip() {
        let html = r#"
        <div data-id="1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj" jsname="vtaz5c" data-tooltip="createA2.zip Compressed archive"></div>
        "#;
        let files = parse_drive_html(html);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "createA2.zip");
        assert_eq!(files[0].id, "1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj");
    }

    #[test]
    fn parse_embedded_flip_entry() {
        let html = r#"
        <div class="flip-entry" id="entry-1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj" tabindex="0" role="link">
          <div class="flip-entry-info">
            <a href="https://drive.google.com/file/d/1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj/view?usp=drive_web" target="_blank">
              <div class="flip-entry-title">createA2.zip</div>
            </a>
          </div>
        </div>
        "#;
        let files = parse_embedded_folder(html);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj");
        assert_eq!(files[0].name, "createA2.zip");
    }

    #[test]
    fn filename_from_drive_label() {
        assert_eq!(
            filename_from_label("createA2.zip Compressed archive Shared").as_deref(),
            Some("createA2.zip")
        );
        assert_eq!(filename_from_label("builds.json").as_deref(), Some("builds.json"));
        assert!(filename_from_label("Shared folder").is_none());
    }

    #[test]
    fn parses_modern_drive_download_form_with_uuid() {
        let html = r#"
        <form id="download-form" action="https://drive.usercontent.google.com/download" method="get">
          <input type="hidden" name="id" value="1AVVO2ENG0WYFduO_1TbEi4L20rPs7eXj">
          <input type="hidden" name="export" value="download">
          <input type="hidden" name="confirm" value="t">
          <input type="hidden" name="uuid" value="1d9f5368-1c76-43bc-a6fa-59d6253a5508">
        </form>
        "#;

        let (action, params) = extract_download_form(html).unwrap();
        assert_eq!(action.as_str(), "https://drive.usercontent.google.com/download");
        assert!(params
            .iter()
            .any(|(name, value)| name == "confirm" && value == "t"));
        assert!(params.iter().any(|(name, value)| {
            name == "uuid" && value == "1d9f5368-1c76-43bc-a6fa-59d6253a5508"
        }));
    }

    #[test]
    fn validates_complete_zip_and_rejects_truncated_zip() {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let valid_path =
            std::env::temp_dir().join(format!("mine-launcher-valid-{suffix}.zip"));
        let truncated_path =
            std::env::temp_dir().join(format!("mine-launcher-truncated-{suffix}.zip"));

        let file = File::create(&valid_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("build.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"{}").unwrap();
        writer.finish().unwrap();
        fs::write(&truncated_path, b"PK\x03\x04incomplete archive").unwrap();

        assert!(validate_zip_archive(&valid_path).is_ok());
        assert!(validate_zip_archive(&truncated_path).is_err());

        let _ = fs::remove_file(valid_path);
        let _ = fs::remove_file(truncated_path);
    }
}
