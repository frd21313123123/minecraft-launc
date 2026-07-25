//! Установка NeoForge через официальный installer (как Prism).

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::download::{self, download_file, ProgressFn};
use crate::error::LauncherError;
use crate::java::find_java;
use crate::paths::{ensure_dirs, game_dir, versions_dir};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn neoforge_version_id(neoforge_ver: &str) -> String {
    format!("neoforge-{neoforge_ver}")
}

pub fn is_neoforge_installed(neoforge_ver: &str) -> bool {
    let id = neoforge_version_id(neoforge_ver);
    let dir = versions_dir().join(&id);
    let json = dir.join(format!("{id}.json"));
    json.is_file()
}

/// Скачивает installer и ставит клиент NeoForge в общий game dir лаунчера.
pub fn install_neoforge(
    neoforge_ver: &str,
    java_path: &str,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<String, LauncherError> {
    ensure_dirs()?;
    let version_id = neoforge_version_id(neoforge_ver);

    if is_neoforge_installed(neoforge_ver) {
        progress(1, 1, &format!("NeoForge {neoforge_ver} уже установлен"));
        return Ok(version_id);
    }

    let java = find_java(java_path).ok_or(LauncherError::JavaNotFound)?;
    let client = download::http_client()?;

    let installer_name = format!("neoforge-{neoforge_ver}-installer.jar");
    let installer_url = format!(
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/{neoforge_ver}/{installer_name}"
    );
    let installer_path = crate::paths::app_dir()
        .join("cache")
        .join(&installer_name);
    if let Some(parent) = installer_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    progress(0, 1, &format!("Скачивание NeoForge installer {neoforge_ver}…"));
    download_file(
        &client,
        &installer_url,
        &installer_path,
        None,
        Some(&progress),
        &installer_name,
    )?;

    if cancel.load(Ordering::Relaxed) {
        return Err(LauncherError::Other("Отменено".into()));
    }

    // installClient пишет в versions/ и libraries/ целевого .minecraft
    let target = game_dir();
    std::fs::create_dir_all(&target)?;
    // Официальный installer требует launcher_profiles.json (как у лаунчера Mojang).
    ensure_launcher_profiles(&target)?;

    progress(0, 1, &format!("Установка NeoForge {neoforge_ver} (это может занять несколько минут)…"));

    let mut cmd = Command::new(&java);
    cmd.arg("-jar")
        .arg(&installer_path)
        .arg("--installClient")
        .arg(&target)
        .current_dir(&target);

    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .map_err(|e| LauncherError::Other(format!("Не удалось запустить installer: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = format!(
            "Installer NeoForge завершился с ошибкой.\n{}\n{}",
            stdout.trim(),
            stderr.trim()
        );
        return Err(LauncherError::Other(msg.chars().take(800).collect()));
    }

    if !is_neoforge_installed(neoforge_ver) {
        // Иногда id чуть другой — поищем
        if let Some(found) = find_neoforge_version_dir(neoforge_ver) {
            progress(1, 1, &format!("NeoForge установлен: {found}"));
            return Ok(found);
        }
        return Err(LauncherError::Other(format!(
            "Installer отработал, но версия {version_id} не найдена в {}",
            versions_dir().display()
        )));
    }

    progress(1, 1, &format!("NeoForge {neoforge_ver} готов"));
    Ok(version_id)
}

fn find_neoforge_version_dir(neoforge_ver: &str) -> Option<String> {
    let dir = versions_dir();
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains(neoforge_ver) && name.to_lowercase().contains("neoforge") {
            let json = e.path().join(format!("{name}.json"));
            if json.is_file() {
                return Some(name);
            }
        }
    }
    None
}

fn ensure_launcher_profiles(mc_dir: &std::path::Path) -> Result<(), LauncherError> {
    let path = mc_dir.join("launcher_profiles.json");
    if path.is_file() {
        return Ok(());
    }
    let json = r#"{
  "profiles": {
    "default": {
      "name": "default",
      "type": "custom",
      "lastVersionId": "latest-release"
    }
  },
  "selectedProfile": "default",
  "clientToken": "00000000-0000-0000-0000-000000000000",
  "launcherVersion": {
    "name": "MineLauncher",
    "format": 21
  }
}
"#;
    std::fs::write(path, json)?;
    Ok(())
}


