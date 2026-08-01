use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

pub const APP_NAME: &str = "MineLauncher";
pub const LAUNCHER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Каталог, из которого был запущен лаунчер.
fn launch_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .clone()
}

pub fn app_dir() -> PathBuf {
    dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

fn builds_root_override() -> &'static RwLock<Option<PathBuf>> {
    static ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    ROOT.get_or_init(|| RwLock::new(None))
}

/// Меняет корень, в котором хранятся архивы и распакованные сборки.
/// Пустой путь возвращает стандартное расположение в каталоге запуска.
pub fn set_builds_root(path: Option<PathBuf>) {
    if let Ok(mut root) = builds_root_override().write() {
        *root = path.filter(|p| !p.as_os_str().is_empty());
    }
}

pub fn builds_root() -> PathBuf {
    builds_root_override()
        .read()
        .ok()
        .and_then(|root| root.clone())
        .unwrap_or_else(launch_dir)
}

/// Общий runtime-каталог (versions / libraries / assets).
/// Не используется как `--gameDir`: у каждой сборки свой каталог.
pub fn game_dir() -> PathBuf {
    app_dir().join("minecraft")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

/// Пользовательская галерея лаунчера.
///
/// Папка располагается рядом с местом запуска приложения, чтобы владелец
/// сборки мог просто положить туда PNG/JPG/WebP без поиска AppData.
pub fn screenshots_dir() -> PathBuf {
    launch_dir().join("screenshots")
}

pub fn last_launch_log() -> PathBuf {
    app_dir().join("last_launch.log")
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
    let root = builds_root();
    if root == app_dir() {
        root.join("builds")
    } else {
        root.join("archives")
    }
}

/// Корень распакованных сборок: `instances/{build_id}/`.
/// Каждая сборка живёт в своей папке и не пересекается с другими.
pub fn instances_dir() -> PathBuf {
    builds_root().join("instances")
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
    std::fs::create_dir_all(builds_root())?;
    std::fs::create_dir_all(game_dir())?;
    std::fs::create_dir_all(versions_dir())?;
    std::fs::create_dir_all(libraries_dir())?;
    std::fs::create_dir_all(assets_dir())?;
    std::fs::create_dir_all(assets_dir().join("indexes"))?;
    std::fs::create_dir_all(assets_dir().join("objects"))?;
    std::fs::create_dir_all(builds_dir())?;
    std::fs::create_dir_all(instances_dir())?;
    std::fs::create_dir_all(screenshots_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_builds_root_is_launch_directory() {
        assert_eq!(builds_root(), launch_dir());
    }

    #[test]
    fn sanitize_build_id_blocks_path_traversal_and_separators() {
        assert_eq!(sanitize_build_id("../../"), "build");
        assert_eq!(sanitize_build_id("..\\evil/pack"), "evil_pack");
        assert_eq!(sanitize_build_id(" My pack! "), "My_pack");
    }

    #[test]
    fn sanitize_build_id_preserves_safe_identifiers() {
        assert_eq!(sanitize_build_id("create-a2_1.21.1"), "create-a2_1.21.1");
    }
}
