#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use mine_launcher::paths;

mod app;

use app::MineLauncherApp;

fn main() -> eframe::Result<()> {
    let _ = paths::ensure_dirs();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 520.0])
            .with_min_inner_size([720.0, 440.0])
            .with_title("MineLauncher"),
        ..Default::default()
    };

    eframe::run_native(
        "MineLauncher",
        options,
        Box::new(|cc| Ok(Box::new(MineLauncherApp::new(cc)))),
    )
}
