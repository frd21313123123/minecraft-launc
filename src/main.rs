#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use mine_launcher::paths;

mod app;

use app::MineLauncherApp;

fn main() -> eframe::Result<()> {
    if let Some(exit_code) = mine_launcher::updater::handle_startup_args() {
        std::process::exit(exit_code);
    }

    let _ = paths::ensure_dirs();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 760.0])
            .with_min_inner_size([960.0, 620.0])
            .with_title("MineLauncher Beta"),
        ..Default::default()
    };

    eframe::run_native(
        "MineLauncher",
        options,
        Box::new(|cc| Ok(Box::new(MineLauncherApp::new(cc)))),
    )
}
