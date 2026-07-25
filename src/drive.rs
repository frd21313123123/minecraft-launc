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

use serde::Deserialize;
use zip::ZipArchive;

use crate::download::{self, ProgressFn};
use crate::error::LauncherError;
use crate::paths::{builds_dir, ensure_dirs, instances_dir};

/// Папка со сборками на Google Drive.
pub const DRIVE_FOLDER_ID: &str = "1mEl5hfZqx5IUiS_gULZBz4v116YuHYtq";

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
    /// Базовая версия Minecraft из builds.json (если указана).
    pub minecraft: Option<String>,
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
                // Подтянуть file_id по filename, если не указан.
                for b in &mut builds {
                    if b.file_id.is_empty() {
                        if let Some(f) = files.iter().find(|f| f.name == b.filename) {
                            b.file_id = f.id.clone();
                            b.size = f.size.or(b.size);
                        }
                    }
                }
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
                minecraft: None,
            }
        })
        .collect();

    builds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(builds)
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
            let id = b
                .id
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem_id(&filename));
            BuildInfo {
                id,
                name: b.name,
                file_id: b.file_id,
                filename,
                size: b.size,
                minecraft: b.minecraft,
            }
        })
        .collect())
}

/// Скачивает и распаковывает сборку в `instances/{id}/`.
/// Возвращает путь к инстансу и опциональные метаданные.
pub fn install_build(
    build: &BuildInfo,
    progress: ProgressFn,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(PathBuf, Option<BuildMeta>), LauncherError> {
    ensure_dirs()?;
    let client = download::http_client()?;

    let cache_zip = builds_dir().join(&build.filename);
    progress(0, 1, &format!("Скачивание «{}»…", build.name));

    download_drive_file(
        &client,
        &build.file_id,
        &cache_zip,
        Some(&progress),
        &build.name,
    )?;

    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(LauncherError::Other("Отменено".into()));
    }

    progress(0, 1, "Распаковка сборки…");
    let dest = instances_dir().join(&build.id);
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    fs::create_dir_all(&dest)?;
    extract_zip(&cache_zip, &dest)?;

    let meta = read_build_meta(&dest);
    progress(1, 1, &format!("Сборка «{}» готова", build.name));
    Ok((dest, meta))
}

pub fn instance_dir(build_id: &str) -> PathBuf {
    instances_dir().join(build_id)
}

pub fn is_build_installed(build_id: &str) -> bool {
    let dir = instance_dir(build_id);
    if !dir.is_dir() {
        return false;
    }
    // Есть хоть что-то, кроме пустой папки.
    fs::read_dir(&dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
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

    // Типичный фрагмент: ["MyPack.zip",null,"application/zip",... ,"FILE_ID",...]
    // Ищем пары name + mime + id.
    // Паттерн: "filename.ext" рядом с mime и 25–44-символьным id.
    let re_entry = regex_lite::Regex::new(
        r#"\["([^"]+\.(?:zip|json|jar|txt|mrpack))",null,"(application/[^"]+|text/[^"]+)"[^]]*?,"([a-zA-Z0-9_-]{25,44})""#,
    )
    .ok();

    if let Some(re) = re_entry {
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
                });
            }
        }
    }

    // Более свободный паттерн: имя файла + id
    if files.is_empty() {
        let re2 = regex_lite::Regex::new(
            r#"\\x22([^\\"]+\.(?:zip|json|jar|mrpack))\\x22[^\\]{0,200}?\\x22([a-zA-Z0-9_-]{25,44})\\x22"#,
        )
        .ok();
        if let Some(re) = re2 {
            for cap in re.captures_iter(html) {
                let name = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let id = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                if !name.is_empty() && id != DRIVE_FOLDER_ID {
                    files.push(DriveFile {
                        id,
                        name: decode_js_string(&name),
                        mime: String::new(),
                        size: None,
                    });
                }
            }
        }
    }

    // data-id + title style
    if files.is_empty() {
        let re3 = regex_lite::Regex::new(
            r#"data-id="([a-zA-Z0-9_-]{25,44})"[^>]{0,300}?aria-label="([^"]+)""#,
        )
        .ok();
        if let Some(re) = re3 {
            for cap in re.captures_iter(html) {
                let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let name = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                if id != DRIVE_FOLDER_ID && looks_like_build_file(&name) {
                    files.push(DriveFile {
                        id,
                        name,
                        mime: String::new(),
                        size: None,
                    });
                }
            }
        }
    }

    files
}

fn parse_embedded_folder(html: &str) -> Vec<DriveFile> {
    let mut files = Vec::new();
    // <a href=".../file/d/ID/...">name</a>
    let re = match regex_lite::Regex::new(
        r#"/file/d/([a-zA-Z0-9_-]{25,44})/[^"]*"[^>]*>([^<]+)<"#,
    ) {
        Ok(r) => r,
        Err(_) => return files,
    };
    for cap in re.captures_iter(html) {
        let id = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let name = cap.get(2).map(|m| m.as_str().trim()).unwrap_or("").to_string();
        if looks_like_build_file(&name) {
            files.push(DriveFile {
                id,
                name,
                mime: String::new(),
                size: None,
            });
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

fn decode_js_string(s: &str) -> String {
    s.replace("\\u0026", "&")
        .replace("\\/", "/")
        .replace("\\\"", "\"")
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
    if out.is_empty() {
        "build".into()
    } else {
        out
    }
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
) -> Result<(), LauncherError> {
    if dest.exists() && dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let url = direct_download_url(file_id);
    let resp = client
        .get(&url)
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

    // Большие файлы: HTML с confirm-токеном.
    if content_type.contains("text/html") {
        let html = resp
            .text()
            .map_err(|e| LauncherError::Network(e.to_string()))?;
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
            return stream_to_file(resp2, dest, progress, label);
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

    stream_to_file(resp, dest, progress, label)
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
    download_drive_file(client, file_id, &tmp, progress, label)?;
    let bytes = fs::read(&tmp)?;
    let _ = fs::remove_file(&tmp);
    Ok(bytes)
}

fn stream_to_file(
    mut resp: reqwest::blocking::Response,
    dest: &Path,
    progress: Option<&ProgressFn>,
    label: &str,
) -> Result<(), LauncherError> {
    let total = resp.content_length().unwrap_or(0);
    let tmp = dest.with_extension("part");
    let mut file = File::create(&tmp)?;
    let mut chunk = [0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = resp
            .read(&mut chunk)
            .map_err(|e| LauncherError::Network(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n])?;
        done += n as u64;
        if let Some(cb) = progress {
            cb(done, total.max(done), label);
        }
    }
    file.flush()?;
    drop(file);
    fs::rename(&tmp, dest)?;
    Ok(())
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
    // form with id download-form
    let re = regex_lite::Regex::new(r#"name="confirm"\s+value="([^"]+)""#).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
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
