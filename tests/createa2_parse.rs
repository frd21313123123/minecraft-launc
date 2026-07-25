use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use mine_launcher::install;
use mine_launcher::mmc;
use mine_launcher::neoforge;
use mine_launcher::paths;

#[test]
fn parse_createa2_instance() {
    let root = paths::instances_dir().join("createa2");
    assert!(
        root.join("mmc-pack.json").is_file(),
        "Сначала распакуйте createA2.zip в {}",
        root.display()
    );
    assert!(mmc::is_mmc_instance(&root));
    let pack = mmc::parse_instance(&root).expect("parse");
    assert_eq!(pack.minecraft, "1.21.1");
    match pack.loader {
        mmc::ModLoader::NeoForge { ref version } => assert_eq!(version, "21.1.238"),
        other => panic!("expected NeoForge, got {}", other.label()),
    }
    assert!(pack.game_dir.ends_with("minecraft"));
    assert!(pack.game_dir.is_dir());
    println!(
        "OK {} / {} / {}",
        pack.name,
        pack.minecraft,
        pack.loader.label()
    );
}

/// Долгий тест: ставит vanilla + NeoForge. Запуск: cargo test -- --ignored --nocapture
#[test]
#[ignore]
fn install_createa2_runtime() {
    let root = paths::instances_dir().join("createa2");
    let pack = mmc::parse_instance(&root).expect("parse");
    let progress = Arc::new(|done: u64, total: u64, label: &str| {
        println!("[{done}/{total}] {label}");
    });
    let cancel = Arc::new(AtomicBool::new(false));

    install::install_version(&pack.minecraft, progress.clone(), cancel.clone()).expect("mc");
    if let mmc::ModLoader::NeoForge { version } = &pack.loader {
        neoforge::install_neoforge(version, "", progress, cancel).expect("neoforge");
        let id = neoforge::neoforge_version_id(version);
        assert!(install::is_version_installed(&id));
        install::ensure_natives_for_version(&id).expect("natives");
        let version_json = install::load_version_json(&id).expect("json");
        assert!(!version_json.main_class.is_empty());
        let jar = install::client_jar_path(&version_json);
        assert!(jar.is_file(), "client jar missing: {}", jar.display());
        println!("launch id={id} jar={}", jar.display());
    }
}
