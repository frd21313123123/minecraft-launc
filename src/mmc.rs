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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "mine-launcher-mmc-test-{}-{timestamp}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_wrapped_prism_export_with_metadata() {
        let temp = TestDir::new();
        let instance = temp.path().join("create-a2");
        fs::create_dir_all(instance.join(".minecraft")).unwrap();
        fs::write(
            instance.join("mmc-pack.json"),
            r#"{
                "components": [
                    {"uid": "net.minecraft", "cachedVersion": "1.21.1"},
                    {"uid": "net.neoforged.neoforge", "version": "21.1.238"}
                ]
            }"#,
        )
        .unwrap();
        fs::write(
            instance.join("instance.cfg"),
            "name=\"Create A2\"\nMaxMemAlloc=4096\n",
        )
        .unwrap();

        assert!(is_mmc_instance(&instance));
        let pack = parse_instance(temp.path()).unwrap();

        assert_eq!(pack.name, "Create A2");
        assert_eq!(pack.minecraft, "1.21.1");
        assert!(matches!(
            pack.loader,
            ModLoader::NeoForge { ref version } if version == "21.1.238"
        ));
        assert_eq!(pack.game_dir, instance.join(".minecraft"));
        assert_eq!(pack.max_mem_mb, Some(4096));
    }

    #[test]
    fn reports_an_error_when_the_minecraft_component_is_missing() {
        let temp = TestDir::new();
        fs::write(
            temp.path().join("mmc-pack.json"),
            r#"{"components":[{"uid":"net.fabricmc.fabric-loader","version":"0.16.10"}]}"#,
        )
        .unwrap();

        let error = parse_instance(temp.path()).unwrap_err();

        assert!(error.to_string().contains("нет компонента Minecraft"));
    }

    #[test]
    fn loader_version_ids_and_labels_follow_each_loader_convention() {
        let cases = [
            (ModLoader::None, "1.21.1", "1.21.1", "Vanilla"),
            (
                ModLoader::NeoForge {
                    version: "21.1.238".into(),
                },
                "1.21.1",
                "neoforge-21.1.238",
                "NeoForge 21.1.238",
            ),
            (
                ModLoader::Forge {
                    version: "47.3.0".into(),
                },
                "1.20.1",
                "1.20.1-forge-47.3.0",
                "Forge 47.3.0",
            ),
            (
                ModLoader::Fabric {
                    version: "0.16.10".into(),
                },
                "1.21.1",
                "fabric-loader-0.16.10-1.21.1",
                "Fabric 0.16.10",
            ),
            (
                ModLoader::Quilt {
                    version: "0.27.1".into(),
                },
                "1.21.1",
                "quilt-loader-0.27.1-1.21.1",
                "Quilt 0.27.1",
            ),
        ];

        for (loader, minecraft, expected_id, expected_label) in cases {
            assert_eq!(loader.launch_version_id(minecraft), expected_id);
            assert_eq!(loader.label(), expected_label);
        }
    }
}
