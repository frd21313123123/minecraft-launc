use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Pos2, Rect, RichText, Sense,
    Stroke, Vec2,
};

use mine_launcher::config::Config;
use mine_launcher::download::ProgressFn;
use mine_launcher::drive::{self, BuildInfo};
use mine_launcher::install::{self, is_version_installed};
use mine_launcher::java::{find_java, java_version_string};
use mine_launcher::mmc::{self, ModLoader};
use mine_launcher::neoforge;
use mine_launcher::paths::{ensure_dirs, game_dir};

#[derive(Clone)]
enum WorkerMsg {
    BuildsOk(Vec<BuildInfo>),
    BuildsErr(String),
    Progress { done: u64, total: u64, label: String },
    Status(String),
    DoneOk { build: String, username: String },
    DoneErr(String),
}

#[derive(PartialEq)]
enum Busy {
    Idle,
    LoadingBuilds,
    Installing,
    Launching,
}

pub struct MineLauncherApp {
    config: Config,
    username: String,
    ram_mb: u32,
    builds: Vec<BuildInfo>,
    selected_idx: usize,
    java_label: String,
    status: String,
    detail: String,
    progress: f32,
    busy: Busy,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    cancel: Arc<AtomicBool>,
    show_settings: bool,
    last_error: Option<String>,
}

impl MineLauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let _ = ensure_dirs();
        configure_style(&cc.egui_ctx);

        let config = Config::load();
        let (tx, rx) = mpsc::channel();

        let java_label = match find_java(&config.java_path) {
            Some(p) => {
                let ver = java_version_string(&p).unwrap_or_else(|| p.display().to_string());
                format!("Java: {ver}")
            }
            None => "Java: не найдена — установите Java 17+".into(),
        };

        let mut app = Self {
            username: config.username.clone(),
            ram_mb: config.ram_mb.clamp(1024, 8192),
            config,
            builds: Vec::new(),
            selected_idx: 0,
            java_label,
            status: "Загрузка списка сборок…".into(),
            detail: String::new(),
            progress: 0.0,
            busy: Busy::Idle,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            show_settings: false,
            last_error: None,
        };
        app.reload_builds();
        app
    }

    fn reload_builds(&mut self) {
        if matches!(self.busy, Busy::Installing | Busy::Launching) {
            return;
        }
        self.busy = Busy::LoadingBuilds;
        self.status = "Загрузка сборок с Google Drive…".into();
        self.detail.clear();
        let tx = self.tx.clone();
        thread::spawn(move || match drive::fetch_builds() {
            Ok(builds) => {
                let _ = tx.send(WorkerMsg::BuildsOk(builds));
            }
            Err(e) => {
                let _ = tx.send(WorkerMsg::BuildsErr(e.to_string()));
            }
        });
    }

    fn save_prefs(&mut self) {
        self.config.username = self.username.clone();
        self.config.ram_mb = self.ram_mb;
        if let Some(b) = self.builds.get(self.selected_idx) {
            self.config.last_build = b.id.clone();
        }
        let _ = self.config.save();
    }

    fn selected_build(&self) -> Option<&BuildInfo> {
        self.builds.get(self.selected_idx)
    }

    fn validate_username(&self) -> Result<(), String> {
        let u = self.username.trim();
        if u.is_empty() {
            return Err("Введите ник".into());
        }
        if u.chars().count() > 16 {
            return Err("Ник не длиннее 16 символов".into());
        }
        if !u.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err("Ник: только латиница, цифры и _".into());
        }
        Ok(())
    }

    fn on_play(&mut self) {
        if self.busy != Busy::Idle {
            return;
        }
        if let Err(e) = self.validate_username() {
            self.status = e;
            return;
        }
        let Some(build) = self.selected_build().cloned() else {
            self.status = "Сборок пока нет — залейте .zip в папку Google Drive".into();
            return;
        };
        if find_java(&self.config.java_path).is_none() {
            self.status = "Java не найдена. Установите Java 17+ (Настройки ⚙)".into();
            self.refresh_java_label();
            return;
        }

        self.save_prefs();
        self.progress = 0.0;
        self.last_error = None;
        self.cancel.store(false, Ordering::Relaxed);

        let username = self.username.trim().to_string();
        let ram = self.ram_mb;
        let java_path = self.config.java_path.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();

        let need_download = !drive::is_build_installed(&build.id);
        self.busy = if need_download {
            Busy::Installing
        } else {
            Busy::Launching
        };
        self.status = if need_download {
            format!("Скачивание «{}»…", build.name)
        } else {
            format!("Запуск «{}»…", build.name)
        };

        thread::spawn(move || {
            let progress: ProgressFn = Arc::new({
                let tx = tx.clone();
                move |done, total, label| {
                    let _ = tx.send(WorkerMsg::Progress {
                        done,
                        total,
                        label: label.to_string(),
                    });
                }
            });

            let instance = if need_download {
                match drive::install_build(&build, progress.clone(), &cancel) {
                    Ok((path, _)) => path,
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                        return;
                    }
                }
            } else {
                drive::instance_dir(&build.id)
            };

            let root = drive::resolve_instance_root(&instance);

            // ── Prism / MultiMC экспорт (createA2 и т.п.) ──
            if mmc::is_mmc_instance(&root) {
                let pack = match mmc::parse_instance(&root) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                        return;
                    }
                };

                let _ = tx.send(WorkerMsg::Status(format!(
                    "Сборка «{}»: MC {} · {}",
                    pack.name,
                    pack.minecraft,
                    pack.loader.label()
                )));

                if !is_version_installed(&pack.minecraft) {
                    let _ = tx.send(WorkerMsg::Status(format!(
                        "Установка Minecraft {}…",
                        pack.minecraft
                    )));
                    if let Err(e) =
                        install::install_version(&pack.minecraft, progress.clone(), cancel.clone())
                    {
                        let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                        return;
                    }
                }

                let launch_id = match &pack.loader {
                    ModLoader::None => pack.minecraft.clone(),
                    ModLoader::NeoForge { version } => {
                        if !neoforge::is_neoforge_installed(version) {
                            let _ = tx.send(WorkerMsg::Status(format!(
                                "Установка NeoForge {version}…"
                            )));
                            match neoforge::install_neoforge(
                                version,
                                &java_path,
                                progress.clone(),
                                cancel.clone(),
                            ) {
                                Ok(id) => id,
                                Err(e) => {
                                    let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                                    return;
                                }
                            }
                        } else {
                            neoforge::neoforge_version_id(version)
                        }
                    }
                    other => {
                        let _ = tx.send(WorkerMsg::DoneErr(format!(
                            "Пока поддерживается только NeoForge/Vanilla, а в сборке: {}",
                            other.label()
                        )));
                        return;
                    }
                };

                // Natives для loader-версии (на базе библиотек vanilla)
                let _ = install::ensure_natives_for_version(&launch_id);

                let game = pack.game_dir;
                let _ = std::fs::create_dir_all(&game);

                let _ = tx.send(WorkerMsg::Status(format!("Запуск «{}»…", pack.name)));
                match launch_game_in_dir(&launch_id, &username, ram, &java_path, &game) {
                    Ok(_child) => {
                        let _ = tx.send(WorkerMsg::DoneOk {
                            build: pack.name,
                            username,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                    }
                }
                return;
            }

            // ── Простой zip / build.json ──
            let meta = drive::read_build_meta(&root);
            let mc_version = build
                .minecraft
                .clone()
                .or_else(|| {
                    meta.as_ref()
                        .and_then(|m| m.minecraft.clone().or(m.version_id.clone()))
                })
                .or_else(|| guess_mc_version(&build.name).or_else(|| guess_mc_version(&build.id)));

            let Some(mc_version) = mc_version else {
                let _ = tx.send(WorkerMsg::DoneErr(
                    "Не удалось определить версию Minecraft. Нужен экспорт Prism (mmc-pack.json) или build.json."
                        .into(),
                ));
                return;
            };

            if !is_version_installed(&mc_version) {
                let _ = tx.send(WorkerMsg::Status(format!(
                    "Установка Minecraft {mc_version}…"
                )));
                if let Err(e) = install::install_version(&mc_version, progress, cancel) {
                    let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                    return;
                }
            }

            let game = instance_game_dir(&root);
            if let Err(e) = prepare_instance_game_dir(&root, &game) {
                let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                return;
            }

            let _ = tx.send(WorkerMsg::Status(format!("Запуск «{}»…", build.name)));
            match launch_game_in_dir(&mc_version, &username, ram, &java_path, &game) {
                Ok(_child) => {
                    let _ = tx.send(WorkerMsg::DoneOk {
                        build: build.name,
                        username,
                    });
                }
                Err(e) => {
                    let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                }
            }
        });
    }

    fn refresh_java_label(&mut self) {
        self.java_label = match find_java(&self.config.java_path) {
            Some(p) => {
                let ver = java_version_string(&p).unwrap_or_else(|| p.display().to_string());
                format!("Java: {ver}")
            }
            None => "Java: не найдена — установите Java 17+".into(),
        };
    }

    fn poll_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::BuildsOk(builds) => {
                    self.builds = builds;
                    self.busy = Busy::Idle;
                    if let Some(idx) = self
                        .builds
                        .iter()
                        .position(|b| b.id == self.config.last_build)
                    {
                        self.selected_idx = idx;
                    } else {
                        self.selected_idx = 0;
                    }
                    if self.builds.is_empty() {
                        self.status = "Сборок пока нет".into();
                        self.detail =
                            "Залейте .zip (и при желании builds.json) в папку Google Drive".into();
                    } else {
                        self.status = format!("Сборок: {}", self.builds.len());
                        self.detail.clear();
                    }
                }
                WorkerMsg::BuildsErr(e) => {
                    self.busy = Busy::Idle;
                    self.status = "Не удалось загрузить сборки".into();
                    self.detail = e;
                }
                WorkerMsg::Progress {
                    done,
                    total,
                    label,
                } => {
                    let t = total.max(1) as f32;
                    self.progress = (done as f32 / t).clamp(0.0, 1.0);
                    self.detail = label;
                    self.status = format!("Загрузка… {done}/{total}");
                }
                WorkerMsg::Status(s) => {
                    self.status = s;
                }
                WorkerMsg::DoneOk { build, username } => {
                    self.busy = Busy::Idle;
                    self.progress = 1.0;
                    self.status = format!("Запущено: {username} · {build}");
                    self.detail = "Приятной игры!".into();
                }
                WorkerMsg::DoneErr(e) => {
                    self.busy = Busy::Idle;
                    self.progress = 0.0;
                    self.status = format!("Ошибка: {e}");
                    self.detail = e.clone();
                    self.last_error = Some(e);
                }
            }
        }
    }
}

fn instance_game_dir(root: &std::path::Path) -> PathBuf {
    // Если в zip уже есть .minecraft-подобная структура — используем root.
    if root.join("mods").is_dir()
        || root.join("config").is_dir()
        || root.join("saves").is_dir()
        || root.join("options.txt").is_file()
    {
        return root.to_path_buf();
    }
    root.join("minecraft")
}

fn prepare_instance_game_dir(
    root: &std::path::Path,
    game: &std::path::Path,
) -> Result<(), mine_launcher::error::LauncherError> {
    std::fs::create_dir_all(game)?;
    // Если game == root — уже готово.
    if game == root {
        return Ok(());
    }
    // Иначе переносим типичные папки модпака в game dir.
    for name in [
        "mods",
        "config",
        "resourcepacks",
        "shaderpacks",
        "saves",
        "options.txt",
        "optionsof.txt",
        "servers.dat",
    ] {
        let src = root.join(name);
        let dst = game.join(name);
        if src.exists() && !dst.exists() {
            if src.is_dir() {
                copy_dir_recursive(&src, &dst)?;
            } else {
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::copy(&src, &dst)?;
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<(), mine_launcher::error::LauncherError> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn guess_mc_version(s: &str) -> Option<String> {
    // 1.20.1 / 1.21 / 1.16.5-Fabric
    let re = regex_lite::Regex::new(r"(1\.\d{1,2}(?:\.\d{1,2})?)").ok()?;
    re.captures(s)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn launch_game_in_dir(
    version_id: &str,
    username: &str,
    ram_mb: u32,
    java_path: &str,
    game_dir_override: &std::path::Path,
) -> Result<std::process::Child, mine_launcher::error::LauncherError> {
    mine_launcher::launch::launch_game_with_dir(
        version_id,
        username,
        ram_mb,
        java_path,
        game_dir_override,
    )
}

impl eframe::App for MineLauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();

        if self.busy != Busy::Idle {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let busy = self.busy != Busy::Idle;

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(Color32::from_rgb(14, 16, 18)))
            .show(ctx, |ui| {
                let full = ui.max_rect();

                // ── Settings gear (top-right) ──
                let gear_size = Vec2::new(36.0, 36.0);
                let gear_pos = Pos2::new(full.right() - 20.0 - gear_size.x, full.top() + 16.0);
                let gear_rect = Rect::from_min_size(gear_pos, gear_size);
                let gear_resp = ui.interact(gear_rect, ui.id().with("gear"), Sense::click());
                let gear_color = if gear_resp.hovered() {
                    Color32::from_rgb(220, 220, 220)
                } else {
                    Color32::from_rgb(150, 150, 150)
                };
                ui.painter().text(
                    gear_rect.center(),
                    Align2::CENTER_CENTER,
                    "⚙",
                    FontId::proportional(22.0),
                    gear_color,
                );
                if gear_resp.clicked() {
                    self.show_settings = !self.show_settings;
                    self.refresh_java_label();
                }
                gear_resp.on_hover_text("Настройки");

                // ── Bottom bar ──
                let bar_h = 72.0;
                let bar = Rect::from_min_max(
                    Pos2::new(full.left(), full.bottom() - bar_h),
                    full.right_bottom(),
                );
                ui.painter().rect_filled(
                    bar,
                    CornerRadius::ZERO,
                    Color32::from_rgb(22, 24, 28),
                );
                ui.painter().line_segment(
                    [bar.left_top(), bar.right_top()],
                    Stroke::new(1.0, Color32::from_rgb(45, 48, 55)),
                );

                // Bottom fields
                let field_h = 34.0;
                let field_y = bar.center().y - field_h / 2.0;
                let pad = 24.0;
                let gap = 24.0;
                let field_w = ((bar.width() - pad * 2.0 - gap) / 2.0).clamp(140.0, 280.0);

                // Username
                let nick_rect = Rect::from_min_size(
                    Pos2::new(bar.left() + pad, field_y),
                    Vec2::new(field_w, field_h),
                );
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(nick_rect), |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.username)
                                .desired_width(field_w)
                                .hint_text("Ник человека")
                                .char_limit(16)
                                .frame(true)
                                .margin(egui::Margin::symmetric(10, 6)),
                        );
                    });
                });

                // Build combo
                let build_rect = Rect::from_min_size(
                    Pos2::new(bar.right() - pad - field_w, field_y),
                    Vec2::new(field_w, field_h),
                );
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(build_rect), |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        let selected_text = if matches!(self.busy, Busy::LoadingBuilds) {
                            "Загрузка…".to_string()
                        } else if self.builds.is_empty() {
                            "Сборок пока нет".to_string()
                        } else {
                            self.builds
                                .get(self.selected_idx)
                                .map(|b| {
                                    let mark = if drive::is_build_installed(&b.id) {
                                        " ✓"
                                    } else {
                                        ""
                                    };
                                    format!("{}{mark}", b.name)
                                })
                                .unwrap_or_else(|| "Сборка".into())
                        };

                        egui::ComboBox::from_id_salt("build_combo")
                            .selected_text(selected_text)
                            .width(field_w)
                            .show_ui(ui, |ui| {
                                if self.builds.is_empty() {
                                    ui.label(
                                        RichText::new("Нет сборок на Drive")
                                            .color(Color32::from_rgb(140, 140, 140)),
                                    );
                                } else {
                                    for (i, b) in self.builds.iter().enumerate() {
                                        let mark = if drive::is_build_installed(&b.id) {
                                            " ✓"
                                        } else {
                                            ""
                                        };
                                        let label = format!("{}{mark}", b.name);
                                        if ui
                                            .selectable_label(self.selected_idx == i, label)
                                            .clicked()
                                        {
                                            self.selected_idx = i;
                                        }
                                    }
                                }
                            });
                    });
                });

                // ── Main area (above bar) ──
                let main = Rect::from_min_max(full.left_top(), Pos2::new(full.right(), bar.top()));

                // Status above play button
                let status_y = main.center().y - 90.0;
                ui.painter().text(
                    Pos2::new(main.center().x, status_y),
                    Align2::CENTER_CENTER,
                    &self.status,
                    FontId::proportional(14.0),
                    Color32::from_rgb(170, 175, 180),
                );
                if !self.detail.is_empty() {
                    ui.painter().text(
                        Pos2::new(main.center().x, status_y + 20.0),
                        Align2::CENTER_CENTER,
                        truncate(&self.detail, 80),
                        FontId::proportional(12.0),
                        Color32::from_rgb(110, 115, 120),
                    );
                }

                // Circular Play button
                let radius = 64.0;
                let center = main.center() + Vec2::new(0.0, 10.0);
                let circle_rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
                let can_play = !busy && !self.builds.is_empty();
                let play_resp = ui.interact(circle_rect, ui.id().with("play"), Sense::click());

                let (fill, stroke_c, text_c) = if !can_play {
                    (
                        Color32::from_rgb(45, 50, 55),
                        Color32::from_rgb(70, 75, 80),
                        Color32::from_rgb(130, 135, 140),
                    )
                } else if play_resp.is_pointer_button_down_on() {
                    (
                        Color32::from_rgb(28, 160, 80),
                        Color32::from_rgb(90, 220, 140),
                        Color32::WHITE,
                    )
                } else if play_resp.hovered() {
                    (
                        Color32::from_rgb(40, 185, 100),
                        Color32::from_rgb(120, 240, 160),
                        Color32::WHITE,
                    )
                } else {
                    (
                        Color32::from_rgb(34, 170, 90),
                        Color32::from_rgb(80, 210, 130),
                        Color32::WHITE,
                    )
                };

                ui.painter()
                    .circle_filled(center, radius, fill);
                ui.painter()
                    .circle_stroke(center, radius, Stroke::new(2.5, stroke_c));

                let play_label = match self.busy {
                    Busy::Idle => "Играть",
                    Busy::LoadingBuilds => "…",
                    Busy::Installing => "…",
                    Busy::Launching => "…",
                };
                ui.painter().text(
                    center,
                    Align2::CENTER_CENTER,
                    play_label,
                    FontId::proportional(22.0),
                    text_c,
                );

                if can_play && play_resp.clicked() {
                    self.on_play();
                }
                if play_resp.hovered() && can_play {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }

                // Progress ring / bar under circle when busy
                if busy {
                    let bar_w = 220.0;
                    let pbar = Rect::from_center_size(
                        center + Vec2::new(0.0, radius + 28.0),
                        Vec2::new(bar_w, 6.0),
                    );
                    ui.painter().rect_filled(
                        pbar,
                        CornerRadius::same(3),
                        Color32::from_rgb(40, 44, 48),
                    );
                    let filled = Rect::from_min_size(
                        pbar.min,
                        Vec2::new(pbar.width() * self.progress.clamp(0.0, 1.0), pbar.height()),
                    );
                    ui.painter().rect_filled(
                        filled,
                        CornerRadius::same(3),
                        Color32::from_rgb(50, 200, 110),
                    );
                }

                // Settings panel overlay
                if self.show_settings {
                    self.draw_settings(ui, full, busy);
                }
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_prefs();
    }
}

impl MineLauncherApp {
    fn draw_settings(&mut self, ui: &mut egui::Ui, full: Rect, busy: bool) {
        // Dim background
        let dim = ui.interact(full, ui.id().with("settings_dim"), Sense::click());
        ui.painter()
            .rect_filled(full, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(0, 0, 0, 160));
        if dim.clicked() {
            self.show_settings = false;
            self.save_prefs();
        }

        let panel_w = 360.0;
        let panel_h = 320.0;
        let panel = Rect::from_center_size(full.center(), Vec2::new(panel_w, panel_h));
        ui.painter().rect_filled(
            panel,
            CornerRadius::same(12),
            Color32::from_rgb(28, 32, 36),
        );
        ui.painter().rect_stroke(
            panel,
            CornerRadius::same(12),
            Stroke::new(1.0, Color32::from_rgb(60, 65, 70)),
            egui::StrokeKind::Outside,
        );

        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(panel.shrink(18.0))
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                // Stop click-through
                let _ = ui.interact(
                    ui.max_rect(),
                    ui.id().with("settings_panel"),
                    Sense::click(),
                );

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Настройки")
                            .size(18.0)
                            .strong()
                            .color(Color32::from_rgb(230, 230, 230)),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("✕").clicked() {
                            self.show_settings = false;
                            self.save_prefs();
                        }
                    });
                });
                ui.add_space(12.0);

                ui.label(RichText::new("ОЗУ (МБ)").size(13.0));
                ui.add_enabled_ui(!busy, |ui| {
                    let mut ram = self.ram_mb as f32;
                    if ui
                        .add(
                            egui::Slider::new(&mut ram, 1024.0..=8192.0)
                                .step_by(512.0)
                                .suffix(" МБ"),
                        )
                        .changed()
                    {
                        self.ram_mb =
                            ((ram / 512.0).round() as u32 * 512).clamp(1024, 8192);
                    }
                });

                ui.add_space(10.0);
                ui.label(RichText::new("Путь к Java (пусто = авто)").size(13.0));
                ui.add_enabled_ui(!busy, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.java_path)
                            .desired_width(f32::INFINITY)
                            .hint_text("C:\\Program Files\\Java\\...\\javaw.exe"),
                    );
                });
                ui.label(
                    RichText::new(&self.java_label)
                        .size(11.0)
                        .color(Color32::from_rgb(140, 140, 140)),
                );

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy, egui::Button::new("Обновить сборки"))
                        .clicked()
                    {
                        self.reload_builds();
                    }
                    if ui.button("Открыть папку игры").clicked() {
                        let _ = open_path(&game_dir());
                    }
                });

                ui.add_space(8.0);
                if ui.link("Папка сборок на Google Drive").clicked() {
                    let _ = open_url(&drive::folder_url());
                }

                ui.add_space(10.0);
                ui.label(
                    RichText::new("Формат: экспорт Prism/MultiMC (mmc-pack.json + minecraft/)")
                        .size(11.0)
                        .color(Color32::from_rgb(100, 105, 110)),
                );
            },
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
        Ok(())
    }
}

fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    ctx.set_style(style);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(14, 16, 18);
    visuals.window_fill = Color32::from_rgb(28, 32, 36);
    visuals.extreme_bg_color = Color32::from_rgb(20, 22, 26);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(36, 40, 46);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 54, 62);
    visuals.widgets.active.bg_fill = Color32::from_rgb(40, 120, 70);
    visuals.selection.bg_fill = Color32::from_rgb(34, 120, 70);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(200, 200, 200));
    ctx.set_visuals(visuals);
}
