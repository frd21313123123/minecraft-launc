use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use image::GenericImageView;
use reqwest::blocking::{multipart, Client};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::config::{AccountConfig, SkinModel};
use crate::paths::app_dir;

const CLIENT_MOD_FILE: &str = "minelauncher-skin-sync-neoforge-1.21.1.jar";
const REQUEST_FILE: &str = "minelauncher-skin-sync.json";
const CACHE_FILE: &str = "skin-upload-cache.json";
const MINESKIN_BASE: &str = "https://api.mineskin.org/v2";
const USER_AGENT: &str = "MineLauncher/1.0 SkinSync";
const MAX_SKIN_BYTES: usize = 3 * 1024 * 1024;
const CLIENT_MOD_BYTES: &[u8] =
    include_bytes!("../assets/minelauncher-skin-sync-neoforge-1.21.1.jar");

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
    command: &'a str,
    fingerprint: &'a str,
    server_address: &'a str,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct UploadCache {
    version: u8,
    skins: HashMap<String, String>,
}

pub fn prepare_for_launch(
    account: &AccountConfig,
    game_dir: &Path,
    server_address: &str,
) -> Result<SkinSyncOutcome, String> {
    install_client_mod(game_dir)?;
    write_disabled_request(game_dir, &account.username, server_address)?;

    let source = account.skin_source.trim();
    if source.is_empty() {
        return Ok(SkinSyncOutcome::Disabled);
    }

    let (command, fingerprint) = if is_web_url(source) {
        (
            url_command(source, account.skin_model)?,
            fingerprint(source.as_bytes(), account.skin_model),
        )
    } else if is_local_source(source) {
        let data = read_and_validate_skin(Path::new(source))?;
        let fingerprint = fingerprint(&data, account.skin_model);
        let url = cached_or_upload_skin(&fingerprint, data, account.skin_model)?;
        (url_command(&url, account.skin_model)?, fingerprint)
    } else {
        (
            name_command(source)?,
            fingerprint(source.as_bytes(), account.skin_model),
        )
    };

    let request = SyncRequest {
        version: 1,
        enabled: true,
        username: account.username.trim(),
        command: &command,
        fingerprint: &fingerprint,
        server_address: server_address.trim(),
    };
    write_request(game_dir, &request)?;
    Ok(SkinSyncOutcome::Ready)
}

fn install_client_mod(game_dir: &Path) -> Result<(), String> {
    let mods_dir = game_dir.join("mods");
    fs::create_dir_all(&mods_dir)
        .map_err(|error| format!("Не удалось создать папку модов: {error}"))?;
    let path = mods_dir.join(CLIENT_MOD_FILE);
    if fs::read(&path).ok().as_deref() == Some(CLIENT_MOD_BYTES) {
        return Ok(());
    }
    fs::write(&path, CLIENT_MOD_BYTES)
        .map_err(|error| format!("Не удалось установить мод синхронизации скина: {error}"))
}

fn write_disabled_request(
    game_dir: &Path,
    username: &str,
    server_address: &str,
) -> Result<(), String> {
    let request = SyncRequest {
        version: 1,
        enabled: false,
        username: username.trim(),
        command: "",
        fingerprint: "",
        server_address: server_address.trim(),
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

fn cached_or_upload_skin(
    fingerprint: &str,
    data: Vec<u8>,
    model: SkinModel,
) -> Result<String, String> {
    let mut cache = load_cache();
    if let Some(url) = cache.skins.get(fingerprint).filter(|url| !url.is_empty()) {
        return Ok(url.clone());
    }

    let url = upload_skin(data, model)?;
    cache.version = 1;
    cache.skins.insert(fingerprint.to_string(), url.clone());
    save_cache(&cache)?;
    Ok(url)
}

fn upload_skin(data: Vec<u8>, model: SkinModel) -> Result<String, String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|error| format!("Не удалось запустить загрузчик скина: {error}"))?;
    let part = multipart::Part::bytes(data)
        .file_name("skin.png")
        .mime_str("image/png")
        .map_err(|error| format!("Не удалось подготовить PNG: {error}"))?;
    let form = multipart::Form::new()
        .part("file", part)
        .text("variant", model.command_value().to_string());

    let value = request_json(
        client
            .post(format!("{MINESKIN_BASE}/queue"))
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .multipart(form)
            .send(),
    )?;
    if let Some(url) = skin_url(&value) {
        return validate_remote_url(url);
    }

    let job_id = value
        .pointer("/job/id")
        .and_then(Value::as_str)
        .ok_or_else(|| api_error(&value, "MineSkin не вернул скин или номер задания"))?
        .to_string();

    for _ in 0..40 {
        thread::sleep(Duration::from_millis(1200));
        let value = request_json(
            client
                .get(format!("{MINESKIN_BASE}/queue/{job_id}"))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .send(),
        )?;
        if let Some(url) = skin_url(&value) {
            return validate_remote_url(url);
        }
        match value.pointer("/job/status").and_then(Value::as_str) {
            Some("failed") => return Err(api_error(&value, "MineSkin не смог обработать PNG")),
            _ => {}
        }
    }

    Err("MineSkin слишком долго обрабатывает PNG. Попробуйте запустить игру ещё раз.".into())
}

fn request_json(
    response: Result<reqwest::blocking::Response, reqwest::Error>,
) -> Result<Value, String> {
    let response = response.map_err(|error| format!("Не удалось связаться с MineSkin: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| format!("Не удалось прочитать ответ MineSkin: {error}"))?;
    let value: Value = serde_json::from_str(&body).map_err(|_| {
        format!(
            "MineSkin вернул некорректный ответ (HTTP {}): {}",
            status.as_u16(),
            truncate(&body, 180)
        )
    })?;
    if !status.is_success() {
        return Err(api_error(
            &value,
            &format!("Ошибка MineSkin (HTTP {})", status.as_u16()),
        ));
    }
    if value.get("success").and_then(Value::as_bool) == Some(false) || value.get("error").is_some()
    {
        return Err(api_error(&value, "MineSkin отклонил PNG"));
    }
    Ok(value)
}

fn skin_url(value: &Value) -> Option<&str> {
    value
        .pointer("/skin/texture/url/skin")
        .or_else(|| value.pointer("/skin/url/skin"))
        .or_else(|| value.pointer("/skin/url"))
        .and_then(Value::as_str)
}

fn validate_remote_url(url: &str) -> Result<String, String> {
    let url = url.trim().replace('"', "");
    if url.starts_with("https://") || url.starts_with("skinsrestorer-axolotl://") {
        Ok(url)
    } else {
        Err("Сервис загрузки вернул неподдерживаемую ссылку на скин".into())
    }
}

fn api_error(value: &Value, fallback: &str) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/errors/0/message").and_then(Value::as_str))
        .or_else(|| value.pointer("/job/error").and_then(Value::as_str))
        .map(|message| format!("{fallback}: {message}"))
        .unwrap_or_else(|| fallback.to_string())
}

fn read_and_validate_skin(path: &Path) -> Result<Vec<u8>, String> {
    if !path.is_file() {
        return Err(format!("PNG-скин не найден: {}", path.display()));
    }
    let data = fs::read(path).map_err(|error| format!("Не удалось прочитать PNG-скин: {error}"))?;
    if data.len() > MAX_SKIN_BYTES {
        return Err("PNG-скин должен быть меньше 3 МБ".into());
    }
    let image = image::load_from_memory_with_format(&data, image::ImageFormat::Png)
        .map_err(|_| "Выбранный файл не является корректным PNG-скином".to_string())?;
    let dimensions = image.dimensions();
    if dimensions != (64, 64) && dimensions != (64, 32) {
        return Err(format!(
            "Неверный размер скина: {}×{}. Нужен PNG 64×64 или 64×32",
            dimensions.0, dimensions.1
        ));
    }
    Ok(data)
}

fn url_command(url: &str, model: SkinModel) -> Result<String, String> {
    let url = url.trim().replace('"', "");
    if !(is_web_url(&url) || url.starts_with("skinsrestorer-axolotl://")) {
        return Err("Некорректная ссылка на скин".into());
    }
    Ok(format!("/skin url \"{url}\" {}", model.command_value()))
}

fn name_command(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > 16
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(
            "Имя скина должно быть ником Minecraft: до 16 латинских букв, цифр или _".into(),
        );
    }
    Ok(format!("/skin set {name}"))
}

fn is_web_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

fn is_local_source(source: &str) -> bool {
    !source.is_empty()
        && !is_web_url(source)
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

fn cache_path() -> PathBuf {
    app_dir().join(CACHE_FILE)
}

fn load_cache() -> UploadCache {
    fs::read_to_string(cache_path())
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &UploadCache) -> Result<(), String> {
    fs::create_dir_all(app_dir())
        .map_err(|error| format!("Не удалось создать кэш скинов: {error}"))?;
    let data = serde_json::to_vec_pretty(cache)
        .map_err(|error| format!("Не удалось сохранить кэш скинов: {error}"))?;
    fs::write(cache_path(), data)
        .map_err(|error| format!("Не удалось сохранить кэш скинов: {error}"))
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let text: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{text}…")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_safe_skin_commands() {
        assert_eq!(name_command("Notch").unwrap(), "/skin set Notch");
        assert_eq!(
            url_command("https://example.com/skin.png", SkinModel::Slim).unwrap(),
            "/skin url \"https://example.com/skin.png\" slim"
        );
        assert!(name_command("Notch other-player").is_err());
        assert!(url_command("file:///skin.png", SkinModel::Classic).is_err());
    }

    #[test]
    fn fingerprints_include_the_model() {
        assert_ne!(
            fingerprint(b"same png", SkinModel::Classic),
            fingerprint(b"same png", SkinModel::Slim)
        );
    }

    #[test]
    fn prepares_client_mod_and_request_without_network() {
        let game_dir = std::env::temp_dir().join(format!(
            "minelauncher-skin-sync-test-{}",
            std::process::id()
        ));
        let account = AccountConfig {
            username: "TestPlayer".into(),
            skin_source: "https://example.com/skin.png".into(),
            skin_model: SkinModel::Classic,
        };

        let outcome =
            prepare_for_launch(&account, &game_dir, "play.example.com").expect("prepare sync");
        assert_eq!(outcome, SkinSyncOutcome::Ready);
        assert_eq!(
            fs::read(game_dir.join("mods").join(CLIENT_MOD_FILE)).unwrap(),
            CLIENT_MOD_BYTES
        );
        let request: Value =
            serde_json::from_slice(&fs::read(game_dir.join(REQUEST_FILE)).unwrap()).unwrap();
        assert_eq!(request["enabled"], true);
        assert_eq!(request["username"], "TestPlayer");
        assert_eq!(request["serverAddress"], "play.example.com");
        assert_eq!(
            request["command"],
            "/skin url \"https://example.com/skin.png\" classic"
        );

        let _ = fs::remove_dir_all(game_dir);
    }

    #[test]
    #[ignore = "uses the public MineSkin generation API"]
    fn uploads_skin_through_mineskin() {
        let url = upload_skin(
            include_bytes!("../assets/minecraft/textures/entity/player/wide/steve.png").to_vec(),
            SkinModel::Classic,
        )
        .expect("MineSkin upload");
        assert!(url.starts_with("https://textures.minecraft.net/texture/"));
    }
}
