use serde::{Deserialize, Serialize};
use std::fs;

use crate::paths::{config_path, ensure_dirs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Dark,
    Light,
}

impl Default for Theme {
    fn default() -> Self {
        Self::Dark
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinModel {
    Classic,
    Slim,
}

impl Default for SkinModel {
    fn default() -> Self {
        Self::Classic
    }
}

impl SkinModel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Классическая (Steve)",
            Self::Slim => "Тонкая (Alex)",
        }
    }

    pub fn command_value(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Slim => "slim",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountConfig {
    pub username: String,
    /// Локальный PNG-файл, имя Minecraft-профиля или публичный HTTPS URL скина.
    pub skin_source: String,
    pub skin_model: SkinModel,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            username: "Player".into(),
            skin_source: String::new(),
            skin_model: SkinModel::Classic,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub username: String,
    /// Последняя выбранная сборка (id).
    pub last_build: String,
    /// Старое поле — сохраняем совместимость с config.json.
    #[serde(default)]
    pub last_version: String,
    pub ram_mb: u32,
    pub java_path: String,
    pub show_snapshots: bool,
    /// Цветовая тема интерфейса.
    pub theme: Theme,
    /// Пользовательский корень для архивов и распакованных сборок.
    /// Пустая строка означает каталог, из которого запущен лаунчер.
    pub builds_directory: String,
    /// Офлайн-профили. Пароли лаунчер намеренно не хранит.
    pub accounts: Vec<AccountConfig>,
    pub active_account: usize,
    pub auto_ram: bool,
    pub server_name: String,
    pub server_address: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: "Player".into(),
            last_build: String::new(),
            last_version: String::new(),
            ram_mb: 2048,
            java_path: String::new(),
            show_snapshots: false,
            theme: Theme::Dark,
            builds_directory: String::new(),
            accounts: vec![AccountConfig::default()],
            active_account: 0,
            auto_ram: true,
            server_name: "SPCREATE".into(),
            server_address: String::new(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let _ = ensure_dirs();
        let path = config_path();
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(mut cfg) = serde_json::from_str::<Self>(&data) {
                cfg.normalize();
                return cfg;
            }
        }
        Self::default()
    }

    pub fn normalize(&mut self) {
        if self.accounts.is_empty() {
            self.accounts.push(AccountConfig {
                username: if self.username.trim().is_empty() {
                    "Player".into()
                } else {
                    self.username.trim().to_string()
                },
                ..Default::default()
            });
        }
        self.active_account = self.active_account.min(self.accounts.len().saturating_sub(1));
        if self.accounts[self.active_account].username.trim().is_empty() {
            self.accounts[self.active_account].username = "Player".into();
        }
        self.username = self.accounts[self.active_account].username.clone();
        self.ram_mb = self.ram_mb.max(1024);
        if self.server_name.trim().is_empty() {
            self.server_name = "МОЯ СБОРКА".into();
        }
    }

    pub fn save(&self) -> Result<(), String> {
        ensure_dirs().map_err(|e| e.to_string())?;
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(config_path(), data).map_err(|e| e.to_string())
    }
}
