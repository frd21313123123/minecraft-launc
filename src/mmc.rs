//! Поддержка экспортов Prism Launcher / MultiMC.
//!
//! Структура zip (пример createA2):
//! - `mmc-pack.json` — компоненты (Minecraft, NeoForge/Forge/Fabric)
//! - `instance.cfg` — имя, ОЗУ, …
//! - `minecraft/` — game directory (mods, config, options, …)

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::LauncherError;

#[derive(Debug, Clone)]
pub struct MmcPack {
    pub name: String,
    pub minecraft: String,
    pub loader: ModLoader,
    /// Каталог `.minecraft` внутри инстанса.
    pub game_dir: PathBuf,
    pub max_mem_mb: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum ModLoader {
    None,
    NeoForge { version: String },
    Forge { version: String },
    Fabric { version: String },
    Quilt { version: String },
}

impl ModLoader {
    pub fn launch_version_id(&self, minecraft: &str) -> String {
        match self {
            ModLoader::None => minecraft.to_string(),
            ModLoader::NeoForge { version } => format!("neoforge-{version}"),
            ModLoader::Forge { version } => format!("{minecraft}-forge-{version}"),
            ModLoader::Fabric { version } => format!("fabric-loader-{version}-{minecraft}"),
            ModLoader::Quilt { version } => format!("quilt-loader-{version}-{minecraft}"),
        }
    }

    pub fn label(&self) -> String {
        match self {
            ModLoader::None => "Vanilla".into(),
            ModLoader::NeoForge { version } => format!("NeoForge {version}"),
            ModLoader::Forge { version } => format!("Forge {version}"),
            ModLoader::Fabric { version } => format!("Fabric {version}"),
            ModLoader::Quilt { version } => format!("Quilt {version}"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MmcPackJson {
    components: Vec<MmcComponent>,
}

#[derive(Debug, Deserialize)]
struct MmcComponent {
    uid: String,
    version: Option<String>,
    #[serde(rename = "cachedVersion")]
    cached_version: Option<String>,
}

/// Есть ли признаки Prism/MultiMC в корне инстанса.
pub fn is_mmc_instance(root: &Path) -> bool {
    root.join("mmc-pack.json").is_file()
        || (root.join("instance.cfg").is_file() && root.join("minecraft").is_dir())
}

/// Разобрать распакованный инстанс.
pub fn parse_instance(root: &Path) -> Result<MmcPack, LauncherError> {
    let root = normalize_root(root);
    let pack_path = root.join("mmc-pack.json");
    if !pack_path.is_file() {
        return Err(LauncherError::Other(
            "Не найден mmc-pack.json (нужен экспорт Prism/MultiMC)".into(),
        ));
    }

    let data = fs::read_to_string(&pack_path)?;
    let pack: MmcPackJson = serde_json::from_str(&data)
        .map_err(|e| LauncherError::Parse(format!("mmc-pack.json: {e}")))?;

    let mut minecraft: Option<String> = None;
    let mut loader = ModLoader::None;

    for c in &pack.components {
        let ver = c
            .version
            .clone()
            .or_else(|| c.cached_version.clone())
            .unwrap_or_default();
        if ver.is_empty() {
            continue;
        }
        match c.uid.as_str() {
            "net.minecraft" => minecraft = Some(ver),
            "net.neoforged" | "net.neoforged.neoforge" => {
                loader = ModLoader::NeoForge { version: ver };
            }
            "net.minecraftforge" => {
                loader = ModLoader::Forge { version: ver };
            }
            "net.fabricmc.fabric-loader" => {
                loader = ModLoader::Fabric { version: ver };
            }
            "org.quiltmc.quilt-loader" => {
                loader = ModLoader::Quilt { version: ver };
            }
            _ => {}
        }
    }

    let minecraft = minecraft.ok_or_else(|| {
        LauncherError::Other("В mmc-pack.json нет компонента Minecraft".into())
    })?;

    let name = read_instance_name(&root).unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "instance".into())
    });

    let max_mem_mb = read_max_mem(&root);
    let game_dir = resolve_game_dir(&root);

    Ok(MmcPack {
        name,
        minecraft,
        loader,
        game_dir,
        max_mem_mb,
    })
}

fn normalize_root(root: &Path) -> PathBuf {
    // Если zip распаковался с одной обёрткой
    if root.join("mmc-pack.json").is_file() {
        return root.to_path_buf();
    }
    if let Ok(entries) = fs::read_dir(root) {
        let dirs: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        if dirs.len() == 1 && dirs[0].join("mmc-pack.json").is_file() {
            return dirs[0].clone();
        }
    }
    root.to_path_buf()
}

fn resolve_game_dir(root: &Path) -> PathBuf {
    let mc = root.join("minecraft");
    if mc.is_dir() {
        mc
    } else if root.join(".minecraft").is_dir() {
        root.join(".minecraft")
    } else if root.join("mods").is_dir() {
        root.to_path_buf()
    } else {
        mc // создадим при запуске
    }
}

fn read_instance_name(root: &Path) -> Option<String> {
    let cfg = root.join("instance.cfg");
    let data = fs::read_to_string(cfg).ok()?;
    for line in data.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name=") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn read_max_mem(root: &Path) -> Option<u32> {
    let cfg = root.join("instance.cfg");
    let data = fs::read_to_string(cfg).ok()?;
    for line in data.lines() {
        if let Some(rest) = line.trim().strip_prefix("MaxMemAlloc=") {
            if let Ok(v) = rest.trim().parse::<u32>() {
                if (1024..=65536).contains(&v) {
                    return Some(v);
                }
            }
        }
    }
    None
}
