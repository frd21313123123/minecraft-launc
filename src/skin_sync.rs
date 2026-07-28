use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use image::GenericImageView;
use reqwest::blocking::Client;
use serde::Serialize;
use sha1::{Digest, Sha1};

use crate::config::{AccountConfig, SkinModel};

const MOD_FILE: &str = "minelauncher-skin-sync-neoforge-1.21.1.jar";
const REQUEST_FILE: &str = "minelauncher-skin-sync.json";
const SKIN_FILE: &str = "minelauncher-skin.png";
const MAX_SKIN_BYTES: usize = 256 * 1024;
const MOD_BYTES: &[u8] = include_bytes!("../assets/minelauncher-skin-sync-neoforge-1.21.1.jar");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkinSyncOutcome {
    Disabled,
    Ready,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncRequest<'a> {
    version: u8,
    enabled: bool,
    username: &'a str,
    model: &'a str,
    fingerprint: &'a str,
}

pub fn prepare_for_launch(
    account: &AccountConfig,
    game_dir: &Path,
) -> Result<SkinSyncOutcome, String> {
    install_mod(game_dir)?;
    write_disabled_request(game_dir, &account.username)?;

    let source = account.skin_source.trim();
    if source.is_empty() {
        return Ok(SkinSyncOutcome::Disabled);
    }

    let data = if source.starts_with("https://") {
        download_skin(source)?
    } else if is_local_source(source) {
        fs::read(source).map_err(|error| format!("Не удалось прочитать PNG-скин: {error}"))?
    } else {
        return Err("Выберите локальный PNG 64×64 или укажите прямую HTTPS-ссылку на PNG".into());
    };
    validate_skin(&data)?;

    let fingerprint = fingerprint(&data, account.skin_model);
    fs::write(game_dir.join(SKIN_FILE), &data)
        .map_err(|error| format!("Не удалось подготовить PNG для мода: {error}"))?;

    let request = SyncRequest {
        version: 2,
        enabled: true,
        username: account.username.trim(),
        model: account.skin_model.command_value(),
        fingerprint: &fingerprint,
    };
    write_request(game_dir, &request)?;
    Ok(SkinSyncOutcome::Ready)
}

fn install_mod(game_dir: &Path) -> Result<(), String> {
    let mods_dir = game_dir.join("mods");
    fs::create_dir_all(&mods_dir)
        .map_err(|error| format!("Не удалось создать папку модов: {error}"))?;
    let path = mods_dir.join(MOD_FILE);
    if fs::read(&path).ok().as_deref() == Some(MOD_BYTES) {
        return Ok(());
    }
    fs::write(&path, MOD_BYTES)
        .map_err(|error| format!("Не удалось установить мод синхронизации скина: {error}"))
}

fn write_disabled_request(game_dir: &Path, username: &str) -> Result<(), String> {
    let request = SyncRequest {
        version: 2,
        enabled: false,
        username: username.trim(),
        model: SkinModel::Classic.command_value(),
        fingerprint: "",
    };
    write_request(game_dir, &request)
}

fn write_request(game_dir: &Path, request: &SyncRequest<'_>) -> Result<(), String> {
    fs::create_dir_all(game_dir)
        .map_err(|error| format!("Не удалось подготовить игровую папку: {error}"))?;
    let data = serde_json::to_vec_pretty(request)
        .map_err(|error| format!("Не удалось подготовить запрос скина: {error}"))?;
    fs::write(game_dir.join(REQUEST_FILE), data)
        .map_err(|error| format!("Не удалось записать запрос синхронизации скина: {error}"))
}

fn download_skin(url: &str) -> Result<Vec<u8>, String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .https_only(true)
        .build()
        .map_err(|error| format!("Не удалось запустить загрузчик скина: {error}"))?;
    let response = client
        .get(url)
        .send()
        .map_err(|error| format!("Не удалось скачать PNG-скин: {error}"))?
        .error_for_status()
        .map_err(|error| format!("Сервер не отдал PNG-скин: {error}"))?;

    let mut data = Vec::new();
    response
        .take((MAX_SKIN_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|error| format!("Не удалось прочитать загруженный PNG: {error}"))?;
    if data.len() > MAX_SKIN_BYTES {
        return Err("PNG-скин должен быть меньше 256 КБ".into());
    }
    Ok(data)
}

fn validate_skin(data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        return Err("PNG-скин пуст".into());
    }
    if data.len() > MAX_SKIN_BYTES {
        return Err("PNG-скин должен быть меньше 256 КБ".into());
    }
    let image = image::load_from_memory_with_format(data, image::ImageFormat::Png)
        .map_err(|_| "Выбранный файл не является корректным PNG-скином".to_string())?;
    let dimensions = image.dimensions();
    if dimensions != (64, 64) {
        return Err(format!(
            "Неверный размер скина: {}×{}. Нужен современный PNG 64×64",
            dimensions.0, dimensions.1
        ));
    }
    Ok(())
}

fn is_local_source(source: &str) -> bool {
    !source.is_empty()
        && !source.contains("://")
        && (Path::new(source).is_absolute()
            || source.to_ascii_lowercase().ends_with(".png")
            || source.contains('\\')
            || source.contains('/'))
}

fn fingerprint(data: &[u8], model: SkinModel) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.update([match model {
        SkinModel::Classic => 0,
        SkinModel::Slim => 1,
    }]);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn fingerprints_include_the_model() {
        assert_ne!(
            fingerprint(b"same png", SkinModel::Classic),
            fingerprint(b"same png", SkinModel::Slim)
        );
    }

    #[test]
    fn rejects_non_png_and_legacy_dimensions() {
        assert!(validate_skin(b"not a png").is_err());
        let legacy = image::RgbaImage::new(64, 32);
        let mut bytes = Vec::new();
        legacy
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert!(validate_skin(&bytes).is_err());
    }

    #[test]
    fn prepares_mod_png_and_request_without_network() {
        let game_dir = std::env::temp_dir().join(format!(
            "minelauncher-skin-sync-test-{}",
            std::process::id()
        ));
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/minecraft/textures/entity/player/wide/steve.png");
        let account = AccountConfig {
            username: "TestPlayer".into(),
            skin_source: source.to_string_lossy().to_string(),
            skin_model: SkinModel::Classic,
        };

        let outcome = prepare_for_launch(&account, &game_dir).expect("prepare sync");
        assert_eq!(outcome, SkinSyncOutcome::Ready);
        assert_eq!(
            fs::read(game_dir.join("mods").join(MOD_FILE)).unwrap(),
            MOD_BYTES
        );
        assert_eq!(
            fs::read(game_dir.join(SKIN_FILE)).unwrap(),
            fs::read(source).unwrap()
        );

        let request: Value =
            serde_json::from_slice(&fs::read(game_dir.join(REQUEST_FILE)).unwrap()).unwrap();
        assert_eq!(request["version"], 2);
        assert_eq!(request["enabled"], true);
        assert_eq!(request["username"], "TestPlayer");
        assert_eq!(request["model"], "classic");
        assert!(request["fingerprint"].as_str().unwrap().len() == 40);

        let _ = fs::remove_dir_all(game_dir);
    }
}
