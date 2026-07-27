use sha1::{Digest, Sha1};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use crate::error::LauncherError;

pub type ProgressFn = Arc<dyn Fn(u64, u64, &str) + Send + Sync>;

pub fn http_client() -> Result<reqwest::blocking::Client, LauncherError> {
    reqwest::blocking::Client::builder()
        .user_agent(format!(
            "MineLauncher/{} (Rust)",
            crate::paths::LAUNCHER_VERSION
        ))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| LauncherError::Network(e.to_string()))
}

pub fn download_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<T, LauncherError> {
    let resp = client
        .get(url)
        .send()
        .map_err(|e| LauncherError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| LauncherError::Network(e.to_string()))?;
    resp.json().map_err(|e| LauncherError::Parse(e.to_string()))
}

/// Скачивает файл, если его нет или sha1 не совпадает.
pub fn download_file(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    expected_sha1: Option<&str>,
    progress: Option<&ProgressFn>,
    label: &str,
) -> Result<(), LauncherError> {
    let mut replace_existing = false;
    if dest.exists() {
        if let Some(sha) = expected_sha1 {
            if verify_sha1(dest, sha)? {
                return Ok(());
            }
            replace_existing = true;
        } else {
            return Ok(());
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| LauncherError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| LauncherError::Network(e.to_string()))?;

    let total = resp.content_length().unwrap_or(0);
    let tmp = dest.with_extension("part");
    let mut file = File::create(&tmp)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0u64;

    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| LauncherError::Network(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if let Some(cb) = progress {
            // total == 0 → размер неизвестен (не подставлять done, иначе UI всегда 100%).
            cb(done, total, label);
        }
    }
    file.flush()?;
    drop(file);

    let hash = hex::encode(hasher.finalize());
    if let Some(sha) = expected_sha1 {
        if !sha.eq_ignore_ascii_case(&hash) {
            let _ = fs::remove_file(&tmp);
            return Err(LauncherError::Checksum {
                path: dest.display().to_string(),
                expected: sha.to_string(),
                got: hash,
            });
        }
    }

    // Windows does not replace an existing destination with `rename`. Only remove
    // the old file after the replacement has been fully downloaded and verified.
    if replace_existing {
        fs::remove_file(dest)?;
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

pub fn verify_sha1(path: &Path, expected: &str) -> Result<bool, LauncherError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = hex::encode(hasher.finalize());
    Ok(hash.eq_ignore_ascii_case(expected))
}
