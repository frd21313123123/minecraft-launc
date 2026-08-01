use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::download::{download_json, http_client, ProgressFn};
use crate::error::LauncherError;
use crate::paths::LAUNCHER_VERSION;

#[cfg(test)]
const REPOSITORY: &str = "frd21313123123/minecraft-launc";
const RELEASES_API_URL: &str =
    "https://api.github.com/repos/frd21313123123/minecraft-launc/releases?per_page=20";
const COMPARE_API_PREFIX: &str =
    "https://api.github.com/repos/frd21313123123/minecraft-launc/compare";
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const UPDATE_MANIFEST_ASSET: &str = "MineLauncher-update.json";
const HEALTH_GRACE_PERIOD: Duration = Duration::from_secs(5);
const APPLY_UPDATE_ARG: &str = "--minelauncher-apply-update";
const UPDATE_HEALTH_ARG: &str = "--minelauncher-update-health";
const CLEANUP_UPDATE_ARG: &str = "--minelauncher-cleanup-update";

/// Commit, from which the release binary was built. GitHub Actions sets it in
/// `.github/workflows/build.yml`; local builds intentionally have no value.
pub const BUILD_COMMIT: Option<&str> = option_env!("MINELAUNCHER_BUILD_COMMIT");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub display_version: String,
    pub release_name: String,
    pub tag: String,
    pub download_url: String,
    pub size: u64,
    pub sha256: String,
    pub html_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate,
    Available(UpdateInfo),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedUpdate {
    pub staged_path: PathBuf,
    pub target_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompareResponse {
    status: String,
}

#[derive(Debug, Deserialize)]
struct UpdateManifest {
    schema: u32,
    commit: String,
    assets: HashMap<String, ManifestAsset>,
}

#[derive(Debug, Deserialize)]
struct ManifestAsset {
    sha256: String,
    size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StableVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

#[derive(Clone, Debug)]
struct HealthContext {
    marker: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    target: PathBuf,
}

static HEALTH_CONTEXT: OnceLock<HealthContext> = OnceLock::new();
static HEALTH_REPORTED: AtomicBool = AtomicBool::new(false);

/// Checks GitHub Releases without blocking the UI. Stable `vX.Y.Z` releases
/// are compared by version. The mutable `latest` channel is accepted only when
/// GitHub confirms that its commit is ahead of this release binary.
pub fn check_for_update() -> Result<CheckOutcome, LauncherError> {
    let asset_name = platform_asset_name().ok_or_else(|| {
        LauncherError::Other("Автообновление не поддерживается на этой платформе".into())
    })?;
    let client = http_client()?;
    let releases: Vec<GithubRelease> = download_json(&client, RELEASES_API_URL)?;
    let current_version = parse_stable_version(LAUNCHER_VERSION).ok_or_else(|| {
        LauncherError::Parse(format!("Некорректная версия лаунчера: {LAUNCHER_VERSION}"))
    })?;

    if let Some(release) = newest_stable_release(&releases, current_version) {
        return update_info_from_release(release, asset_name, release.tag_name.clone())
            .map(CheckOutcome::Available);
    }

    let Some(current_commit) = BUILD_COMMIT.and_then(normalize_commit) else {
        // A local build has no reliable place in the rolling commit history.
        return Ok(CheckOutcome::UpToDate);
    };
    let Some(rolling) = releases
        .iter()
        .find(|release| !release.draft && release.tag_name.eq_ignore_ascii_case("latest"))
    else {
        return Ok(CheckOutcome::UpToDate);
    };
    let Some(target_commit) = rolling_build_commit(&client, rolling, asset_name)? else {
        // Older rolling releases did not publish a build manifest. Treat them
        // as unknown rather than guessing from GitHub's target_commitish field,
        // which is not the identity of an updated release asset.
        return Ok(CheckOutcome::UpToDate);
    };
    if commits_match(current_commit, &target_commit) {
        return Ok(CheckOutcome::UpToDate);
    }

    let compare_url = format!("{COMPARE_API_PREFIX}/{current_commit}...{target_commit}");
    let comparison: CompareResponse = download_json(&client, &compare_url)?;
    if !rolling_status_is_newer(&comparison.status) {
        return Ok(CheckOutcome::UpToDate);
    }

    let short_commit = &target_commit[..target_commit.len().min(7)];
    update_info_from_release(rolling, asset_name, format!("latest ({short_commit})"))
        .map(CheckOutcome::Available)
}

/// Downloads the selected release next to the executable, verifies both its
/// exact size and GitHub-provided SHA-256, and only then exposes the staged file.
pub fn download_update(
    update: &UpdateInfo,
    progress: Option<&ProgressFn>,
) -> Result<PreparedUpdate, LauncherError> {
    validate_update_info(update)?;
    let target_path = std::env::current_exe()?;
    let parent = target_path
        .parent()
        .ok_or_else(|| LauncherError::Other("Не удалось определить папку лаунчера".into()))?;
    let staged_path = staged_path_for(&target_path, std::process::id())?;
    let part_path = part_path_for(&staged_path)?;
    remove_file_if_exists(&part_path)?;
    remove_file_if_exists(&staged_path)?;

    let result = (|| -> Result<(), LauncherError> {
        let client = http_client()?;
        let mut response = client
            .get(&update.download_url)
            .send()
            .map_err(|error| LauncherError::Network(error.to_string()))?
            .error_for_status()
            .map_err(|error| LauncherError::Network(error.to_string()))?;

        if let Some(length) = response.content_length() {
            if length != update.size {
                return Err(LauncherError::Other(format!(
                    "GitHub сообщил размер {length}, а в release asset указан {}",
                    update.size
                )));
            }
        }

        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part_path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut downloaded = 0u64;

        loop {
            let count = response
                .read(&mut buffer)
                .map_err(|error| LauncherError::Network(error.to_string()))?;
            if count == 0 {
                break;
            }
            downloaded = downloaded
                .checked_add(count as u64)
                .ok_or_else(|| LauncherError::Other("Слишком большой файл обновления".into()))?;
            if downloaded > update.size || downloaded > MAX_UPDATE_BYTES {
                return Err(LauncherError::Other(
                    "Размер загружаемого обновления превышает ожидаемый".into(),
                ));
            }
            file.write_all(&buffer[..count])?;
            hasher.update(&buffer[..count]);
            if let Some(callback) = progress {
                callback(downloaded, update.size, "Загрузка обновления лаунчера");
            }
        }

        file.flush()?;
        file.sync_all()?;
        drop(file);

        if downloaded != update.size {
            return Err(LauncherError::Other(format!(
                "Обновление загружено не полностью: {downloaded} из {} байт",
                update.size
            )));
        }
        let actual_hash = hex::encode(hasher.finalize());
        if !actual_hash.eq_ignore_ascii_case(&update.sha256) {
            return Err(LauncherError::Checksum {
                path: part_path.display().to_string(),
                expected: update.sha256.clone(),
                got: actual_hash,
            });
        }
        validate_executable_header(&part_path)?;
        make_executable(&part_path)?;
        fs::rename(&part_path, &staged_path)?;
        sync_directory(parent);
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&part_path);
        let _ = fs::remove_file(&staged_path);
        return Err(error);
    }

    Ok(PreparedUpdate {
        staged_path,
        target_path,
    })
}

/// Starts the verified candidate in an internal helper mode. The caller should
/// close the GUI normally only after this function succeeds.
pub fn start_update(prepared: &PreparedUpdate) -> Result<(), LauncherError> {
    let current_exe = std::env::current_exe()?;
    if !same_path(&current_exe, &prepared.target_path) {
        return Err(LauncherError::Other(
            "Целевой файл обновления не совпадает с запущенным лаунчером".into(),
        ));
    }
    if !prepared.staged_path.is_file() {
        return Err(LauncherError::Other(
            "Подготовленный файл обновления не найден".into(),
        ));
    }
    validate_internal_sibling(
        &prepared.staged_path,
        &prepared.target_path,
        InternalKind::Staged,
    )?;
    let current_hash = file_sha256(&current_exe)?;

    let mut command = Command::new(&prepared.staged_path);
    command
        .arg(APPLY_UPDATE_ARG)
        .arg(&prepared.target_path)
        .arg(std::process::id().to_string())
        .arg(current_hash);
    if let Ok(directory) = std::env::current_dir() {
        command.current_dir(directory);
    }
    configure_background_process(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| LauncherError::Other(format!("Не удалось запустить updater: {error}")))
}

/// Handles private helper arguments before eframe starts. `Some(code)` means
/// this process was the apply helper and must exit. Health/cleanup modes keep
/// launching the normal GUI.
pub fn handle_startup_args() -> Option<i32> {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mode = arguments.first()?;

    if mode == OsStr::new(APPLY_UPDATE_ARG) {
        if arguments.len() != 4 {
            return Some(2);
        }
        let target = PathBuf::from(&arguments[1]);
        let parent_pid = arguments[2]
            .to_string_lossy()
            .parse::<u32>()
            .unwrap_or_default();
        if parent_pid == 0 {
            return Some(2);
        }
        let expected_target_hash = arguments[3].to_string_lossy();
        if parse_plain_sha256(&expected_target_hash).is_none() {
            return Some(2);
        }
        return Some(
            match apply_update(&target, parent_pid, &expected_target_hash) {
                Ok(()) => 0,
                Err(error) => {
                    log_update(&target, &format!("Update failed: {error}"));
                    1
                }
            },
        );
    }

    if mode == OsStr::new(UPDATE_HEALTH_ARG) {
        if arguments.len() == 4 {
            let context = HealthContext {
                marker: PathBuf::from(&arguments[1]),
                staged: PathBuf::from(&arguments[2]),
                backup: PathBuf::from(&arguments[3]),
                target: std::env::current_exe().unwrap_or_default(),
            };
            if validate_health_context(&context).is_ok() {
                let _ = HEALTH_CONTEXT.set(context);
            }
        }
        return None;
    }

    if mode == OsStr::new(CLEANUP_UPDATE_ARG) && arguments.len() == 2 {
        let staged = PathBuf::from(&arguments[1]);
        if let Ok(target) = std::env::current_exe() {
            if validate_internal_sibling(&staged, &target, InternalKind::Staged).is_ok() {
                thread::spawn(move || retry_remove_files(&[staged], Duration::from_secs(30)));
            }
        }
    }
    None
}

/// Called from the first eframe update. It confirms that the replacement
/// process reached a usable GUI frame, then safely schedules backup cleanup.
pub fn mark_startup_healthy() {
    let Some(context) = HEALTH_CONTEXT.get().cloned() else {
        return;
    };
    if HEALTH_REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }

    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&context.marker)
    {
        Ok(mut marker) => {
            let _ = marker.write_all(b"healthy\n");
            let _ = marker.sync_all();
            log_update(&context.target, "New launcher reported healthy startup");
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(2));
                // The helper owns backup/marker until it has observed health.
                // This process only removes the helper executable after the
                // helper exits and releases the Windows image lock.
                retry_remove_files(&[context.staged], Duration::from_secs(30));
            });
        }
        Err(error) => {
            log_update(
                &context.target,
                &format!("Could not write update health marker: {error}"),
            );
        }
    }
}

fn apply_update(
    target: &Path,
    parent_pid: u32,
    expected_target_hash: &str,
) -> Result<(), LauncherError> {
    let helper = std::env::current_exe()?;
    validate_internal_sibling(&helper, target, InternalKind::Staged)?;
    if !target.is_file() {
        return Err(LauncherError::Other(format!(
            "Текущий launcher не найден: {}",
            target.display()
        )));
    }
    let _update_lock = UpdateLock::acquire(target)?;
    let actual_target_hash = file_sha256(target)?;
    if !actual_target_hash.eq_ignore_ascii_case(expected_target_hash) {
        return Err(LauncherError::Other(
            "Launcher уже изменён другим процессом обновления".into(),
        ));
    }

    let backup = internal_path(target, "backup", std::process::id())?;
    let replacement = internal_path(target, "installing", std::process::id())?;
    let marker = internal_path(target, "health", std::process::id())?.with_extension("marker");
    for path in [&backup, &replacement, &marker] {
        remove_file_if_exists(path)?;
    }

    // Prepare and fsync the complete replacement while the old launcher is
    // still present. The interval between moving target -> backup and putting
    // the new target in place is then only a single same-directory rename.
    let prepare_result = (|| -> Result<(), LauncherError> {
        fs::copy(&helper, &replacement)?;
        make_executable(&replacement)?;
        sync_file(&replacement)?;
        let source_hash = file_sha256(&helper)?;
        let copied_hash = file_sha256(&replacement)?;
        if source_hash != copied_hash {
            return Err(LauncherError::Checksum {
                path: replacement.display().to_string(),
                expected: source_hash,
                got: copied_hash,
            });
        }
        Ok(())
    })();
    if let Err(error) = prepare_result {
        let _ = fs::remove_file(&replacement);
        return Err(error);
    }

    log_update(
        target,
        &format!("Applying update; waiting for launcher PID {parent_pid}"),
    );
    if let Err(error) = wait_and_move_target_to_backup(target, &backup) {
        let _ = fs::remove_file(&replacement);
        return Err(error);
    }
    let backup_validation = (|| -> Result<(), LauncherError> {
        let backup_hash = file_sha256(&backup)?;
        if !backup_hash.eq_ignore_ascii_case(expected_target_hash) {
            return Err(LauncherError::Checksum {
                path: backup.display().to_string(),
                expected: expected_target_hash.to_string(),
                got: backup_hash,
            });
        }
        Ok(())
    })();
    if let Err(validation_error) = backup_validation {
        let _ = fs::remove_file(&replacement);
        if let Err(rollback_error) = rollback_update(target, &backup) {
            return Err(LauncherError::Other(format!(
                "{validation_error}; восстановление завершилось ошибкой: {rollback_error}"
            )));
        }
        return Err(validation_error);
    }

    let apply_result = (|| -> Result<(), LauncherError> {
        fs::rename(&replacement, target)?;
        if let Some(parent) = target.parent() {
            sync_directory(parent);
        }

        let mut command = Command::new(target);
        command
            .arg(UPDATE_HEALTH_ARG)
            .arg(&marker)
            .arg(&helper)
            .arg(&backup);
        if let Ok(directory) = std::env::current_dir() {
            command.current_dir(directory);
        }
        configure_background_process(&mut command);
        let mut child = command.spawn().map_err(|error| {
            LauncherError::Other(format!("Новая версия не запустилась: {error}"))
        })?;

        let deadline = Instant::now() + Duration::from_secs(45);
        let mut healthy_since = None;
        loop {
            if let Some(status) = child.try_wait()? {
                if marker.is_file() && status.success() {
                    log_update(
                        target,
                        "Replacement process closed normally after reporting healthy startup",
                    );
                    retry_remove_files(&[backup.clone(), marker.clone()], Duration::from_secs(5));
                    log_update(target, "Update completed");
                    return Ok(());
                }
                return Err(LauncherError::Other(format!(
                    "Новая версия завершилась до запуска интерфейса ({status})"
                )));
            }
            let now = Instant::now();
            if marker.is_file() {
                let first_seen = healthy_since.get_or_insert(now);
                if now.duration_since(*first_seen) >= HEALTH_GRACE_PERIOD {
                    log_update(
                        target,
                        "Replacement process passed startup health check and grace period",
                    );
                    retry_remove_files(&[backup.clone(), marker.clone()], Duration::from_secs(5));
                    log_update(target, "Update completed");
                    return Ok(());
                }
            }
            if healthy_since.is_none() && now >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LauncherError::Other(
                    "Новая версия не подтвердила запуск за 45 секунд".into(),
                ));
            }
            thread::sleep(Duration::from_millis(200));
        }
    })();

    if let Err(error) = apply_result {
        let _ = fs::remove_file(&replacement);
        let _ = fs::remove_file(&marker);
        if let Err(rollback_error) = rollback_update(target, &backup) {
            log_update(
                target,
                &format!("Update failed: {error}; rollback failed: {rollback_error}"),
            );
            return Err(LauncherError::Other(format!(
                "Обновление не установлено ({error}); восстановление завершилось ошибкой: {rollback_error}"
            )));
        }
        log_update(target, &format!("Rolled back update: {error}"));

        let mut command = Command::new(target);
        command.arg(CLEANUP_UPDATE_ARG).arg(&helper);
        if let Ok(directory) = std::env::current_dir() {
            command.current_dir(directory);
        }
        configure_background_process(&mut command);
        let _ = command.spawn();
        return Err(error);
    }

    Ok(())
}

fn wait_and_move_target_to_backup(target: &Path, backup: &Path) -> Result<(), LauncherError> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match fs::rename(target, backup) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() < deadline => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    return Err(error.into());
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                return Err(LauncherError::Other(format!(
                    "Не удалось освободить запущенный launcher: {error}"
                )));
            }
        }
    }
}

fn rollback_update(target: &Path, backup: &Path) -> Result<(), LauncherError> {
    if !backup.is_file() {
        return Err(LauncherError::Other(format!(
            "Резервная копия launcher не найдена: {}",
            backup.display()
        )));
    }

    let quarantine = internal_path(target, "failed", std::process::id())?;
    remove_file_if_exists(&quarantine)?;
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if !target.exists() {
            match fs::rename(backup, target) {
                Ok(()) => break,
                Err(_error) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }

        match fs::rename(target, &quarantine) {
            Ok(()) => {}
            Err(_error) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(error) => return Err(error.into()),
        }

        match fs::rename(backup, target) {
            Ok(()) => break,
            Err(backup_error) => {
                if let Err(candidate_error) = retry_rename(
                    &quarantine,
                    target,
                    Instant::now() + Duration::from_secs(10),
                ) {
                    return Err(LauncherError::Other(format!(
                        "Не удалось вернуть backup ({backup_error}) и восстановить запускаемый файл ({candidate_error})"
                    )));
                }
                if Instant::now() >= deadline {
                    return Err(LauncherError::Other(format!(
                        "Не удалось вернуть резервную копию launcher: {backup_error}"
                    )));
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    if let Some(parent) = target.parent() {
        sync_directory(parent);
    }
    retry_remove_files(&[quarantine], Duration::from_secs(5));
    Ok(())
}

fn retry_rename(from: &Path, to: &Path, deadline: Instant) -> Result<(), std::io::Error> {
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(_error) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

fn newest_stable_release(
    releases: &[GithubRelease],
    current: StableVersion,
) -> Option<&GithubRelease> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| parse_stable_tag(&release.tag_name).map(|version| (version, release)))
        .filter(|(version, _)| *version > current)
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
}

fn rolling_build_commit(
    client: &reqwest::blocking::Client,
    release: &GithubRelease,
    asset_name: &str,
) -> Result<Option<String>, LauncherError> {
    let Some(manifest_asset) = release
        .assets
        .iter()
        .find(|asset| asset.name == UPDATE_MANIFEST_ASSET)
    else {
        return Ok(None);
    };
    let binary_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| LauncherError::Other(format!("В rolling-релизе нет файла {asset_name}")))?;
    let bytes = download_verified_asset_bytes(client, manifest_asset, MAX_MANIFEST_BYTES)?;
    validate_rolling_manifest(&bytes, asset_name, binary_asset).map(Some)
}

fn download_verified_asset_bytes(
    client: &reqwest::blocking::Client,
    asset: &GithubAsset,
    maximum_size: u64,
) -> Result<Vec<u8>, LauncherError> {
    validate_https_url(&asset.browser_download_url)?;
    if asset.size == 0 || asset.size > maximum_size {
        return Err(LauncherError::Other(format!(
            "Недопустимый размер {}: {} байт",
            asset.name, asset.size
        )));
    }
    let expected_hash = parse_sha256_digest(asset.digest.as_deref().unwrap_or_default())
        .ok_or_else(|| {
            LauncherError::Other(format!(
                "GitHub не предоставил корректный SHA-256 для {}",
                asset.name
            ))
        })?;
    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .map_err(|error| LauncherError::Network(error.to_string()))?
        .error_for_status()
        .map_err(|error| LauncherError::Network(error.to_string()))?;
    if let Some(length) = response.content_length() {
        if length != asset.size {
            return Err(LauncherError::Other(format!(
                "Размер {} отличается от GitHub release metadata",
                asset.name
            )));
        }
    }

    let capacity = usize::try_from(asset.size)
        .map_err(|_| LauncherError::Other("Update manifest слишком большой".into()))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let count = response
            .read(&mut buffer)
            .map_err(|error| LauncherError::Network(error.to_string()))?;
        if count == 0 {
            break;
        }
        let next_size = (bytes.len() as u64)
            .checked_add(count as u64)
            .ok_or_else(|| LauncherError::Other("Update manifest слишком большой".into()))?;
        if next_size > asset.size || next_size > maximum_size {
            return Err(LauncherError::Other(
                "Update manifest превышает заявленный размер".into(),
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
        hasher.update(&buffer[..count]);
    }
    if bytes.len() as u64 != asset.size {
        return Err(LauncherError::Other(
            "Update manifest загружен не полностью".into(),
        ));
    }
    let actual_hash = hex::encode(hasher.finalize());
    if !actual_hash.eq_ignore_ascii_case(&expected_hash) {
        return Err(LauncherError::Checksum {
            path: asset.name.clone(),
            expected: expected_hash,
            got: actual_hash,
        });
    }
    Ok(bytes)
}

fn validate_rolling_manifest(
    bytes: &[u8],
    asset_name: &str,
    binary_asset: &GithubAsset,
) -> Result<String, LauncherError> {
    let manifest: UpdateManifest = serde_json::from_slice(bytes)
        .map_err(|error| LauncherError::Parse(format!("Некорректный update manifest: {error}")))?;
    if manifest.schema != 1 {
        return Err(LauncherError::Parse(format!(
            "Неподдерживаемая схема update manifest: {}",
            manifest.schema
        )));
    }
    let commit = normalize_full_commit(&manifest.commit).ok_or_else(|| {
        LauncherError::Parse("Update manifest не содержит полный commit SHA".into())
    })?;
    let manifest_asset = manifest.assets.get(asset_name).ok_or_else(|| {
        LauncherError::Parse(format!("Update manifest не описывает {asset_name}"))
    })?;
    let manifest_hash = parse_plain_sha256(&manifest_asset.sha256).ok_or_else(|| {
        LauncherError::Parse(format!(
            "Update manifest содержит неверный SHA-256 для {asset_name}"
        ))
    })?;
    let github_hash = parse_sha256_digest(binary_asset.digest.as_deref().unwrap_or_default())
        .ok_or_else(|| {
            LauncherError::Other(format!(
                "GitHub не предоставил корректный SHA-256 для {asset_name}"
            ))
        })?;
    if manifest_asset.size != binary_asset.size || !manifest_hash.eq_ignore_ascii_case(&github_hash)
    {
        return Err(LauncherError::Other(format!(
            "Update manifest не совпадает с release asset {asset_name}"
        )));
    }
    Ok(commit.to_ascii_lowercase())
}

fn update_info_from_release(
    release: &GithubRelease,
    asset_name: &str,
    display_version: String,
) -> Result<UpdateInfo, LauncherError> {
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| {
            LauncherError::Other(format!(
                "В релизе {} нет файла {asset_name}",
                release.tag_name
            ))
        })?;
    let sha256 =
        parse_sha256_digest(asset.digest.as_deref().unwrap_or_default()).ok_or_else(|| {
            LauncherError::Other(format!(
                "GitHub не предоставил корректный SHA-256 для {asset_name}"
            ))
        })?;
    if asset.size == 0 || asset.size > MAX_UPDATE_BYTES {
        return Err(LauncherError::Other(format!(
            "Недопустимый размер release asset: {} байт",
            asset.size
        )));
    }
    validate_https_url(&asset.browser_download_url)?;

    Ok(UpdateInfo {
        display_version,
        release_name: if release.name.trim().is_empty() {
            release.tag_name.clone()
        } else {
            release.name.clone()
        },
        tag: release.tag_name.clone(),
        download_url: asset.browser_download_url.clone(),
        size: asset.size,
        sha256,
        html_url: release.html_url.clone(),
    })
}

fn validate_update_info(update: &UpdateInfo) -> Result<(), LauncherError> {
    validate_https_url(&update.download_url)?;
    if update.size == 0 || update.size > MAX_UPDATE_BYTES {
        return Err(LauncherError::Other(
            "Недопустимый размер обновления".into(),
        ));
    }
    if parse_sha256_digest(&format!("sha256:{}", update.sha256)).is_none() {
        return Err(LauncherError::Other(
            "Некорректная SHA-256 сумма обновления".into(),
        ));
    }
    Ok(())
}

fn validate_https_url(value: &str) -> Result<(), LauncherError> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| LauncherError::Parse(format!("Некорректный URL обновления: {error}")))?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return Err(LauncherError::Other(
            "Release asset должен загружаться по HTTPS с github.com".into(),
        ));
    }
    Ok(())
}

fn parse_stable_tag(value: &str) -> Option<StableVersion> {
    let value = value
        .strip_prefix('v')
        .or_else(|| value.strip_prefix('V'))?;
    parse_stable_version(value)
}

fn parse_stable_version(value: &str) -> Option<StableVersion> {
    let value = value.split_once('+').map_or(value, |(version, _)| version);
    if value.contains('-') {
        return None;
    }
    let mut components = value.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Some(StableVersion {
        major,
        minor,
        patch,
    })
}

fn parse_sha256_digest(value: &str) -> Option<String> {
    let digest = value.strip_prefix("sha256:")?;
    parse_plain_sha256(digest).map(str::to_owned)
}

fn parse_plain_sha256(value: &str) -> Option<&str> {
    let value = value.trim();
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(value)
}

fn normalize_commit(value: &str) -> Option<&str> {
    let value = value.trim();
    if (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(value)
    } else {
        None
    }
}

fn normalize_full_commit(value: &str) -> Option<&str> {
    let value = value.trim();
    (value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(value)
}

fn commits_match(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
        || (left.len() >= 7
            && right.len() >= left.len()
            && right[..left.len()].eq_ignore_ascii_case(left))
        || (right.len() >= 7
            && left.len() >= right.len()
            && left[..right.len()].eq_ignore_ascii_case(right))
}

fn rolling_status_is_newer(status: &str) -> bool {
    status.eq_ignore_ascii_case("ahead")
}

fn platform_asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("MineLauncher-windows-x64.exe")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("MineLauncher-linux-x64")
    } else {
        None
    }
}

fn staged_path_for(target: &Path, process_id: u32) -> Result<PathBuf, LauncherError> {
    internal_path(target, "update", process_id)
}

fn internal_path(target: &Path, kind: &str, process_id: u32) -> Result<PathBuf, LauncherError> {
    let parent = target.parent().ok_or_else(|| {
        LauncherError::Other("У launcher-файла нет родительского каталога".into())
    })?;
    let stem = target
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| LauncherError::Other("Некорректное имя launcher-файла".into()))?;
    let extension = target.extension().and_then(OsStr::to_str);
    let file_name = match extension {
        Some(extension) => format!(".{stem}.{kind}-{process_id}.{extension}"),
        None => format!(".{stem}.{kind}-{process_id}"),
    };
    Ok(parent.join(file_name))
}

fn part_path_for(staged: &Path) -> Result<PathBuf, LauncherError> {
    let file_name = staged
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| LauncherError::Other("Некорректное имя временного файла".into()))?;
    Ok(staged.with_file_name(format!("{file_name}.part")))
}

#[derive(Clone, Copy)]
enum InternalKind {
    Staged,
    Backup,
    Health,
}

fn validate_internal_sibling(
    internal: &Path,
    target: &Path,
    kind: InternalKind,
) -> Result<(), LauncherError> {
    let internal_parent = canonical_parent(internal)?;
    let target_parent = canonical_parent(target)?;
    if !same_path(&internal_parent, &target_parent) {
        return Err(LauncherError::Other(
            "Временный updater-файл находится вне папки launcher".into(),
        ));
    }

    let stem = target
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| LauncherError::Other("Некорректное имя launcher-файла".into()))?;
    let file_name = internal
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| LauncherError::Other("Некорректное имя updater-файла".into()))?;
    let expected = match kind {
        InternalKind::Staged => format!(".{stem}.update-"),
        InternalKind::Backup => format!(".{stem}.backup-"),
        InternalKind::Health => format!(".{stem}.health-"),
    };
    if !file_name
        .to_ascii_lowercase()
        .starts_with(&expected.to_ascii_lowercase())
    {
        return Err(LauncherError::Other(
            "Updater отклонил небезопасное имя временного файла".into(),
        ));
    }
    Ok(())
}

fn validate_health_context(context: &HealthContext) -> Result<(), LauncherError> {
    validate_internal_sibling(&context.staged, &context.target, InternalKind::Staged)?;
    validate_internal_sibling(&context.backup, &context.target, InternalKind::Backup)?;
    validate_internal_sibling(&context.marker, &context.target, InternalKind::Health)?;
    Ok(())
}

fn canonical_parent(path: &Path) -> Result<PathBuf, LauncherError> {
    path.parent()
        .ok_or_else(|| LauncherError::Other("Путь не содержит родительскую папку".into()))?
        .canonicalize()
        .map_err(LauncherError::from)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

struct UpdateLock {
    file: File,
}

impl UpdateLock {
    fn acquire(target: &Path) -> Result<Self, LauncherError> {
        let path = update_lock_path(target)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        lock_file_exclusive(&file).map_err(|error| {
            LauncherError::Other(format!(
                "Не удалось заблокировать параллельное обновление {}: {error}",
                target.display()
            ))
        })?;
        Ok(Self { file })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
    }
}

fn update_lock_path(target: &Path) -> Result<PathBuf, LauncherError> {
    let parent = target.parent().ok_or_else(|| {
        LauncherError::Other("У launcher-файла нет родительского каталога".into())
    })?;
    let stem = target
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| LauncherError::Other("Некорректное имя launcher-файла".into()))?;
    Ok(parent.join(format!(".{stem}.update.lock")))
}

#[cfg(windows)]
fn lock_file_exclusive(file: &File) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "LockFileEx"]
        fn lock_file_ex(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    let result = unsafe {
        lock_file_ex(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn unlock_file(file: &File) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "UnlockFileEx"]
        fn unlock_file_ex(
            file: *mut c_void,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    let result = unsafe { unlock_file_ex(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    extern "C" {
        #[link_name = "flock"]
        fn system_flock(file: i32, operation: i32) -> i32;
    }

    const LOCK_EXCLUSIVE: i32 = 2;
    let result = unsafe { system_flock(file.as_raw_fd(), LOCK_EXCLUSIVE) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    extern "C" {
        #[link_name = "flock"]
        fn system_flock(file: i32, operation: i32) -> i32;
    }

    const LOCK_UNLOCK: i32 = 8;
    let result = unsafe { system_flock(file.as_raw_fd(), LOCK_UNLOCK) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(windows, unix)))]
fn lock_file_exclusive(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn unlock_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

fn validate_executable_header(path: &Path) -> Result<(), LauncherError> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 4];
    file.read_exact(&mut header)?;
    if cfg!(target_os = "windows") && &header[..2] != b"MZ" {
        return Err(LauncherError::Other(
            "Загруженный файл не является Windows executable".into(),
        ));
    }
    if cfg!(target_os = "linux") && header != *b"\x7fELF" {
        return Err(LauncherError::Other(
            "Загруженный файл не является Linux executable".into(),
        ));
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, LauncherError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sync_file(path: &Path) -> Result<(), LauncherError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), LauncherError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), LauncherError> {
    Ok(())
}

fn sync_directory(path: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn remove_file_if_exists(path: &Path) -> Result<(), LauncherError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn retry_remove_files(paths: &[PathBuf], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut remaining = paths.to_vec();
    while !remaining.is_empty() && Instant::now() < deadline {
        remaining.retain(|path| match fs::remove_file(path) {
            Ok(()) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        });
        if !remaining.is_empty() {
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn configure_background_process(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn log_update(target: &Path, message: &str) {
    let log_path = target.with_extension("update.log");
    if let Ok(mut log) = OpenOptions::new().create(true).append(true).open(log_path) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let _ = writeln!(log, "[{timestamp}] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "mine-launcher-updater-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn release(tag: &str, prerelease: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            name: format!("Release {tag}"),
            draft: false,
            prerelease,
            html_url: format!("https://github.com/{REPOSITORY}/releases/tag/{tag}"),
            assets: vec![GithubAsset {
                name: "MineLauncher-windows-x64.exe".into(),
                browser_download_url: format!(
                    "https://github.com/{REPOSITORY}/releases/download/{tag}/MineLauncher-windows-x64.exe"
                ),
                size: 10_000_000,
                digest: Some(format!("sha256:{HASH}")),
            }],
        }
    }

    #[test]
    fn parses_only_stable_semver_triplets() {
        assert_eq!(
            parse_stable_tag("v2.10.3"),
            Some(StableVersion {
                major: 2,
                minor: 10,
                patch: 3
            })
        );
        assert_eq!(parse_stable_version("1.0.0+build.7").unwrap().patch, 0);
        assert!(parse_stable_tag("latest").is_none());
        assert!(parse_stable_tag("v1.2.0-beta.1").is_none());
        assert!(parse_stable_tag("v1.2").is_none());
    }

    #[test]
    fn stable_selection_chooses_highest_newer_release() {
        let releases = vec![
            release("v1.1.0", false),
            release("v2.0.0", false),
            release("v3.0.0", true),
            release("v1.0.0", false),
        ];
        let selected = newest_stable_release(
            &releases,
            StableVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
        )
        .unwrap();
        assert_eq!(selected.tag_name, "v2.0.0");
    }

    #[test]
    fn stable_selection_never_downgrades() {
        let releases = vec![release("v1.9.9", false), release("v2.0.0", false)];
        assert!(newest_stable_release(
            &releases,
            StableVersion {
                major: 2,
                minor: 1,
                patch: 0,
            }
        )
        .is_none());
    }

    #[test]
    fn rolling_channel_accepts_only_ahead_comparison() {
        assert!(rolling_status_is_newer("ahead"));
        assert!(!rolling_status_is_newer("identical"));
        assert!(!rolling_status_is_newer("behind"));
        assert!(!rolling_status_is_newer("diverged"));
        assert!(commits_match(
            "4bfe084",
            "4bfe084dca88b1748d7ed09e816d6e529f5b3153"
        ));
    }

    #[test]
    fn parses_and_requires_github_sha256_digest() {
        assert_eq!(
            parse_sha256_digest(&format!("sha256:{HASH}")),
            Some(HASH.into())
        );
        assert!(parse_sha256_digest(HASH).is_none());
        assert!(parse_sha256_digest("sha256:abcd").is_none());
        assert!(parse_sha256_digest(&format!("sha1:{HASH}")).is_none());
    }

    #[test]
    fn rolling_manifest_links_commit_to_exact_release_asset() {
        let asset = GithubAsset {
            name: "MineLauncher-windows-x64.exe".into(),
            browser_download_url: "https://github.com/owner/repo/file.exe".into(),
            size: 10_000_000,
            digest: Some(format!("sha256:{HASH}")),
        };
        let commit = "abcdef0123456789abcdef0123456789abcdef01";
        let json = format!(
            r#"{{
                "schema": 1,
                "commit": "{commit}",
                "assets": {{
                    "MineLauncher-windows-x64.exe": {{
                        "sha256": "{HASH}",
                        "size": 10000000
                    }}
                }}
            }}"#
        );

        assert_eq!(
            validate_rolling_manifest(json.as_bytes(), "MineLauncher-windows-x64.exe", &asset)
                .unwrap(),
            commit
        );

        let mismatched = GithubAsset {
            size: 10_000_001,
            ..asset
        };
        assert!(validate_rolling_manifest(
            json.as_bytes(),
            "MineLauncher-windows-x64.exe",
            &mismatched
        )
        .is_err());
    }

    #[test]
    fn parses_release_json_and_selects_exact_asset() {
        let json = format!(
            r#"[{{
                "tag_name":"v1.2.0",
                "name":"MineLauncher v1.2.0",
                "draft":false,
                "prerelease":false,
                "html_url":"https://github.com/{REPOSITORY}/releases/tag/v1.2.0",
                "assets":[{{
                    "name":"MineLauncher-windows-x64.exe",
                    "browser_download_url":"https://github.com/{REPOSITORY}/releases/download/v1.2.0/MineLauncher-windows-x64.exe",
                    "size":12345,
                    "digest":"sha256:{HASH}"
                }}]
            }}]"#
        );
        let releases: Vec<GithubRelease> = serde_json::from_str(&json).unwrap();
        let info = update_info_from_release(
            &releases[0],
            "MineLauncher-windows-x64.exe",
            "v1.2.0".into(),
        )
        .unwrap();
        assert_eq!(info.tag, "v1.2.0");
        assert_eq!(info.size, 12_345);
        assert_eq!(info.sha256, HASH);
    }

    #[test]
    fn rejects_non_github_or_non_https_asset_urls() {
        assert!(validate_https_url("http://github.com/file.exe").is_err());
        assert!(validate_https_url("https://example.com/file.exe").is_err());
        assert!(validate_https_url("https://github.com/file.exe").is_ok());
    }

    #[test]
    fn internal_paths_stay_next_to_unicode_target_and_reject_other_directories() {
        let directory = TestDirectory::new("paths");
        let other = directory.0.join("other");
        fs::create_dir_all(&other).unwrap();
        let target = directory.0.join("Мой Mine Launcher.exe");
        fs::write(&target, b"old").unwrap();
        let staged = staged_path_for(&target, 42).unwrap();

        assert_eq!(staged.parent(), target.parent());
        assert!(staged
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(".update-42.exe"));
        validate_internal_sibling(&staged, &target, InternalKind::Staged).unwrap();

        let escaped = other.join(staged.file_name().unwrap());
        assert!(validate_internal_sibling(&escaped, &target, InternalKind::Staged).is_err());
    }

    #[test]
    fn rollback_restores_backup_without_leaving_candidate() {
        let directory = TestDirectory::new("rollback");
        let target = directory.0.join("MineLauncher.exe");
        let backup = directory.0.join(".MineLauncher.backup-42.exe");
        fs::write(&target, b"broken candidate").unwrap();
        fs::write(&backup, b"known good launcher").unwrap();

        rollback_update(&target, &backup).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"known good launcher");
        assert!(!backup.exists());
    }
}
