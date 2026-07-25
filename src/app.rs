use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Vec2};
use crate::config::Config;
use crate::download::ProgressFn;
use crate::install::{self, fetch_versions, installed_versions, is_version_installed};
use crate::java::{find_java, java_version_string};
use crate::launch::launch_game;
use crate::models::VersionInfo;
use crate::paths::{ensure_dirs, game_dir};

#[derive(Clone)]
enum WorkerMsg {
    VersionsOk(Vec<VersionInfo>, Vec<String>),
    VersionsErr(String),
    Progress { done: u64, total: u64, label: String },
    Status(String),
    DoneOk { version: String, username: String },
    DoneErr(String),
}

#[derive(PartialEq)]
enum Busy {
    Idle,
    LoadingVersions,
    Installing,
    Launching,
}

pub struct MineLauncherApp {
    config: Config,
    username: String,
    ram_mb: u32,
    show_snapshots: bool,
    versions: Vec<VersionInfo>,
    installed: Vec<String>,
    selected_idx: usize,
    java_label: String,
    status: String,
    detail: String,
    progress: f32,
    busy: Busy,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    cancel: Arc<AtomicBool>,
    /// shared progress for worker
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
            None => "Java: не найдена — установите Java 17+ (Temurin / Microsoft / Oracle)".into(),
        };

        let mut app = Self {
            username: config.username.clone(),
            ram_mb: config.ram_mb.clamp(1024, 8192),
            show_snapshots: config.show_snapshots,
            config,
            versions: Vec::new(),
            installed: Vec::new(),
            selected_idx: 0,
            java_label,
            status: "Загрузка списка версий…".into(),
            detail: String::new(),
            progress: 0.0,
            busy: Busy::Idle,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            last_error: None,
        };
        app.reload_versions();
        app
    }

    fn reload_versions(&mut self) {
        if matches!(self.busy, Busy::Installing | Busy::Launching) {
            return;
        }
        self.busy = Busy::LoadingVersions;
        self.status = "Загрузка списка версий с серверов Mojang…".into();
        self.detail.clear();
        let snapshots = self.show_snapshots;
        let tx = self.tx.clone();
        thread::spawn(move || {
            match fetch_versions(snapshots) {
                Ok(versions) => {
                    let installed = installed_versions();
                    let _ = tx.send(WorkerMsg::VersionsOk(versions, installed));
                }
                Err(e) => {
                    let _ = tx.send(WorkerMsg::VersionsErr(e.to_string()));
                }
            }
        });
    }

    fn save_prefs(&mut self) {
        self.config.username = self.username.clone();
        self.config.ram_mb = self.ram_mb;
        self.config.show_snapshots = self.show_snapshots;
        if let Some(v) = self.versions.get(self.selected_idx) {
            self.config.last_version = v.id.clone();
        }
        let _ = self.config.save();
    }

    fn selected_version_id(&self) -> Option<String> {
        self.versions.get(self.selected_idx).map(|v| v.id.clone())
    }

    fn validate_username(&self) -> Result<(), String> {
        let u = self.username.trim();
        if u.is_empty() {
            return Err("Введите ник для игры".into());
        }
        if u.chars().count() > 16 {
            return Err("Ник не длиннее 16 символов".into());
        }
        if !u.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err("Ник: только латинские буквы, цифры и _".into());
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
        let Some(version_id) = self.selected_version_id() else {
            self.status = "Выберите версию Minecraft".into();
            return;
        };
        if find_java(&self.config.java_path).is_none() {
            self.status = "Java не найдена. Установите Java 17+ и перезапустите.".into();
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

        let need_install = !is_version_installed(&version_id);
        self.busy = if need_install {
            Busy::Installing
        } else {
            Busy::Launching
        };
        self.status = if need_install {
            format!("Скачивание Minecraft {version_id}…")
        } else {
            format!("Запуск Minecraft {version_id}…")
        };

        thread::spawn(move || {
            if need_install {
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
                if let Err(e) = install::install_version(&version_id, progress, cancel) {
                    let _ = tx.send(WorkerMsg::DoneErr(e.to_string()));
                    return;
                }
            }

            let _ = tx.send(WorkerMsg::Status(format!(
                "Запуск Minecraft {version_id}…"
            )));
            match launch_game(&version_id, &username, ram, &java_path) {
                Ok(_child) => {
                    let _ = tx.send(WorkerMsg::DoneOk {
                        version: version_id,
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
            None => "Java: не найдена — установите Java 17+ (Temurin / Microsoft / Oracle)".into(),
        };
    }

    fn poll_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::VersionsOk(versions, installed) => {
                    self.versions = versions;
                    self.installed = installed;
                    self.busy = Busy::Idle;
                    // restore last version
                    if let Some(idx) = self
                        .versions
                        .iter()
                        .position(|v| v.id == self.config.last_version)
                    {
                        self.selected_idx = idx;
                    } else {
                        self.selected_idx = 0;
                    }
                    self.status = format!(
                        "Доступно версий: {}  |  Установлено: {}",
                        self.versions.len(),
                        self.installed.len()
                    );
                    self.detail.clear();
                }
                WorkerMsg::VersionsErr(e) => {
                    self.busy = Busy::Idle;
                    self.status = "Не удалось загрузить список версий".into();
                    self.detail = e;
                }
                WorkerMsg::Progress { done, total, label } => {
                    let t = total.max(1) as f32;
                    self.progress = (done as f32 / t).clamp(0.0, 1.0);
                    self.detail = label;
                    self.status = format!("Загрузка… {done}/{total}");
                }
                WorkerMsg::Status(s) => {
                    self.status = s;
                }
                WorkerMsg::DoneOk { version, username } => {
                    self.busy = Busy::Idle;
                    self.progress = 1.0;
                    self.status = format!("Игра запущена: {username} · {version}");
                    self.detail = "Можно свернуть лаунчер. Приятной игры!".into();
                    self.installed = installed_versions();
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

impl eframe::App for MineLauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();

        // keep UI responsive during downloads
        if self.busy != Busy::Idle {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let busy = self.busy != Busy::Idle;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);

            // Header
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("⛏  MineLauncher")
                            .size(28.0)
                            .color(Color32::from_rgb(110, 231, 160))
                            .strong(),
                    );
                    ui.label(
                        RichText::new("Выбери ник и версию — лаунчер скачает всё сам")
                            .size(13.0)
                            .color(Color32::from_rgb(160, 160, 160)),
                    );
                });
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            egui::Frame::group(ui.style())
                .inner_margin(18.0)
                .corner_radius(10.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());

                    ui.label(RichText::new("Ник в игре").strong().size(14.0));
                    ui.add_space(4.0);
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.username)
                                .desired_width(f32::INFINITY)
                                .hint_text("Например: Steve")
                                .char_limit(16),
                        );
                    });

                    ui.add_space(14.0);
                    ui.label(RichText::new("Версия Minecraft").strong().size(14.0));
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!busy && !self.versions.is_empty(), |ui| {
                            let labels: Vec<String> = self
                                .versions
                                .iter()
                                .map(|v| {
                                    let mark = if self.installed.iter().any(|i| i == &v.id) {
                                        " ✓"
                                    } else {
                                        ""
                                    };
                                    format!("{}{mark}", v.label)
                                })
                                .collect();

                            let selected_text = labels
                                .get(self.selected_idx)
                                .cloned()
                                .unwrap_or_else(|| {
                                    if matches!(self.busy, Busy::LoadingVersions) {
                                        "Загрузка…".into()
                                    } else {
                                        "Нет версий".into()
                                    }
                                });

                            egui::ComboBox::from_id_salt("version_combo")
                                .selected_text(selected_text)
                                .width(ui.available_width() - 48.0)
                                .show_ui(ui, |ui| {
                                    for (i, label) in labels.iter().enumerate() {
                                        if ui
                                            .selectable_label(self.selected_idx == i, label)
                                            .clicked()
                                        {
                                            self.selected_idx = i;
                                        }
                                    }
                                });
                        });

                        if ui
                            .add_enabled(!busy, egui::Button::new("↻").min_size(Vec2::new(36.0, 28.0)))
                            .on_hover_text("Обновить список версий")
                            .clicked()
                        {
                            self.reload_versions();
                        }
                    });

                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.label("ОЗУ (МБ):");
                        ui.add_enabled_ui(!busy, |ui| {
                            let mut ram = self.ram_mb as f32;
                            if ui
                                .add(
                                    egui::Slider::new(&mut ram, 1024.0..=8192.0)
                                        .step_by(512.0)
                                        .show_value(false),
                                )
                                .changed()
                            {
                                self.ram_mb = ((ram / 512.0).round() as u32 * 512).clamp(1024, 8192);
                            }
                        });
                        ui.label(
                            RichText::new(format!("{} МБ", self.ram_mb))
                                .strong()
                                .monospace(),
                        );
                    });

                    ui.add_space(8.0);
                    ui.add_enabled_ui(!busy, |ui| {
                        if ui
                            .checkbox(&mut self.show_snapshots, "Показывать snapshot-версии")
                            .changed()
                        {
                            self.reload_versions();
                        }
                    });

                    ui.add_space(10.0);
                    let java_color = if self.java_label.contains("не найдена") {
                        Color32::from_rgb(251, 191, 36)
                    } else {
                        Color32::from_rgb(140, 140, 140)
                    };
                    ui.label(RichText::new(&self.java_label).size(12.0).color(java_color));

                    ui.add_space(16.0);
                    ui.label(RichText::new(&self.status).size(14.0));
                    ui.add_space(6.0);
                    let progress = self.progress;
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .desired_height(12.0)
                            .animate(busy),
                    );
                    if !self.detail.is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(&self.detail)
                                .size(11.0)
                                .color(Color32::from_rgb(130, 130, 130)),
                        );
                    }

                    ui.add_space(18.0);
                    let play_text = match self.busy {
                        Busy::Idle => "ИГРАТЬ".to_string(),
                        Busy::LoadingVersions => "Загрузка версий…".into(),
                        Busy::Installing => "Скачивание…".into(),
                        Busy::Launching => "Запуск…".into(),
                    };

                    ui.add_enabled_ui(!busy && !self.versions.is_empty(), |ui| {
                        let btn = egui::Button::new(
                            RichText::new(play_text)
                                .size(18.0)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .min_size(Vec2::new(ui.available_width(), 48.0))
                        .fill(Color32::from_rgb(34, 197, 94))
                        .sense(Sense::click());

                        if ui.add(btn).clicked() {
                            self.on_play();
                        }
                    });
                });

            ui.add_space(10.0);
            ui.with_layout(Layout::left_to_right(Align::BOTTOM), |ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Игровые файлы: {}", game_dir().display()))
                        .size(11.0)
                        .color(Color32::from_rgb(110, 110, 110)),
                );
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_prefs();
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    ctx.set_style(style);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(18, 22, 18);
    visuals.window_fill = Color32::from_rgb(24, 30, 24);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(40, 50, 40);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(50, 70, 50);
    visuals.selection.bg_fill = Color32::from_rgb(34, 120, 70);
    ctx.set_visuals(visuals);
}

