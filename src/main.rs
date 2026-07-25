#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod download;
mod error;
mod install;
mod java;
mod launch;
mod models;
mod paths;
mod rules;

use app::MineLauncherApp;
use eframe::egui;

fn main() -> eframe::Result<()> {
    let _ = paths::ensure_dirs();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([640.0, 500.0])
            .with_title("MineLauncher — Minecraft"),
        ..Default::default()
    };

    eframe::run_native(
        "MineLauncher",
        options,
        Box::new(|cc| Ok(Box::new(MineLauncherApp::new(cc)))),
    )
}
