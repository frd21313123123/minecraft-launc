use std::path::PathBuf;

pub const APP_NAME: &str = "MineLauncher";
pub const LAUNCHER_VERSION: &str = "1.0.0";

pub fn app_dir() -> PathBuf {
    dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

/// Общий runtime-каталог (versions / libraries / assets).
/// Не используется как `--gameDir`: у каждой сборки свой каталог.
pub fn game_dir() -> PathBuf {
    app_dir().join("minecraft")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

pub fn versions_dir() -> PathBuf {
    game_dir().join("versions")
}

pub fn libraries_dir() -> PathBuf {
    game_dir().join("libraries")
}

pub fn assets_dir() -> PathBuf {
    game_dir().join("assets")
}

pub fn natives_dir(version_id: &str) -> PathBuf {
    versions_dir().join(version_id).join("natives")
}

/// Кэш скачанных zip-сборок с Google Drive.
pub fn builds_dir() -> PathBuf {
    app_dir().join("builds")
}

/// Корень распакованных сборок: `instances/{build_id}/`.
/// Каждая сборка живёт в своей папке и не пересекается с другими.
pub fn instances_dir() -> PathBuf {
    app_dir().join("instances")
}

/// Корень конкретной сборки: `instances/{build_id}/`.
pub fn instance_dir(build_id: &str) -> PathBuf {
    instances_dir().join(sanitize_build_id(build_id))
}

/// Игровой каталог сборки (`--gameDir`): mods, config, saves, options.
/// По умолчанию `instances/{build_id}/minecraft` — у каждой сборки свой.
pub fn instance_game_dir(build_id: &str) -> PathBuf {
    instance_dir(build_id).join("minecraft")
}

/// Безопасное имя папки сборки (без `..` и разделителей).
pub fn sanitize_build_id(build_id: &str) -> String {
    let s: String = build_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches(['.', '_', '-']).to_string();
    if s.is_empty() || s == "." || s == ".." {
        "build".into()
    } else {
        s
    }
}

pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(app_dir())?;
    std::fs::create_dir_all(game_dir())?;
    std::fs::create_dir_all(versions_dir())?;
    std::fs::create_dir_all(libraries_dir())?;
    std::fs::create_dir_all(assets_dir())?;
    std::fs::create_dir_all(assets_dir().join("indexes"))?;
    std::fs::create_dir_all(assets_dir().join("objects"))?;
    std::fs::create_dir_all(builds_dir())?;
    std::fs::create_dir_all(instances_dir())?;
    Ok(())
}
