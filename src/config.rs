use serde::{Deserialize, Serialize};
use std::fs;

use crate::paths::{config_path, ensure_dirs};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub username: String,
    pub last_version: String,
    pub ram_mb: u32,
    pub java_path: String,
    pub show_snapshots: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: "Player".into(),
            last_version: String::new(),
            ram_mb: 2048,
            java_path: String::new(),
            show_snapshots: false,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let _ = ensure_dirs();
        let path = config_path();
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&data) {
                return cfg;
            }
        }
        Self::default()
    }

    pub fn save(&self) -> Result<(), String> {
        ensure_dirs().map_err(|e| e.to_string())?;
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(config_path(), data).map_err(|e| e.to_string())
    }
}
