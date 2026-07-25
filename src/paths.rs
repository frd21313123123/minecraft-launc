use std::path::PathBuf;

pub const APP_NAME: &str = "MineLauncher";
pub const LAUNCHER_VERSION: &str = "1.0.0";

pub fn app_dir() -> PathBuf {
    dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

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

pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(app_dir())?;
    std::fs::create_dir_all(game_dir())?;
    std::fs::create_dir_all(versions_dir())?;
    std::fs::create_dir_all(libraries_dir())?;
    std::fs::create_dir_all(assets_dir())?;
    std::fs::create_dir_all(assets_dir().join("indexes"))?;
    std::fs::create_dir_all(assets_dir().join("objects"))?;
    Ok(())
}
