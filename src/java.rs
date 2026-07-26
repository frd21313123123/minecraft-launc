use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::download::ProgressFn;
use crate::error::LauncherError;
use crate::models::VersionJson;
use crate::paths::{app_dir, APP_NAME, LAUNCHER_VERSION};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const DEFAULT_JAVA_MAJOR: u32 = 21;
const ADOPTIUM_API: &str = "https://api.adoptium.net/v3";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaInfo {
    pub path: PathBuf,
    pub major_version: u32,
    pub version_line: String,
    pub is_64_bit: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AdoptiumAsset {
    binary: AdoptiumBinary,
}

#[derive(Debug, Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}

#[derive(Debug, Deserialize)]
struct AdoptiumPackage {
    checksum: String,
    link: String,
    name: String,
    size: Option<u64>,
}

/// Ищет любую работоспособную Java. Существование файла само по себе недостаточно:
/// JVM должна успешно ответить на `-version`.
pub fn find_java(custom: &str) -> Option<PathBuf> {
    find_java_info(custom, None).map(|info| info.path)
}

/// Ищет работоспособную 64-битную (для 64-битного лаунчера) JVM нужной major-версии.
pub fn find_compatible_java(custom: &str, required_major: u32) -> Option<PathBuf> {
    find_java_info(custom, Some(required_major)).map(|info| info.path)
}

pub fn java_info(java: &Path) -> Option<JavaInfo> {
    inspect_java(java)
}

pub fn java_version_string(java: &Path) -> Option<String> {
    inspect_java(java).map(|info| info.version_line)
}

/// Возвращает подходящую JVM, а если её нет — скачивает переносимую Eclipse Temurin.
///
/// Пользовательский путь имеет приоритет, но сломанная или несовместимая Java не
/// блокирует запуск: лаунчер попробует остальные JVM и затем встроенный runtime.
pub fn ensure_java(
    custom: &str,
    required_major: u32,
    progress: Option<&ProgressFn>,
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf, LauncherError> {
    let required_major = required_major.max(1);
    if let Some(java) = find_compatible_java(custom, required_major) {
        return Ok(java);
    }

    if is_cancelled(cancel) {
        return Err(LauncherError::Other("Отменено".into()));
    }

    let guard = install_lock()
        .lock()
        .map_err(|_| LauncherError::Java("внутренняя блокировка установки повреждена".into()))?;

    // Пока поток ждал блокировку, другой поток мог уже установить runtime.
    if let Some(java) = find_compatible_java(custom, required_major) {
        drop(guard);
        return Ok(java);
    }

    let result = install_managed_java(required_major, progress, cancel).map_err(|error| {
        LauncherError::Java(format!(
            "нужна Java {required_major}, но подходящая JVM не найдена и автоматическая загрузка не удалась: {error}"
        ))
    });
    drop(guard);
    result
}

pub fn ensure_java_for_version(
    version_id: &str,
    custom: &str,
    progress: Option<&ProgressFn>,
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf, LauncherError> {
    let version = crate::install::load_version_json(version_id)?;
    ensure_java(
        custom,
        required_java_major(&version, version_id),
        progress,
        cancel,
    )
}

/// Требование Mojang из version.json имеет приоритет. Fallback нужен для
/// старых/сторонних JSON, где поле javaVersion отсутствует.
pub fn required_java_major(version: &VersionJson, version_id: &str) -> u32 {
    version
        .java_version
        .as_ref()
        .and_then(|java| java.major_version)
        .filter(|major| *major > 0)
        .unwrap_or_else(|| infer_java_major(version_id))
}

/// Версия Java для запуска установщика NeoForge до чтения его итогового JSON.
pub fn neoforge_java_major(neoforge_version: &str) -> u32 {
    let minecraft_minor = neoforge_version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok());
    match minecraft_minor {
        Some(minor) if minor >= 21 => 21,
        Some(minor) if minor >= 18 => 17,
        _ => DEFAULT_JAVA_MAJOR,
    }
}

fn find_java_info(custom: &str, required_major: Option<u32>) -> Option<JavaInfo> {
    for candidate in java_candidates(custom, required_major) {
        let Some(info) = inspect_java(&candidate) else {
            continue;
        };
        if required_major.is_some_and(|major| info.major_version != major) {
            continue;
        }
        if cfg!(target_pointer_width = "64") && info.is_64_bit == Some(false) {
            continue;
        }
        return Some(info);
    }
    None
}

fn java_candidates(custom: &str, required_major: Option<u32>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    let custom = custom.trim().trim_matches('"');
    if !custom.is_empty() {
        push_java_from_path(Path::new(custom), &mut candidates);
    }

    if let Some(major) = required_major {
        push_java_from_home(&managed_runtime_dir(major), &mut candidates);
    }
    collect_java_bins(&managed_runtimes_dir(), 2, &mut candidates);

    if let Ok(home) = std::env::var("JAVA_HOME") {
        push_java_from_home(Path::new(home.trim().trim_matches('"')), &mut candidates);
    }

    if let Some(path) = which("java").or_else(|| which("javaw")) {
        push_unique(&mut candidates, path);
    }

    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            let root = PathBuf::from(program_files);
            roots.push(root.join("Java"));
            roots.push(root.join("Eclipse Adoptium"));
            roots.push(root.join("Microsoft"));
            roots.push(root.join("Zulu"));
            roots.push(root.join("Amazon Corretto"));
        }
        if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
            roots.push(PathBuf::from(program_files_x86).join("Java"));
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".jdks"));
        }
        for root in roots {
            collect_java_bins(&root, 2, &mut candidates);
        }
    }

    deduplicate_paths(candidates)
}

fn inspect_java(java: &Path) -> Option<JavaInfo> {
    if !java.is_file() {
        return None;
    }

    let probe = console_java_for(java);
    let mut command = Command::new(&probe);
    command.args(["-XshowSettings:properties", "-version"]);
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = format!("{stderr}\n{stdout}");
    let version_line = text
        .lines()
        .map(str::trim)
        .find(|line| {
            line.starts_with("java version")
                || line.starts_with("openjdk version")
                || line.starts_with("openj9 version")
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Java {}", parse_java_major(&text).unwrap_or_default()));
    let major_version = parse_java_major(&text)?;
    let is_64_bit = parse_property(&text, "sun.arch.data.model")
        .map(|value| value == "64")
        .or_else(|| {
            parse_property(&text, "os.arch").map(|arch| {
                matches!(
                    arch.to_ascii_lowercase().as_str(),
                    "amd64" | "x86_64" | "aarch64" | "ppc64" | "ppc64le" | "s390x" | "riscv64"
                )
            })
        });

    Some(JavaInfo {
        path: probe,
        major_version,
        version_line,
        is_64_bit,
    })
}

fn parse_java_major(text: &str) -> Option<u32> {
    if let Some(value) = parse_property(text, "java.version") {
        if let Some(major) = parse_version_value(value) {
            return Some(major);
        }
    }

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("java version")
            && !line.starts_with("openjdk version")
            && !line.starts_with("openj9 version")
        {
            continue;
        }
        if let Some(version) = line.split('"').nth(1) {
            return parse_version_value(version);
        }
    }
    None
}

fn parse_property<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == name).then(|| value.trim())
    })
}

fn parse_version_value(version: &str) -> Option<u32> {
    let version = version.trim();
    if let Some(legacy) = version.strip_prefix("1.") {
        return leading_number(legacy);
    }
    leading_number(version)
}

fn leading_number(value: &str) -> Option<u32> {
    let digits: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn infer_java_major(version_id: &str) -> u32 {
    let release = version_id
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .find(|part| part.starts_with("1."))
        .unwrap_or(version_id);
    let mut numbers = release
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    let major = numbers.next();
    let minor = numbers.next();
    let patch = numbers.next().unwrap_or(0);

    match (major, minor) {
        (Some(1), Some(minor)) if minor >= 21 => 21,
        (Some(1), Some(20)) if patch >= 5 => 21,
        (Some(1), Some(minor)) if minor >= 18 => 17,
        (Some(1), Some(17)) => 16,
        (Some(1), Some(_)) => 8,
        _ => DEFAULT_JAVA_MAJOR,
    }
}

fn install_managed_java(
    major: u32,
    progress: Option<&ProgressFn>,
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf, LauncherError> {
    let (os, arch, archive_kind) = adoptium_platform()?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("{APP_NAME}/{LAUNCHER_VERSION} (Rust)"))
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(15 * 60))
        .build()
        .map_err(|error| LauncherError::Network(error.to_string()))?;
    let package = fetch_adoptium_package(&client, major, os, arch)?;

    let cache_dir = managed_java_root().join("cache");
    fs::create_dir_all(&cache_dir)?;
    let archive_name = Path::new(&package.name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| LauncherError::Java("API вернул небезопасное имя архива".into()))?;
    let archive_path = cache_dir.join(archive_name);
    download_java_archive(&client, &package, &archive_path, major, progress, cancel)?;

    if is_cancelled(cancel) {
        return Err(LauncherError::Other("Отменено".into()));
    }
    report(progress, 0, 0, &format!("Распаковка Java {major}…"));

    let runtimes = managed_runtimes_dir();
    fs::create_dir_all(&runtimes)?;
    let staging = runtimes.join(format!(".java-{major}-installing"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;

    let extraction = extract_runtime_archive(&archive_path, &staging, archive_kind)
        .and_then(|_| locate_java_home(&staging));
    let java_home = match extraction {
        Ok(home) => home,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    if is_cancelled(cancel) {
        let _ = fs::remove_dir_all(&staging);
        return Err(LauncherError::Other("Отменено".into()));
    }

    let final_dir = managed_runtime_dir(major);
    if final_dir.exists() {
        fs::remove_dir_all(&final_dir)?;
    }
    if java_home == staging {
        fs::rename(&staging, &final_dir)?;
    } else {
        fs::rename(&java_home, &final_dir)?;
        let _ = fs::remove_dir_all(&staging);
    }

    let java = java_launcher_in_home(&final_dir).ok_or_else(|| {
        LauncherError::Java(format!(
            "после распаковки Java {major} не найден исполняемый файл"
        ))
    })?;
    let info = inspect_java(&java).ok_or_else(|| {
        LauncherError::Java(format!(
            "скачанная Java {major} не запускается: {}",
            java.display()
        ))
    })?;
    if info.major_version != major {
        return Err(LauncherError::Java(format!(
            "API вернул Java {}, хотя требовалась Java {major}",
            info.major_version
        )));
    }
    if cfg!(target_pointer_width = "64") && info.is_64_bit == Some(false) {
        return Err(LauncherError::Java(
            "скачана 32-битная Java для 64-битного лаунчера".into(),
        ));
    }

    report(progress, 1, 1, &format!("Java {major} готова"));
    Ok(info.path)
}

fn fetch_adoptium_package(
    client: &reqwest::blocking::Client,
    major: u32,
    os: &str,
    arch: &str,
) -> Result<AdoptiumPackage, LauncherError> {
    let mut last_error = None;
    // JRE меньше; JDK является совместимым резервным вариантом, если JRE для
    // конкретной платформы/версии не публикуется.
    for image_type in ["jre", "jdk"] {
        let url = format!(
            "{ADOPTIUM_API}/assets/latest/{major}/hotspot?architecture={arch}&heap_size=normal&image_type={image_type}&jvm_impl=hotspot&os={os}&page=0&page_size=1&project=jdk&sort_order=DESC&vendor=eclipse"
        );
        let response = client
            .get(&url)
            .send()
            .map_err(|error| LauncherError::Network(error.to_string()));
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        let response = match response.error_for_status() {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        let assets: Vec<AdoptiumAsset> = match response.json() {
            Ok(assets) => assets,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        if let Some(asset) = assets.into_iter().next() {
            return Ok(asset.binary.package);
        }
        last_error = Some(format!(
            "для {os}/{arch} нет образа {image_type} Java {major}"
        ));
    }

    Err(LauncherError::Java(last_error.unwrap_or_else(|| {
        format!("Adoptium не вернул Java {major} для {os}/{arch}")
    })))
}

fn download_java_archive(
    client: &reqwest::blocking::Client,
    package: &AdoptiumPackage,
    destination: &Path,
    major: u32,
    progress: Option<&ProgressFn>,
    cancel: Option<&AtomicBool>,
) -> Result<(), LauncherError> {
    if destination.is_file() && verify_sha256(destination, &package.checksum)? {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut response = client
        .get(&package.link)
        .send()
        .map_err(|error| LauncherError::Network(error.to_string()))?
        .error_for_status()
        .map_err(|error| LauncherError::Network(error.to_string()))?;
    let total = response
        .content_length()
        .or(package.size)
        .unwrap_or_default();
    let temporary = destination.with_extension("part");
    let mut file = File::create(&temporary)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut done = 0_u64;
    let label = format!("Скачивание Java {major}…");

    loop {
        if is_cancelled(cancel) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(LauncherError::Other("Отменено".into()));
        }
        let count = response
            .read(&mut buffer)
            .map_err(|error| LauncherError::Network(error.to_string()))?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        done += count as u64;
        report(progress, done, total, &label);
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);

    let checksum = hex::encode(hasher.finalize());
    if !checksum.eq_ignore_ascii_case(&package.checksum) {
        let _ = fs::remove_file(&temporary);
        return Err(LauncherError::Checksum {
            path: destination.display().to_string(),
            expected: package.checksum.clone(),
            got: checksum,
        });
    }
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(temporary, destination)?;
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<bool, LauncherError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected))
}

fn extract_runtime_archive(
    archive_path: &Path,
    destination: &Path,
    archive_kind: ArchiveKind,
) -> Result<(), LauncherError> {
    match archive_kind {
        ArchiveKind::Zip => extract_zip(archive_path, destination),
        ArchiveKind::TarGz => extract_tar_gz(archive_path, destination),
    }
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<(), LauncherError> {
    let file = File::open(archive_path)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| LauncherError::Parse(error.to_string()))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| LauncherError::Parse(error.to_string()))?;
        let relative = entry.enclosed_name().ok_or_else(|| {
            LauncherError::Java(format!("небезопасный путь в Java-архиве: {}", entry.name()))
        })?;
        let output = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&output)?;
        std::io::copy(&mut entry, &mut file)?;
    }
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, destination: &Path) -> Result<(), LauncherError> {
    let file = File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(destination)
        .map_err(|error| LauncherError::Java(format!("не удалось распаковать Java: {error}")))
}

fn locate_java_home(root: &Path) -> Result<PathBuf, LauncherError> {
    locate_java_home_inner(root, 4).ok_or_else(|| {
        LauncherError::Java("в скачанном архиве отсутствует bin/java".into())
    })
}

fn locate_java_home_inner(root: &Path, depth: u8) -> Option<PathBuf> {
    if java_launcher_in_home(root).is_some() {
        return Some(root.to_path_buf());
    }
    if depth == 0 {
        return None;
    }
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(home) = locate_java_home_inner(&path, depth - 1) {
                return Some(home);
            }
        }
    }
    None
}

fn managed_java_root() -> PathBuf {
    app_dir().join("java")
}

fn managed_runtimes_dir() -> PathBuf {
    managed_java_root().join("runtimes")
}

fn managed_runtime_dir(major: u32) -> PathBuf {
    managed_runtimes_dir().join(format!("temurin-{major}"))
}

fn install_lock() -> &'static Mutex<()> {
    static INSTALL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    INSTALL_LOCK.get_or_init(|| Mutex::new(()))
}

fn java_launcher_in_home(home: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    for name in ["java.exe", "javaw.exe"] {
        let path = home.join("bin").join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    #[cfg(not(windows))]
    {
        let path = home.join("bin").join("java");
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn push_java_from_home(home: &Path, candidates: &mut Vec<PathBuf>) {
    if let Some(java) = java_launcher_in_home(home) {
        candidates.push(java);
    }
    if let Some(java) = java_launcher_in_home(&home.join("jre")) {
        candidates.push(java);
    }
}

fn push_java_from_path(path: &Path, candidates: &mut Vec<PathBuf>) {
    if path.is_dir() {
        push_java_from_home(path, candidates);
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            if name.eq_ignore_ascii_case("bin") {
                push_java_from_home(path.parent().unwrap_or(path), candidates);
            }
        }
        return;
    }
    if path.is_file() {
        candidates.push(path.to_path_buf());
    }
}

fn collect_java_bins(root: &Path, depth: u8, candidates: &mut Vec<PathBuf>) {
    if !root.is_dir() {
        return;
    }
    push_java_from_home(root, candidates);
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_java_bins(&path, depth - 1, candidates);
        }
    }
}

fn console_java_for(java: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if java
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("javaw.exe"))
        {
            let console = java.with_file_name("java.exe");
            if console.is_file() {
                return console;
            }
        }
    }
    java.to_path_buf()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = directory.join(format!("{name}.exe"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn deduplicate_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| {
            let key = if cfg!(windows) {
                path.to_string_lossy().to_ascii_lowercase()
            } else {
                path.to_string_lossy().into_owned()
            };
            seen.insert(key)
        })
        .collect()
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| {
        if cfg!(windows) {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
        } else {
            existing == &path
        }
    }) {
        paths.push(path);
    }
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

fn report(progress: Option<&ProgressFn>, done: u64, total: u64, label: &str) {
    if let Some(progress) = progress {
        progress(done, total, label);
    }
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    Zip,
    TarGz,
}

fn adoptium_platform() -> Result<(&'static str, &'static str, ArchiveKind), LauncherError> {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "mac",
        other => {
            return Err(LauncherError::Java(format!(
                "автозагрузка Java не поддерживает ОС {other}"
            )));
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        "x86" => "x86",
        "arm" => "arm",
        other => {
            return Err(LauncherError::Java(format!(
                "автозагрузка Java не поддерживает архитектуру {other}"
            )));
        }
    };
    let archive = if os == "windows" {
        ArchiveKind::Zip
    } else {
        ArchiveKind::TarGz
    };
    Ok((os, arch, archive))
}

#[cfg(test)]
mod tests {
    use super::{
        fetch_adoptium_package, infer_java_major, parse_java_major, parse_version_value,
    };

    #[test]
    fn parses_modern_and_legacy_java_versions() {
        assert_eq!(parse_version_value("21.0.8+9-LTS"), Some(21));
        assert_eq!(parse_version_value("17.0.12"), Some(17));
        assert_eq!(parse_version_value("1.8.0_442"), Some(8));
        assert_eq!(
            parse_java_major(
                r#"
                    Property settings:
                        java.version = 21.0.8
                    openjdk version "21.0.8" 2025-07-15 LTS
                "#
            ),
            Some(21)
        );
    }

    #[test]
    fn infers_minecraft_fallback_java_version() {
        assert_eq!(infer_java_major("1.16.5"), 8);
        assert_eq!(infer_java_major("1.17.1"), 16);
        assert_eq!(infer_java_major("1.18.2"), 17);
        assert_eq!(infer_java_major("1.20.4"), 17);
        assert_eq!(infer_java_major("1.20.5"), 21);
        assert_eq!(infer_java_major("1.21.1"), 21);
        assert_eq!(infer_java_major("neoforge-1.21.1"), 21);
    }

    #[test]
    #[ignore = "uses the public Adoptium metadata API"]
    fn adoptium_api_returns_a_verified_windows_archive() {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let package = fetch_adoptium_package(&client, 21, "windows", "x64").unwrap();
        assert!(package.name.ends_with(".zip"));
        assert!(package.link.starts_with("https://"));
        assert_eq!(package.checksum.len(), 64);
        assert!(package.size.is_some_and(|size| size > 0));
    }
}
