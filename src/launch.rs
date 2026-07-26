use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use uuid::Uuid;

use crate::error::LauncherError;
use crate::install::{client_jar_path, library_classpath_path, load_version_json};
use crate::java::{ensure_java, required_java_major};
use crate::models::VersionJson;
use crate::paths::{assets_dir, ensure_dirs, game_dir, natives_dir, APP_NAME, LAUNCHER_VERSION};
use crate::rules::expand_argument;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn offline_uuid(username: &str) -> String {
    // UUID v3 DNS namespace — same as Java UUID.nameUUIDFromBytes offline
    // Mojang offline: UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))
    // which is UUID v3 with a custom namespace encoding... Actually Java's
    // nameUUIDFromBytes uses MD5 and sets version 3 without DNS namespace.
    // Standard custom launchers often use:
    // Uuid::new_v3(&Uuid::NAMESPACE_DNS, format!("OfflinePlayer:{username}"))
    // OR the Java-compatible offline UUID.
    java_name_uuid_from_bytes(&format!("OfflinePlayer:{username}"))
}

/// Совместимо с `UUID.nameUUIDFromBytes` в Java (MD5, version 3).
fn java_name_uuid_from_bytes(data: &str) -> String {
    use md5::{Digest, Md5}; // crate: md-5
    let mut hasher = Md5::new();
    hasher.update(data.as_bytes());
    let mut bytes = hasher.finalize();
    bytes[6] = (bytes[6] & 0x0f) | 0x30; // version 3
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // IETF variant
    let uuid = Uuid::from_bytes(bytes.into());
    uuid.to_string()
}

pub fn build_launch_command(
    version_id: &str,
    username: &str,
    ram_mb: u32,
    java_path: &str,
) -> Result<Vec<String>, LauncherError> {
    build_launch_command_with_dir(version_id, username, ram_mb, java_path, &game_dir())
}

pub fn build_launch_command_with_dir(
    version_id: &str,
    username: &str,
    ram_mb: u32,
    java_path: &str,
    game: &Path,
) -> Result<Vec<String>, LauncherError> {
    ensure_dirs()?;
    // `game` — изолированный каталог сборки (mods/saves/config).
    // versions/libraries/assets берутся из общего runtime, не из game.
    std::fs::create_dir_all(game)?;
    let username = {
        let u = username.trim();
        let u = if u.is_empty() { "Player" } else { u };
        u.chars().take(16).collect::<String>()
    };

    let version = load_version_json(version_id)?;
    let java = ensure_java(
        java_path,
        required_java_major(&version, version_id),
        None,
        None,
    )?;
    let natives = natives_dir(version_id);
    let assets = assets_dir();
    let client_jar = client_jar_path(&version);
    let mod_bootstrap = uses_mod_bootstrap(&version);

    // Vanilla: нужен client jar. NeoForge/Forge берут transformed client из libraries.
    if !mod_bootstrap && !client_jar.exists() {
        return Err(LauncherError::Other(format!(
            "Клиент не найден: {} (установите базовую версию Minecraft)",
            client_jar.display()
        )));
    }
    // Для loader всё равно должна быть установлена parent-vanilla (assets / jar id).
    if mod_bootstrap && !client_jar.exists() {
        let parent = version.jar.as_deref().unwrap_or("?");
        return Err(LauncherError::Other(format!(
            "Не найдена базовая версия Minecraft ({parent}): {}\nУстановите vanilla перед запуском сборки.",
            client_jar.display()
        )));
    }

    // Natives: если пусто у loader-версии — взять от jar id (vanilla)
    let natives = if natives.exists() {
        natives
    } else if let Some(jar_id) = version.jar.as_ref() {
        let p = natives_dir(jar_id);
        if p.exists() {
            p
        } else {
            natives_dir(version_id)
        }
    } else {
        natives
    };
    if !natives.exists() {
        std::fs::create_dir_all(&natives)?;
    }

    let classpath = build_classpath(&version, &client_jar, mod_bootstrap)?;
    let uuid = offline_uuid(&username);
    let asset_index = version
        .asset_index
        .as_ref()
        .map(|a| a.id.clone())
        .unwrap_or_else(|| "legacy".into());

    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("auth_player_name".into(), username.clone());
    vars.insert("version_name".into(), version_id.to_string());
    vars.insert("game_directory".into(), path_str(game));
    vars.insert("assets_root".into(), path_str(&assets));
    vars.insert(
        "game_assets".into(),
        path_str(&assets.join("virtual").join("legacy")),
    );
    vars.insert("assets_index_name".into(), asset_index.clone());
    vars.insert("auth_uuid".into(), uuid);
    vars.insert("auth_access_token".into(), "0".into());
    vars.insert("user_type".into(), "legacy".into());
    vars.insert("version_type".into(), version.version_type.clone());
    vars.insert("natives_directory".into(), path_str(&natives));
    vars.insert("launcher_name".into(), APP_NAME.into());
    vars.insert("launcher_version".into(), LAUNCHER_VERSION.into());
    vars.insert("classpath".into(), classpath.clone());
    vars.insert("user_properties".into(), "{}".into());
    vars.insert("clientid".into(), "0".into());
    vars.insert("auth_xuid".into(), "0".into());
    vars.insert("resolution_width".into(), "854".into());
    vars.insert("resolution_height".into(), "480".into());
    vars.insert(
        "library_directory".into(),
        path_str(&crate::paths::libraries_dir()),
    );
    vars.insert(
        "classpath_separator".into(),
        if cfg!(windows) {
            ";".into()
        } else {
            ":".into()
        },
    );

    let mut cmd: Vec<String> = Vec::new();
    cmd.push(path_str(&java));

    // Memory
    let xms = ram_mb.min(512);
    cmd.push(format!("-Xmx{ram_mb}M"));
    cmd.push(format!("-Xms{xms}M"));

    // Offline auth stubs (меньше сетевых таймаутов)
    cmd.push("-Dminecraft.api.auth.host=https://0.0.0.0".into());
    cmd.push("-Dminecraft.api.account.host=https://0.0.0.0".into());
    cmd.push("-Dminecraft.api.session.host=https://0.0.0.0".into());
    cmd.push("-Dminecraft.api.services.host=https://0.0.0.0".into());

    // JVM args from version
    let mut added_cp = false;
    if let Some(args) = &version.arguments {
        if let Some(jvm) = &args.jvm {
            for arg in jvm {
                for piece in expand_argument(arg) {
                    let s = substitute(&piece, &vars);
                    if s == "-cp" || s == "-classpath" {
                        added_cp = true;
                    }
                    cmd.push(s);
                }
            }
        } else {
            push_default_jvm(&mut cmd, &vars);
            added_cp = true;
        }
    } else {
        push_default_jvm(&mut cmd, &vars);
        added_cp = true;
    }

    // NeoForge/Forge часто не кладут ${classpath} в jvm-аргументы
    if !added_cp && !cmd.iter().any(|a| a == &classpath) {
        cmd.push("-cp".into());
        cmd.push(classpath);
    }

    if version.main_class.is_empty() {
        return Err(LauncherError::Other("mainClass пуст в version.json".into()));
    }
    cmd.push(version.main_class.clone());

    // Game args
    if let Some(args) = &version.arguments {
        if let Some(game_args) = &args.game {
            for arg in game_args {
                for piece in expand_argument(arg) {
                    cmd.push(substitute(&piece, &vars));
                }
            }
        }
    } else if let Some(legacy) = &version.minecraft_arguments {
        for piece in legacy.split_whitespace() {
            cmd.push(substitute(piece, &vars));
        }
    } else {
        // minimal fallback
        cmd.push("--username".into());
        cmd.push(username);
        cmd.push("--version".into());
        cmd.push(version_id.to_string());
        cmd.push("--gameDir".into());
        cmd.push(path_str(game));
        cmd.push("--assetsDir".into());
        cmd.push(path_str(&assets));
        cmd.push("--assetIndex".into());
        cmd.push(asset_index);
        cmd.push("--uuid".into());
        cmd.push(offline_uuid(
            vars.get("auth_player_name")
                .map(|s| s.as_str())
                .unwrap_or("Player"),
        ));
        cmd.push("--accessToken".into());
        cmd.push("0".into());
        cmd.push("--userType".into());
        cmd.push("legacy".into());
    }

    Ok(cmd)
}

fn push_default_jvm(cmd: &mut Vec<String>, vars: &HashMap<String, String>) {
    cmd.push(format!("-Djava.library.path={}", vars["natives_directory"]));
    cmd.push("-cp".into());
    cmd.push(vars["classpath"].clone());
}

/// NeoForge/Forge (ModLauncher) — mainClass = BootstrapLauncher.
fn uses_mod_bootstrap(version: &VersionJson) -> bool {
    let mc = version.main_class.to_lowercase();
    mc.contains("bootstraplauncher") || mc.contains("cpw.mods.modlauncher")
}

fn build_classpath(
    version: &VersionJson,
    client_jar: &Path,
    mod_bootstrap: bool,
) -> Result<String, LauncherError> {
    use std::collections::HashSet;

    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut parts: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push_unique = |path: String| {
        // BootstrapLauncher падает на дубликатах одного и того же jar.
        let key = path.replace('/', "\\").to_lowercase();
        if seen.insert(key) {
            parts.push(path);
        }
    };

    for lib in &version.libraries {
        if let Some(p) = library_classpath_path(lib) {
            push_unique(path_str(&p));
        }
    }

    // НЕ кладём vanilla client jar в classpath для NeoForge/Forge.
    // Иначе появляется второй модуль `_1._21._1` рядом с `minecraft` и краш:
    // "Modules _1._21._1 and minecraft export package net.minecraft.server ..."
    // Transformed client уже в libraries (net/minecraft/client/...-srg.jar).
    if !mod_bootstrap {
        push_unique(path_str(client_jar));
    }

    Ok(parts.join(sep))
}

fn substitute(template: &str, vars: &HashMap<String, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

pub fn launch_game(
    version_id: &str,
    username: &str,
    ram_mb: u32,
    java_path: &str,
) -> Result<Child, LauncherError> {
    launch_game_with_dir(version_id, username, ram_mb, java_path, &game_dir())
}

pub fn launch_game_with_dir(
    version_id: &str,
    username: &str,
    ram_mb: u32,
    java_path: &str,
    game: &Path,
) -> Result<Child, LauncherError> {
    let args = build_launch_command_with_dir(version_id, username, ram_mb, java_path, game)?;
    let (program, rest) = args
        .split_first()
        .ok_or_else(|| LauncherError::Other("Пустая команда запуска".into()))?;

    std::fs::create_dir_all(game)?;

    // Лог запуска — чтобы показать ошибку, если Java сразу падает.
    let log_path = crate::paths::app_dir().join("last_launch.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (stdout, stderr) = match std::fs::File::create(&log_path) {
        Ok(out) => match out.try_clone() {
            Ok(err) => (Stdio::from(out), Stdio::from(err)),
            Err(_) => (Stdio::from(out), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = Command::new(program);
    cmd.args(rest)
        .current_dir(game)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);

    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| LauncherError::Other(format!("Не удалось запустить игру: {e}")))?;

    // Если процесс умер за ~2.5 с — это почти наверняка crash при старте.
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        match child.try_wait() {
            Ok(Some(status)) => {
                let tail = read_log_tail(&log_path, 1200);
                return Err(LauncherError::Other(format!(
                    "Игра сразу закрылась (код {}).\n{}",
                    status.code().unwrap_or(-1),
                    if tail.is_empty() {
                        format!("Смотрите лог: {}", log_path.display())
                    } else {
                        tail
                    }
                )));
            }
            Ok(None) => {}
            Err(e) => {
                return Err(LauncherError::Other(format!(
                    "Ошибка ожидания процесса: {e}"
                )));
            }
        }
    }

    Ok(child)
}

fn read_log_tail(path: &Path, max_chars: usize) -> String {
    let Ok(data) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Берём хвост — там обычно Exception.
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max_chars {
        return trimmed.to_string();
    }
    let start = chars.len() - max_chars;
    format!("…{}", chars[start..].iter().collect::<String>())
}
