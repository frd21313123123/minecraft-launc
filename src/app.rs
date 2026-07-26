use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Pos2, Rect, RichText,
    Sense, Stroke, Vec2,
};

use mine_launcher::config::{AccountConfig, Config, SkinModel, Theme};
use mine_launcher::download::ProgressFn;
use mine_launcher::drive::{self, BuildInfo};
use mine_launcher::install::{self, is_version_installed};
use mine_launcher::java::{find_java, java_version_string};
use mine_launcher::mmc::{self, ModLoader};
use mine_launcher::neoforge;
use mine_launcher::paths::{
    builds_root, ensure_dirs, instance_dir, instances_dir, last_launch_log, screenshots_dir,
    set_builds_root,
};
use mine_launcher::skin_sync::{self, SkinSyncOutcome};

#[derive(Clone)]
enum WorkerMsg {
    BuildsOk(Vec<BuildInfo>),
    BuildsErr(String),
    Progress {
        done: u64,
        total: u64,
        label: String,
    },
    Status(String),
    DoneOk {
        build: String,
        username: String,
        skin_sync: Option<Result<SkinSyncOutcome, String>>,
    },
    DoneErr(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    LoadingBuilds,
    Installing,
    Launching,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Skins,
    Gallery,
    Console,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Accounts,
    About,
}

const PAGE_TRANSITION_SECONDS: f32 = 0.32;

struct GalleryItem {
    path: PathBuf,
    name: String,
    texture: egui::TextureHandle,
    aspect: f32,
}

pub struct MineLauncherApp {
    config: Config,
    username: String,
    ram_mb: u32,
    steve_texture: egui::TextureHandle,
    builds: Vec<BuildInfo>,
    selected_idx: usize,
    java_label: String,
    status: String,
    detail: String,
    progress: f32,
    progress_indeterminate: bool,
    progress_text: String,
    busy: Busy,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    cancel: Arc<AtomicBool>,
    page: Page,
    settings_tab: SettingsTab,
    transition_started: Option<Instant>,
    transition_direction: f32,
    storage_path_edit: String,
    gallery: Vec<GalleryItem>,
    gallery_status: String,
    launcher_log: String,
    game_log: String,
    console_filter: String,
    last_log_refresh: Instant,
    skin_editor_open: bool,
    skin_source_draft: String,
    skin_model_draft: SkinModel,
    account_draft: AccountConfig,
    editing_account: Option<usize>,
    account_message: String,
}

impl MineLauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut config = Config::load();
        config.normalize();
        set_builds_root(configured_builds_root(&config));
        let _ = ensure_dirs();
        configure_style(&cc.egui_ctx, config.theme);
        let steve_texture = config
            .accounts
            .get(config.active_account)
            .filter(|account| is_local_skin_source(&account.skin_source))
            .and_then(|account| {
                load_skin_texture(&cc.egui_ctx, Path::new(&account.skin_source)).ok()
            })
            .unwrap_or_else(|| load_steve_texture(&cc.egui_ctx));
        let storage_path_edit = display_builds_root(&config);
        let (tx, rx) = mpsc::channel();

        let java_label = java_status(&config.java_path);
        let username = config
            .accounts
            .get(config.active_account)
            .map(|account| account.username.clone())
            .unwrap_or_else(|| config.username.clone());

        let mut app = Self {
            username,
            ram_mb: config.ram_mb.clamp(1024, 16384),
            steve_texture,
            config,
            builds: Vec::new(),
            selected_idx: 0,
            java_label,
            status: "Подключение к каталогу сборок…".into(),
            detail: "Подготавливаем лаунчер".into(),
            progress: 0.0,
            progress_indeterminate: true,
            progress_text: String::new(),
            busy: Busy::Idle,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            page: Page::Home,
            settings_tab: SettingsTab::General,
            transition_started: None,
            transition_direction: 1.0,
            storage_path_edit,
            gallery: Vec::new(),
            gallery_status: String::new(),
            launcher_log: String::new(),
            game_log: String::new(),
            console_filter: String::new(),
            last_log_refresh: Instant::now() - Duration::from_secs(5),
            skin_editor_open: false,
            skin_source_draft: String::new(),
            skin_model_draft: SkinModel::Classic,
            account_draft: empty_account(),
            editing_account: None,
            account_message: String::new(),
        };

        app.append_log("Лаунчер запущен");
        app.reload_gallery(&cc.egui_ctx);
        app.refresh_game_log();
        app.reload_builds();
        app
    }

    fn navigation_rank(page: Page, settings_tab: SettingsTab) -> i32 {
        match page {
            Page::Home => 0,
            Page::Skins => 1,
            Page::Gallery => 2,
            Page::Console => 3,
            Page::Settings => {
                10 + match settings_tab {
                    SettingsTab::General => 0,
                    SettingsTab::Accounts => 1,
                    SettingsTab::About => 2,
                }
            }
        }
    }

    fn navigate_to(&mut self, ctx: &egui::Context, page: Page, settings_tab: Option<SettingsTab>) {
        let target_tab = settings_tab.unwrap_or(self.settings_tab);
        if self.page == page && (page != Page::Settings || self.settings_tab == target_tab) {
            return;
        }

        let from = Self::navigation_rank(self.page, self.settings_tab);
        let to = Self::navigation_rank(page, target_tab);
        self.transition_direction = if to < from { -1.0 } else { 1.0 };
        self.page = page;
        self.settings_tab = target_tab;
        self.transition_started = Some(Instant::now());
        ctx.request_repaint();
    }

    fn transition_frame(&mut self, ctx: &egui::Context) -> (f32, f32, f32, bool) {
        let Some(started) = self.transition_started else {
            return (0.0, 1.0, 1.0, false);
        };

        let linear = (started.elapsed().as_secs_f32() / PAGE_TRANSITION_SECONDS).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - linear).powi(3);
        let offset = self.transition_direction * 28.0 * (1.0 - eased);
        let opacity = 0.28 + 0.72 * eased;

        if linear < 1.0 {
            ctx.request_repaint();
        } else {
            self.transition_started = None;
        }

        (offset, opacity, eased, linear < 1.0)
    }

    fn append_log(&mut self, message: impl AsRef<str>) {
        if !self.launcher_log.is_empty() {
            self.launcher_log.push('\n');
        }
        self.launcher_log.push_str("› ");
        self.launcher_log.push_str(message.as_ref());
        if self.launcher_log.chars().count() > 80_000 {
            self.launcher_log = self
                .launcher_log
                .chars()
                .rev()
                .take(60_000)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
        }
    }

    fn reload_builds(&mut self) {
        if matches!(self.busy, Busy::Installing | Busy::Launching) {
            return;
        }
        self.busy = Busy::LoadingBuilds;
        self.status = "Загрузка списка сборок…".into();
        self.detail = "Получаем данные с Google Drive".into();
        self.progress_indeterminate = true;
        self.append_log("Запрошено обновление каталога сборок");
        let tx = self.tx.clone();
        thread::spawn(move || match drive::fetch_builds() {
            Ok(builds) => {
                let _ = tx.send(WorkerMsg::BuildsOk(builds));
            }
            Err(error) => {
                let _ = tx.send(WorkerMsg::BuildsErr(error.to_string()));
            }
        });
    }

    fn save_prefs(&mut self) {
        self.config.username = self.username.trim().to_string();
        self.config.ram_mb = self.ram_mb;
        if let Some(account) = self.config.accounts.get_mut(self.config.active_account) {
            account.username = self.username.trim().to_string();
        }
        if let Some(build) = self.builds.get(self.selected_idx) {
            self.config.last_build = build.id.clone();
        }
        self.config.normalize();
        let _ = self.config.save();
    }

    fn apply_storage_path(&mut self) -> Result<(), String> {
        let raw = self.storage_path_edit.trim();
        let selected = if raw.is_empty() {
            None
        } else {
            Some(PathBuf::from(raw))
        };
        let previous = configured_builds_root(&self.config);

        set_builds_root(selected.clone());
        if let Err(error) = ensure_dirs() {
            set_builds_root(previous);
            return Err(format!("Не удалось использовать папку: {error}"));
        }

        self.config.builds_directory = selected
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        self.storage_path_edit = builds_root().to_string_lossy().to_string();
        self.config.save()?;
        Ok(())
    }

    fn selected_build(&self) -> Option<&BuildInfo> {
        self.builds.get(self.selected_idx)
    }

    fn validate_username(username: &str) -> Result<(), String> {
        let username = username.trim();
        if username.is_empty() {
            return Err("Введите ник".into());
        }
        if username.chars().count() > 16 {
            return Err("Ник не должен быть длиннее 16 символов".into());
        }
        if !username
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err("В нике допустимы латинские буквы, цифры и _".into());
        }
        Ok(())
    }

    fn on_play(&mut self) {
        if self.busy != Busy::Idle {
            return;
        }
        if let Err(error) = Self::validate_username(&self.username) {
            self.status = error.clone();
            self.detail = "Исправьте активную учётную запись".into();
            self.append_log(error);
            return;
        }
        let Some(build) = self.selected_build().cloned() else {
            self.status = "Сборки пока не найдены".into();
            self.detail = "Добавьте .zip в папку Google Drive".into();
            return;
        };
        if find_java(&self.config.java_path).is_none() {
            self.status = "Java не найдена".into();
            self.detail = "Укажите Java 17/21 в настройках".into();
            self.refresh_java_label();
            return;
        }

        self.save_prefs();
        self.progress = 0.0;
        self.progress_text.clear();
        self.cancel.store(false, Ordering::Relaxed);

        let username = self.username.trim().to_string();
        let ram = self.ram_mb;
        let java_path = self.config.java_path.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        let need_download = !drive::is_build_installed(&build.id);
        let skin_account = self
            .config
            .accounts
            .get(self.config.active_account)
            .cloned()
            .unwrap_or_else(|| AccountConfig {
                username: username.clone(),
                ..Default::default()
            });
        let skin_server_address = self.config.server_address.clone();

        self.busy = if need_download {
            Busy::Installing
        } else {
            Busy::Launching
        };
        if need_download {
            self.status = format!("Установка «{}»…", build.name);
            self.detail = "Скачиваем файлы сборки".into();
            if let Some(size) = build.size.filter(|size| *size > 0) {
                self.progress_indeterminate = false;
                self.progress_text = format!("0 / {} · 0%", format_bytes(size));
            } else {
                self.progress_indeterminate = true;
                self.progress_text = "Подключение…".into();
            }
        } else {
            self.status = format!("Запуск «{}»…", build.name);
            self.detail = format!("Профиль: {username}");
            self.progress_indeterminate = true;
        }
        self.append_log(format!("{}: {} ({username})", self.status, build.id));

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
                    Err(error) => {
                        let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                        return;
                    }
                }
            } else {
                drive::instance_dir(&build.id)
            };

            let root = drive::resolve_instance_root(&instance);
            if mmc::is_mmc_instance(&root) {
                let pack = match mmc::parse_instance(&root) {
                    Ok(pack) => pack,
                    Err(error) => {
                        let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                        return;
                    }
                };

                let _ = tx.send(WorkerMsg::Status(format!(
                    "{} · Minecraft {} · {}",
                    pack.name,
                    pack.minecraft,
                    pack.loader.label()
                )));

                if !is_version_installed(&pack.minecraft) {
                    let _ = tx.send(WorkerMsg::Status(format!(
                        "Установка Minecraft {}…",
                        pack.minecraft
                    )));
                    if let Err(error) =
                        install::install_version(&pack.minecraft, progress.clone(), cancel.clone())
                    {
                        let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
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
                                Err(error) => {
                                    let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                                    return;
                                }
                            }
                        } else {
                            neoforge::neoforge_version_id(version)
                        }
                    }
                    other => {
                        let _ = tx.send(WorkerMsg::DoneErr(format!(
                            "Поддерживаются NeoForge и Vanilla. В сборке: {}",
                            other.label()
                        )));
                        return;
                    }
                };

                let _ = install::ensure_natives_for_version(&launch_id);
                let instance_root = instance_dir(&build.id);
                let game = if pack.game_dir.starts_with(&instance_root) {
                    pack.game_dir
                } else {
                    drive::build_game_dir(&build.id)
                };
                let _ = std::fs::create_dir_all(&game);

                let skin_sync = match &pack.loader {
                    ModLoader::NeoForge { .. } if pack.minecraft == "1.21.1" => {
                        let _ = tx.send(WorkerMsg::Status(
                            "Подготовка синхронизации скина…".into(),
                        ));
                        Some(skin_sync::prepare_for_launch(
                            &skin_account,
                            &game,
                            &skin_server_address,
                        ))
                    }
                    ModLoader::NeoForge { .. } if skin_account.skin_source.trim().is_empty() => None,
                    ModLoader::NeoForge { .. } => Some(Err(format!(
                        "Автосинхронизация скина пока поддерживает Minecraft 1.21.1, а в сборке {}",
                        pack.minecraft
                    ))),
                    _ if skin_account.skin_source.trim().is_empty() => None,
                    _ => Some(Err(
                        "Для автосинхронизации скина нужна клиентская сборка NeoForge 1.21.1"
                            .into(),
                    )),
                };

                let _ = tx.send(WorkerMsg::Status(format!("Запуск «{}»…", pack.name)));
                match launch_game_in_dir(&launch_id, &username, ram, &java_path, &game) {
                    Ok(_) => {
                        let _ = tx.send(WorkerMsg::DoneOk {
                            build: pack.name,
                            username,
                            skin_sync,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                    }
                }
                return;
            }

            let meta = drive::read_build_meta(&root);
            let minecraft = build
                .minecraft
                .clone()
                .or_else(|| {
                    meta.as_ref()
                        .and_then(|meta| meta.minecraft.clone().or(meta.version_id.clone()))
                })
                .or_else(|| guess_mc_version(&build.name).or_else(|| guess_mc_version(&build.id)));

            let Some(minecraft) = minecraft else {
                let _ = tx.send(WorkerMsg::DoneErr(
                    "Не удалось определить версию Minecraft. Добавьте mmc-pack.json или build.json."
                        .into(),
                ));
                return;
            };

            if !is_version_installed(&minecraft) {
                let _ = tx.send(WorkerMsg::Status(format!(
                    "Установка Minecraft {minecraft}…"
                )));
                if let Err(error) = install::install_version(&minecraft, progress, cancel) {
                    let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                    return;
                }
            }

            let game = drive::build_game_dir(&build.id);
            if let Err(error) = prepare_instance_game_dir(&root, &game) {
                let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                return;
            }

            let _ = tx.send(WorkerMsg::Status(format!("Запуск «{}»…", build.name)));
            let skin_sync = if skin_account.skin_source.trim().is_empty() {
                None
            } else {
                Some(Err(
                    "Для автосинхронизации скина нужна клиентская сборка NeoForge 1.21.1"
                        .into(),
                ))
            };
            match launch_game_in_dir(&minecraft, &username, ram, &java_path, &game) {
                Ok(_) => {
                    let _ = tx.send(WorkerMsg::DoneOk {
                        build: build.name,
                        username,
                        skin_sync,
                    });
                }
                Err(error) => {
                    let _ = tx.send(WorkerMsg::DoneErr(error.to_string()));
                }
            }
        });
    }

    fn refresh_java_label(&mut self) {
        self.java_label = java_status(&self.config.java_path);
    }

    fn poll_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                WorkerMsg::BuildsOk(builds) => {
                    self.builds = builds;
                    self.busy = Busy::Idle;
                    self.selected_idx = self
                        .builds
                        .iter()
                        .position(|build| build.id == self.config.last_build)
                        .unwrap_or(0);
                    if self.builds.is_empty() {
                        self.status = "Сборок пока нет".into();
                        self.detail = "Добавьте архивы в настроенную папку Google Drive".into();
                    } else {
                        let installed = self
                            .builds
                            .iter()
                            .filter(|build| drive::is_build_installed(&build.id))
                            .count();
                        self.status = "Готово к запуску".into();
                        self.detail = format!(
                            "Доступно сборок: {} · установлено: {installed}",
                            self.builds.len()
                        );
                    }
                    self.append_log(format!("Каталог обновлён: {} сборок", self.builds.len()));
                }
                WorkerMsg::BuildsErr(error) => {
                    self.busy = Busy::Idle;
                    self.status = "Не удалось загрузить сборки".into();
                    self.detail = error.clone();
                    self.append_log(format!("Ошибка каталога: {error}"));
                }
                WorkerMsg::Progress { done, total, label } => {
                    self.status = label;
                    if total > 0 {
                        self.progress_indeterminate = false;
                        self.progress = (done as f32 / total as f32).clamp(0.0, 1.0);
                        self.progress_text = format!(
                            "{} / {} · {:.0}%",
                            format_bytes(done),
                            format_bytes(total),
                            self.progress * 100.0
                        );
                        self.detail = self.progress_text.clone();
                    } else {
                        self.progress_indeterminate = true;
                        self.progress_text = format!("Скачано {}", format_bytes(done));
                        self.detail = self.progress_text.clone();
                    }
                }
                WorkerMsg::Status(status) => {
                    self.status = status.clone();
                    self.detail = "Операция выполняется…".into();
                    self.progress_indeterminate = true;
                    self.append_log(status);
                }
                WorkerMsg::DoneOk {
                    build,
                    username,
                    skin_sync,
                } => {
                    self.busy = Busy::Idle;
                    self.progress = 1.0;
                    self.progress_indeterminate = false;
                    self.progress_text.clear();
                    self.status = format!("Запущено: {build}");
                    self.detail = match &skin_sync {
                        Some(Ok(SkinSyncOutcome::Ready)) => {
                            format!("Игрок {username} · скин применится после входа на сервер")
                        }
                        Some(Err(error)) => {
                            format!("Игра запущена, но скин не синхронизирован: {error}")
                        }
                        _ => format!("Игрок {username} · приятной игры!"),
                    };
                    self.append_log(format!("Игра запущена: {build} ({username})"));
                    if let Some(Err(error)) = skin_sync {
                        self.append_log(format!("Синхронизация скина: {error}"));
                    }
                    self.refresh_game_log();
                }
                WorkerMsg::DoneErr(error) => {
                    self.busy = Busy::Idle;
                    self.progress = 0.0;
                    self.progress_indeterminate = false;
                    self.progress_text.clear();
                    self.status = "Ошибка запуска".into();
                    self.detail = error.clone();
                    self.append_log(format!("Ошибка: {error}"));
                    self.refresh_game_log();
                }
            }
        }
    }

    fn reload_gallery(&mut self, ctx: &egui::Context) {
        let folder = screenshots_dir();
        let _ = std::fs::create_dir_all(&folder);
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&folder)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| {
                        matches!(
                            extension.to_ascii_lowercase().as_str(),
                            "png" | "jpg" | "jpeg"
                        )
                    })
                    .unwrap_or(false)
            })
            .collect();
        paths.sort_by_key(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        });

        let mut gallery = Vec::new();
        let mut skipped = 0usize;
        for image_path in paths.into_iter().take(80) {
            let loaded = std::fs::read(&image_path)
                .ok()
                .and_then(|bytes| image::load_from_memory(&bytes).ok());
            let Some(image) = loaded else {
                skipped += 1;
                continue;
            };
            let thumbnail = image.thumbnail(1400, 900).to_rgba8();
            let size = [thumbnail.width() as usize, thumbnail.height() as usize];
            if size[0] == 0 || size[1] == 0 {
                skipped += 1;
                continue;
            }
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, thumbnail.as_raw());
            let name = image_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "Скриншот".into());
            let texture = ctx.load_texture(
                format!("gallery:{}", image_path.display()),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            gallery.push(GalleryItem {
                path: image_path,
                name,
                texture,
                aspect: size[0] as f32 / size[1] as f32,
            });
        }
        self.gallery = gallery;
        self.gallery_status = if self.gallery.is_empty() {
            "Пока пусто — добавьте PNG или JPG".into()
        } else if skipped > 0 {
            format!("Загружено: {} · пропущено: {skipped}", self.gallery.len())
        } else {
            format!("Загружено скриншотов: {}", self.gallery.len())
        };
        self.append_log(format!("Галерея: {}", self.gallery_status));
    }

    fn refresh_game_log(&mut self) {
        self.game_log = std::fs::read_to_string(last_launch_log()).unwrap_or_default();
        if self.game_log.chars().count() > 100_000 {
            self.game_log = self
                .game_log
                .chars()
                .rev()
                .take(80_000)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
        }
        self.last_log_refresh = Instant::now();
    }

    fn activate_account(&mut self, index: usize) {
        if index >= self.config.accounts.len() {
            return;
        }
        self.config.active_account = index;
        self.username = self.config.accounts[index].username.clone();
        self.config.username = self.username.clone();
        let _ = self.config.save();
        self.account_message = format!("Активный профиль: {}", self.username);
        self.append_log(self.account_message.clone());
    }

    fn start_account_edit(&mut self, index: Option<usize>) {
        self.editing_account = index;
        self.account_draft = index
            .and_then(|index| self.config.accounts.get(index).cloned())
            .unwrap_or_else(empty_account);
        self.account_message.clear();
    }

    fn save_account_draft(&mut self) {
        if let Err(error) = Self::validate_username(&self.account_draft.username) {
            self.account_message = error;
            return;
        }
        self.account_draft.username = self.account_draft.username.trim().to_string();
        if let Some(index) = self.editing_account {
            if let Some(account) = self.config.accounts.get_mut(index) {
                *account = self.account_draft.clone();
                if self.config.active_account == index {
                    self.username = account.username.clone();
                }
                self.account_message = "Изменения сохранены".into();
            }
        } else {
            self.config.accounts.push(self.account_draft.clone());
            let index = self.config.accounts.len() - 1;
            self.activate_account(index);
            self.editing_account = Some(index);
            self.account_message = "Учётная запись добавлена".into();
        }
        self.config.username = self.username.clone();
        let _ = self.config.save();
    }

    fn delete_account(&mut self, index: usize) {
        if self.config.accounts.len() <= 1 || index >= self.config.accounts.len() {
            self.account_message = "Должна остаться хотя бы одна учётная запись".into();
            return;
        }
        let removed = self.config.accounts.remove(index);
        if self.config.active_account >= self.config.accounts.len() {
            self.config.active_account = self.config.accounts.len() - 1;
        } else if index < self.config.active_account {
            self.config.active_account -= 1;
        }
        self.username = self.config.accounts[self.config.active_account]
            .username
            .clone();
        self.config.username = self.username.clone();
        self.start_account_edit(None);
        self.account_message = format!("Профиль {} удалён", removed.username);
        let _ = self.config.save();
    }

    fn active_skin_command(&self) -> String {
        self.config
            .accounts
            .get(self.config.active_account)
            .map(skin_command)
            .unwrap_or_else(|| "/skin update".into())
    }

    fn draw_shell(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let palette = Palette::for_theme(self.config.theme);
        let full = ui.max_rect();
        let title_height = 36.0;
        let sidebar_width = 232.0_f32.min(full.width() * 0.27);
        let nav_height_target: f32 = if self.page == Page::Console {
            0.0
        } else {
            58.0
        };
        let nav_height =
            ctx.animate_value_with_time(ui.id().with("top_nav_height"), nav_height_target, 0.22);

        ui.painter()
            .rect_filled(full, CornerRadius::ZERO, palette.background);

        let title = Rect::from_min_size(full.min, Vec2::new(full.width(), title_height));
        ui.painter()
            .rect_filled(title, CornerRadius::ZERO, palette.titlebar);
        ui.painter().line_segment(
            [title.left_bottom(), title.right_bottom()],
            Stroke::new(1.0, palette.border),
        );
        let logo = Rect::from_min_size(
            title.min + Vec2::new(10.0, 10.0),
            Vec2::new(14.0, 14.0),
        );
        draw_logo(ui.painter(), logo);
        ui.painter().text(
            Pos2::new(33.0, title.center().y),
            Align2::LEFT_CENTER,
            "MineLauncher Beta",
            FontId::proportional(13.0),
            palette.text,
        );

        let sidebar = Rect::from_min_max(
            Pos2::new(full.left(), title.bottom()),
            Pos2::new(full.left() + sidebar_width, full.bottom()),
        );
        self.draw_sidebar(ui, sidebar, palette);

        let main = Rect::from_min_max(
            Pos2::new(sidebar.right(), title.bottom()),
            full.right_bottom(),
        );
        let top_nav =
            Rect::from_min_size(main.min, Vec2::new(main.width(), nav_height.min(main.height())));
        if nav_height > 0.0 {
            self.draw_top_nav(ui, top_nav, palette);
        }

        let content = Rect::from_min_max(
            Pos2::new(main.left(), top_nav.bottom()),
            main.right_bottom(),
        );
        let (offset, opacity, transition_progress, transition_active) = self.transition_frame(ctx);
        let animated_content = content.translate(Vec2::new(offset, 0.0));
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(content), |content_ui| {
            content_ui.set_clip_rect(content);
            content_ui.set_opacity(opacity);
            if transition_active {
                content_ui.disable();
            }
            match self.page {
                Page::Home => self.draw_home(content_ui, animated_content, palette),
                Page::Skins => self.draw_skins(content_ui, animated_content, palette),
                Page::Gallery => self.draw_gallery(content_ui, animated_content, ctx, palette),
                Page::Console => self.draw_console(content_ui, animated_content, palette),
                Page::Settings => self.draw_settings(content_ui, animated_content, palette),
            }
        });

        if transition_active {
            let glow = (1.0 - transition_progress).powi(2);
            let edge_x = if self.transition_direction > 0.0 {
                animated_content.left()
            } else {
                animated_content.right()
            };
            let glow_color = color_with_alpha(palette.accent, 0.55 * glow);
            ui.painter().line_segment(
                [
                    Pos2::new(edge_x, content.top() + 14.0),
                    Pos2::new(edge_x, content.bottom() - 14.0),
                ],
                Stroke::new(2.0, glow_color),
            );
        }
    }

    fn draw_sidebar(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, palette.sidebar);
        ui.painter().line_segment(
            [rect.right_top(), rect.right_bottom()],
            Stroke::new(1.0, palette.border),
        );

        let profile = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 66.0));
        ui.painter()
            .rect_filled(profile, CornerRadius::ZERO, palette.sidebar_profile);
        let avatar = Rect::from_min_size(
            profile.min + Vec2::new(16.0, 13.0),
            Vec2::new(40.0, 40.0),
        );
        draw_avatar(ui.painter(), avatar, palette);
        ui.painter().text(
            avatar.right_top() + Vec2::new(11.0, 7.0),
            Align2::LEFT_CENTER,
            truncate(&self.username, 17),
            FontId::proportional(15.0),
            palette.text,
        );
        ui.painter().text(
            avatar.right_bottom() + Vec2::new(11.0, -7.0),
            Align2::LEFT_CENTER,
            "Офлайн-профиль",
            FontId::proportional(11.5),
            palette.muted,
        );
        ui.painter().circle_filled(
            Pos2::new(profile.right() - 18.0, profile.center().y),
            4.0,
            palette.danger,
        );
        if ui
            .interact(profile, ui.id().with("profile"), Sense::click())
            .clicked()
        {
            self.navigate_to(ui.ctx(), Page::Settings, Some(SettingsTab::Accounts));
        }

        let server_rect = Rect::from_min_size(
            Pos2::new(rect.left(), profile.bottom() + 10.0),
            Vec2::new(rect.width(), 54.0),
        );
        let server_selected = matches!(self.page, Page::Home | Page::Skins | Page::Gallery);
        if side_button(
            ui,
            server_rect,
            "◆",
            &self.config.server_name.to_uppercase(),
            server_selected,
            palette,
        )
        .clicked()
        {
            self.navigate_to(ui.ctx(), Page::Home, None);
        }

        let console_rect = server_rect.translate(Vec2::new(0.0, 64.0));
        if side_button(
            ui,
            console_rect,
            ">_",
            "КОНСОЛЬ",
            self.page == Page::Console,
            palette,
        )
        .clicked()
        {
            self.navigate_to(ui.ctx(), Page::Console, None);
            self.refresh_game_log();
        }

        let settings_rect = Rect::from_min_size(
            Pos2::new(rect.left(), rect.bottom() - 64.0),
            Vec2::new(rect.width(), 46.0),
        );
        if side_button(
            ui,
            settings_rect,
            "⚙",
            "НАСТРОЙКИ",
            self.page == Page::Settings,
            palette,
        )
        .clicked()
        {
            self.navigate_to(ui.ctx(), Page::Settings, None);
            self.refresh_java_label();
        }
        ui.painter().text(
            Pos2::new(rect.center().x, rect.bottom() - 7.0),
            Align2::CENTER_BOTTOM,
            "v1.0.0",
            FontId::proportional(10.0),
            palette.muted,
        );
    }

    fn draw_top_nav(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, palette.titlebar);
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, palette.border),
        );

        let mut x = rect.left() + 24.0;
        if self.page == Page::Settings {
            ui.painter().text(
                Pos2::new(x, rect.center().y),
                Align2::LEFT_CENTER,
                "‹",
                FontId::proportional(22.0),
                palette.muted,
            );
            x += 22.0;
            let mut active_rect = None;
            for (tab, label, width) in [
                (SettingsTab::General, "Основное", 94.0),
                (SettingsTab::Accounts, "Учётные записи", 142.0),
                (SettingsTab::About, "О лаунчере", 122.0),
            ] {
                let tab_rect =
                    Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(width, rect.height()));
                if top_tab(ui, tab_rect, label, self.settings_tab == tab, palette).clicked() {
                    self.navigate_to(ui.ctx(), Page::Settings, Some(tab));
                }
                if self.settings_tab == tab {
                    active_rect = Some(tab_rect);
                }
                x += width + 8.0;
            }
            if let Some(active_rect) = active_rect {
                animated_tab_indicator(ui, "settings_tabs", active_rect, palette);
            }
            return;
        }

        let mut active_rect = None;
        for (page, label, width) in [
            (Page::Home, "Установки", 94.0),
            (Page::Skins, "Скины", 72.0),
            (Page::Gallery, "Галерея", 86.0),
        ] {
            let tab_rect =
                Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(width, rect.height()));
            if top_tab(ui, tab_rect, label, self.page == page, palette).clicked() {
                self.navigate_to(ui.ctx(), page, None);
            }
            if self.page == page {
                active_rect = Some(tab_rect);
            }
            x += width + 10.0;
        }
        if let Some(active_rect) = active_rect {
            animated_tab_indicator(ui, "launcher_tabs", active_rect, palette);
        }
    }

    fn draw_home(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        let footer_height = 76.0_f32.min(rect.height() * 0.18);
        let hero = Rect::from_min_max(
            rect.min,
            Pos2::new(rect.right(), rect.bottom() - footer_height),
        );
        if let Some(item) = self.gallery.first() {
            paint_cover(ui.painter(), item.texture.id(), hero, item.aspect);
            ui.painter().rect_filled(
                hero,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(4, 9, 8, 92),
            );
        } else {
            paint_minecraft_placeholder(ui.painter(), hero, palette);
        }

        let pill_width = (self.status.chars().count() as f32 * 8.1 + 34.0).clamp(170.0, 390.0);
        let pill = Rect::from_center_size(
            Pos2::new(hero.center().x, hero.bottom() - 48.0),
            Vec2::new(pill_width, 38.0),
        );
        ui.painter()
            .rect_filled(pill, CornerRadius::same(9), Color32::from_black_alpha(190));
        ui.painter().text(
            pill.center(),
            Align2::CENTER_CENTER,
            truncate(&self.status, 42),
            FontId::proportional(14.0),
            Color32::WHITE,
        );

        if self.busy != Busy::Idle {
            let progress_rect = Rect::from_min_size(
                Pos2::new(hero.left(), hero.bottom() - 3.0),
                Vec2::new(hero.width(), 3.0),
            );
            ui.painter()
                .rect_filled(progress_rect, CornerRadius::ZERO, palette.progress_track);
            let fraction = if self.progress_indeterminate {
                let time = ui.input(|input| input.time) as f32;
                ((time * 0.22).sin() * 0.25 + 0.5).clamp(0.12, 0.88)
            } else {
                self.progress.clamp(0.0, 1.0)
            };
            ui.painter().rect_filled(
                Rect::from_min_size(
                    progress_rect.min,
                    Vec2::new(progress_rect.width() * fraction, progress_rect.height()),
                ),
                CornerRadius::ZERO,
                palette.accent,
            );
        }

        if self.gallery.is_empty() {
            let hint = Rect::from_center_size(
                hero.center() + Vec2::new(0.0, 42.0),
                Vec2::new(410.0_f32.min(hero.width() - 48.0), 94.0),
            );
            ui.painter()
                .rect_filled(hint, CornerRadius::same(12), Color32::from_black_alpha(112));
            ui.painter().text(
                hint.center_top() + Vec2::new(0.0, 24.0),
                Align2::CENTER_CENTER,
                "ФОН-ЗАГЛУШКА",
                FontId::proportional(12.0),
                palette.accent,
            );
            ui.painter().text(
                hint.center() + Vec2::new(0.0, 12.0),
                Align2::CENTER_CENTER,
                "Добавьте скриншоты в папку screenshots",
                FontId::proportional(15.0),
                Color32::WHITE,
            );
        }

        let footer = Rect::from_min_max(
            Pos2::new(rect.left(), hero.bottom()),
            rect.right_bottom(),
        );
        ui.painter()
            .rect_filled(footer, CornerRadius::ZERO, palette.surface);
        ui.painter().line_segment(
            [footer.left_top(), footer.right_top()],
            Stroke::new(1.0, palette.border),
        );

        let icon = Rect::from_center_size(
            Pos2::new(footer.left() + 42.0, footer.center().y),
            Vec2::splat(42.0),
        );
        draw_build_icon(ui.painter(), icon, palette);

        let info_left = icon.right() + 12.0;
        let selected_build = self
            .selected_build()
            .map(|build| build.name.clone())
            .unwrap_or_else(|| "Сборка не выбрана".into());
        ui.painter().text(
            Pos2::new(info_left, footer.center().y - 10.0),
            Align2::LEFT_CENTER,
            truncate(&selected_build, 28),
            FontId::proportional(14.0),
            palette.text,
        );
        ui.painter().text(
            Pos2::new(info_left, footer.center().y + 12.0),
            Align2::LEFT_CENTER,
            truncate(&self.detail, 44),
            FontId::proportional(11.0),
            palette.muted,
        );

        let play_width = 205.0_f32.min(footer.width() * 0.28);
        let play_rect = Rect::from_center_size(
            Pos2::new(footer.right() - play_width / 2.0 - 24.0, footer.center().y),
            Vec2::new(play_width, 46.0),
        );
        let can_play = self.busy == Busy::Idle && !self.builds.is_empty();
        let play_response = ui.interact(play_rect, ui.id().with("home_play"), Sense::click());
        let play_color = if !can_play {
            palette.disabled
        } else if play_response.hovered() {
            palette.accent_hover
        } else {
            palette.accent
        };
        ui.painter()
            .rect_filled(play_rect, CornerRadius::same(10), play_color);
        ui.painter().text(
            play_rect.center(),
            Align2::CENTER_CENTER,
            match self.busy {
                Busy::Idle => "ИГРАТЬ".to_string(),
                Busy::LoadingBuilds => "ЗАГРУЗКА…".into(),
                Busy::Installing if !self.progress_indeterminate => {
                    format!("УСТАНОВКА {:.0}%", self.progress * 100.0)
                }
                Busy::Installing => "УСТАНОВКА…".into(),
                Busy::Launching => "ЗАПУСК…".into(),
            },
            FontId::proportional(14.0),
            if can_play {
                Color32::WHITE
            } else {
                palette.muted
            },
        );
        if can_play && play_response.clicked() {
            self.on_play();
        }

        let combo_width = 190.0_f32.min((play_rect.left() - info_left - 20.0).max(120.0));
        let combo_rect = Rect::from_min_size(
            Pos2::new(play_rect.left() - combo_width - 14.0, footer.center().y - 17.0),
            Vec2::new(combo_width, 34.0),
        );
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(combo_rect), |ui| {
            ui.add_enabled_ui(self.busy == Busy::Idle, |ui| {
                let selected = self
                    .selected_build()
                    .map(|build| build.name.clone())
                    .unwrap_or_else(|| "Нет сборок".into());
                egui::ComboBox::from_id_salt("home_build")
                    .width(combo_width)
                    .selected_text(truncate(&selected, 22))
                    .show_ui(ui, |ui| {
                        for (index, build) in self.builds.iter().enumerate() {
                            let suffix = if drive::is_build_installed(&build.id) {
                                " · установлено"
                            } else {
                                ""
                            };
                            if ui
                                .selectable_label(
                                    self.selected_idx == index,
                                    format!("{}{suffix}", build.name),
                                )
                                .clicked()
                            {
                                self.selected_idx = index;
                            }
                        }
                    });
            });
        });
    }

    fn draw_skins(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, palette.background);

        let account = self
            .config
            .accounts
            .get(self.config.active_account)
            .cloned()
            .unwrap_or_default();
        let padding = 24.0;
        let split = (rect.left() + rect.width() * 0.37)
            .clamp(rect.left() + 330.0, rect.left() + 445.0);
        let left = Rect::from_min_max(
            rect.min + Vec2::splat(padding),
            Pos2::new(split - 18.0, rect.bottom() - padding),
        );
        let right = Rect::from_min_max(
            Pos2::new(split + 18.0, rect.top() + padding),
            rect.right_bottom() - Vec2::splat(padding),
        );

        ui.painter().text(
            left.left_top(),
            Align2::LEFT_TOP,
            "Текущий",
            FontId::proportional(17.0),
            palette.text,
        );
        draw_eye_icon(
            ui.painter(),
            Pos2::new(left.right() - 42.0, left.top() + 10.0),
            palette.muted,
        );
        draw_shirt_icon(
            ui.painter(),
            Pos2::new(left.right() - 10.0, left.top() + 10.0),
            palette.muted,
        );

        let model_rect = Rect::from_center_size(
            Pos2::new(left.center().x, left.center().y - 38.0),
            Vec2::new(left.width() * 0.78, (left.height() - 170.0).max(330.0)),
        );
        draw_skin_avatar_3d(ui.painter(), self.steve_texture.id(), model_rect, palette);

        let profile_y = left.bottom() - 89.0;
        ui.painter().text(
            Pos2::new(left.center().x, profile_y),
            Align2::CENTER_CENTER,
            &account.username,
            FontId::proportional(14.0),
            palette.text,
        );
        ui.painter().text(
            Pos2::new(left.center().x, profile_y + 21.0),
            Align2::CENTER_CENTER,
            match account.skin_model {
                SkinModel::Classic => "Classic",
                SkinModel::Slim => "Slim",
            },
            FontId::proportional(12.0),
            palette.muted,
        );

        let apply_rect = Rect::from_center_size(
            Pos2::new(left.center().x, left.bottom() - 38.0),
            Vec2::new((left.width() - 12.0).min(360.0), 36.0),
        );
        let can_apply = can_apply_skin_source(&account.skin_source);
        let apply_response =
            ui.interact(apply_rect, ui.id().with("apply_skin"), Sense::click());
        ui.painter().rect_filled(
            apply_rect,
            CornerRadius::same(6),
            if can_apply && apply_response.hovered() {
                palette.accent_dim
            } else {
                palette.disabled
            },
        );
        ui.painter().text(
            apply_rect.center() + Vec2::new(8.0, 0.0),
            Align2::CENTER_CENTER,
            "Применить",
            FontId::proportional(13.0),
            if can_apply {
                palette.text
            } else {
                palette.muted
            },
        );
        let check_color = if can_apply {
            palette.text
        } else {
            palette.muted
        };
        let check_center = apply_rect.center() - Vec2::new(47.0, 0.0);
        ui.painter().line_segment(
            [
                check_center + Vec2::new(-4.0, 0.0),
                check_center + Vec2::new(-1.0, 3.0),
            ],
            Stroke::new(1.3, check_color),
        );
        ui.painter().line_segment(
            [
                check_center + Vec2::new(-1.0, 3.0),
                check_center + Vec2::new(5.0, -4.0),
            ],
            Stroke::new(1.3, check_color),
        );
        if can_apply && apply_response.clicked() {
            if is_local_skin_source(&account.skin_source) {
                self.account_message =
                    "PNG автоматически загрузится и применится при следующем входе на сервер"
                        .into();
            } else {
                let command = self.active_skin_command();
                ui.ctx().copy_text(command);
                self.account_message =
                    "Скин применится автоматически при входе; команда скопирована как запасной вариант"
                        .into();
            }
        }

        let tabs_y = right.top() + 1.0;
        let mut tab_x = right.left();
        for (label, selected, width) in [
            ("Библиотека", true, 108.0),
            ("История", false, 84.0),
            ("Поиск", false, 72.0),
            ("Плащи", false, 72.0),
        ] {
            let tab_rect = Rect::from_min_size(
                Pos2::new(tab_x, tabs_y),
                Vec2::new(width, 34.0),
            );
            if selected {
                ui.painter()
                    .rect_filled(tab_rect, CornerRadius::same(8), palette.surface);
            }
            ui.painter().text(
                tab_rect.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(13.0),
                if selected {
                    palette.text
                } else {
                    palette.muted
                },
            );
            tab_x += width + 7.0;
        }

        let new_skin = Rect::from_min_size(
            Pos2::new(right.left(), right.top() + 46.0),
            Vec2::new(142.0, 160.0),
        );
        let new_response =
            ui.interact(new_skin, ui.id().with("new_skin"), Sense::click());
        draw_dashed_rect(
            ui.painter(),
            new_skin,
            if new_response.hovered() {
                palette.accent_dim
            } else {
                palette.border
            },
        );
        ui.painter().text(
            new_skin.center() - Vec2::new(0.0, 14.0),
            Align2::CENTER_CENTER,
            "+",
            FontId::proportional(34.0),
            if new_response.hovered() {
                palette.text
            } else {
                palette.muted
            },
        );
        ui.painter().text(
            new_skin.center() + Vec2::new(0.0, 22.0),
            Align2::CENTER_CENTER,
            "Новый скин",
            FontId::proportional(12.0),
            palette.muted,
        );
        if new_response.clicked() {
            self.skin_source_draft = account.skin_source.clone();
            self.skin_model_draft = account.skin_model;
            self.skin_editor_open = true;
        }

        if !account.skin_source.trim().is_empty() {
            let saved_skin = new_skin.translate(Vec2::new(160.0, 0.0));
            ui.painter()
                .rect_filled(saved_skin, CornerRadius::same(8), palette.surface);
            ui.painter().rect_stroke(
                saved_skin,
                CornerRadius::same(8),
                Stroke::new(1.0, palette.border),
                egui::StrokeKind::Inside,
            );
            let mini_model = Rect::from_center_size(
                saved_skin.center() - Vec2::new(0.0, 15.0),
                Vec2::new(84.0, 105.0),
            );
            draw_skin_avatar_3d(ui.painter(), self.steve_texture.id(), mini_model, palette);
            ui.painter().text(
                saved_skin.center_bottom() - Vec2::new(0.0, 16.0),
                Align2::CENTER_BOTTOM,
                truncate(&account.skin_source, 18),
                FontId::proportional(11.0),
                palette.text,
            );
        }

        if !self.account_message.is_empty() {
            ui.painter().text(
                Pos2::new(left.center().x, left.bottom() - 3.0),
                Align2::CENTER_BOTTOM,
                truncate(&self.account_message, 58),
                FontId::proportional(10.5),
                palette.accent_text,
            );
        } else if !can_apply {
            ui.painter().text(
                Pos2::new(left.center().x, left.bottom() - 3.0),
                Align2::CENTER_BOTTOM,
                "Добавьте скин в библиотеку",
                FontId::proportional(10.5),
                palette.muted,
            );
        }

        if self.skin_editor_open {
            let mut open = true;
            let mut save_clicked = false;
            let mut copy_clicked = false;
            let mut browse_clicked = false;
            egui::Window::new("Новый скин")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(430.0)
                .show(ui.ctx(), |ui| {
                    ui.label("PNG-файл, ник Minecraft или публичная HTTPS-ссылка");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.skin_source_draft)
                                .desired_width(306.0)
                                .hint_text("Выберите файл или укажите ссылку"),
                        );
                        browse_clicked = ui.button("Обзор…").clicked();
                    });
                    ui.add_space(8.0);
                    egui::ComboBox::from_id_salt("skin_library_model")
                        .selected_text(self.skin_model_draft.label())
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.skin_model_draft,
                                SkinModel::Classic,
                                SkinModel::Classic.label(),
                            );
                            ui.selectable_value(
                                &mut self.skin_model_draft,
                                SkinModel::Slim,
                                SkinModel::Slim.label(),
                            );
                        });
                    ui.add_space(10.0);
                    if is_local_skin_source(&self.skin_source_draft) {
                        ui.label(
                            RichText::new(
                                "Локальный PNG будет автоматически загружен через официальный \
                                 MineSkin API и применён при входе на сервер.",
                            )
                            .color(palette.muted),
                        );
                    } else {
                        ui.label(
                            RichText::new(skin_command(&AccountConfig {
                                username: account.username.clone(),
                                skin_source: self.skin_source_draft.clone(),
                                skin_model: self.skin_model_draft,
                            }))
                            .monospace()
                            .color(palette.accent_text),
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        save_clicked = ui.button("Сохранить в библиотеку").clicked();
                        copy_clicked = ui
                            .add_enabled(
                                !is_local_skin_source(&self.skin_source_draft),
                                egui::Button::new("Копировать команду"),
                            )
                            .clicked();
                    });
                });
            if browse_clicked {
                if let Some(path) = select_skin_file() {
                    match load_skin_texture(ui.ctx(), &path) {
                        Ok(texture) => {
                            self.steve_texture = texture;
                            self.skin_source_draft = path.to_string_lossy().to_string();
                            self.account_message =
                                format!("Выбран скин: {}", skin_file_name(&path));
                        }
                        Err(error) => self.account_message = error,
                    }
                }
            }
            if copy_clicked {
                ui.ctx().copy_text(skin_command(&AccountConfig {
                    username: account.username.clone(),
                    skin_source: self.skin_source_draft.clone(),
                    skin_model: self.skin_model_draft,
                }));
                self.account_message = "Команда SkinRestorer скопирована".into();
            }
            if save_clicked {
                let source = self.skin_source_draft.trim().to_string();
                let valid_source = if is_local_skin_source(&source) {
                    match load_skin_texture(ui.ctx(), Path::new(&source)) {
                        Ok(texture) => {
                            self.steve_texture = texture;
                            true
                        }
                        Err(error) => {
                            self.account_message = error;
                            false
                        }
                    }
                } else {
                    true
                };
                if valid_source {
                    if let Some(active) = self.config.accounts.get_mut(self.config.active_account) {
                        active.skin_source = source;
                        active.skin_model = self.skin_model_draft;
                    }
                    let _ = self.config.save();
                    self.account_message = "Скин сохранён в библиотеке".into();
                    open = false;
                }
            }
            self.skin_editor_open = open;
        }
    }

    fn draw_gallery(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        ctx: &egui::Context,
        palette: Palette,
    ) {
        let content = rect.shrink2(Vec2::new(26.0, 20.0));
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Галерея")
                                .size(25.0)
                                .strong()
                                .color(palette.text),
                        );
                        ui.label(
                            RichText::new(&self.gallery_status)
                                .size(12.0)
                                .color(palette.muted),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Открыть папку").clicked() {
                            let _ = std::fs::create_dir_all(screenshots_dir());
                            let _ = open_path(&screenshots_dir());
                        }
                        if ui.button("Обновить").clicked() {
                            self.reload_gallery(ctx);
                        }
                    });
                });
                ui.add_space(16.0);

                if self.gallery.is_empty() {
                    let available = ui.available_size();
                    let (empty_rect, _) = ui.allocate_exact_size(available, Sense::hover());
                    draw_empty_gallery(ui.painter(), empty_rect, palette);
                    return;
                }

                let available_width = ui.available_width();
                let columns = if available_width > 920.0 {
                    3
                } else if available_width > 570.0 {
                    2
                } else {
                    1
                };
                let gap = 14.0;
                let card_width =
                    ((available_width - gap * (columns as f32 - 1.0)) / columns as f32).max(220.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in self.gallery.chunks(columns) {
                            ui.horizontal_top(|ui| {
                                for item in row {
                                    Frame::NONE
                                        .fill(palette.surface)
                                        .stroke(Stroke::new(1.0, palette.border))
                                        .corner_radius(10.0)
                                        .inner_margin(egui::Margin::same(8))
                                        .show(ui, |ui| {
                                            ui.set_width(card_width - 18.0);
                                            let image_height =
                                                ((card_width - 18.0) / item.aspect).clamp(145.0, 240.0);
                                            let (image_rect, response) = ui.allocate_exact_size(
                                                Vec2::new(card_width - 18.0, image_height),
                                                Sense::click(),
                                            );
                                            ui.painter().rect_filled(
                                                image_rect,
                                                CornerRadius::same(7),
                                                palette.code,
                                            );
                                            paint_cover(
                                                ui.painter(),
                                                item.texture.id(),
                                                image_rect,
                                                item.aspect,
                                            );
                                            if response.double_clicked() {
                                                let _ = open_path(&item.path);
                                            }
                                            ui.add_space(4.0);
                                            ui.label(
                                                RichText::new(truncate(&item.name, 34))
                                                    .size(12.0)
                                                    .color(palette.text),
                                            );
                                        });
                                }
                            });
                            ui.add_space(gap);
                        }
                    });
            },
        );
    }

    fn draw_console(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, palette.console);
        let top_height = 44.0;
        let bottom_height = 26.0;
        let top = Rect::from_min_size(rect.min, Vec2::new(rect.width(), top_height));
        let bottom = Rect::from_min_max(
            Pos2::new(rect.left(), rect.bottom() - bottom_height),
            rect.right_bottom(),
        );
        let output = Rect::from_min_max(
            Pos2::new(rect.left(), top.bottom()),
            Pos2::new(rect.right(), bottom.top()),
        );

        ui.painter()
            .rect_filled(top, CornerRadius::ZERO, palette.surface);
        ui.painter().line_segment(
            [top.left_bottom(), top.right_bottom()],
            Stroke::new(1.0, palette.border),
        );
        ui.painter().circle_filled(
            Pos2::new(top.left() + 13.0, top.center().y),
            5.0,
            if self.busy == Busy::Idle {
                palette.disabled
            } else {
                palette.accent
            },
        );
        ui.painter().text(
            Pos2::new(top.left() + 27.0, top.center().y),
            Align2::LEFT_CENTER,
            if self.busy == Busy::Idle {
                "Нет активного процесса"
            } else {
                &self.status
            },
            FontId::proportional(12.0),
            palette.muted,
        );

        let trash = Rect::from_center_size(
            Pos2::new(top.right() - 27.0, top.center().y),
            Vec2::splat(28.0),
        );
        let trash_response =
            ui.interact(trash, ui.id().with("clear_console"), Sense::click());
        ui.painter().text(
            trash.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(20.0),
            if trash_response.hovered() {
                palette.text
            } else {
                palette.muted
            },
        );
        if trash_response.clicked() {
            self.launcher_log.clear();
            self.game_log.clear();
            let _ = std::fs::write(last_launch_log(), "");
            self.last_log_refresh = Instant::now();
        }

        let filter_rect = Rect::from_center_size(
            Pos2::new(trash.left() - 108.0, top.center().y),
            Vec2::new(192.0, 28.0),
        );
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(filter_rect), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.console_filter)
                    .desired_width(filter_rect.width())
                    .hint_text("Фильтр…"),
            );
        });

        let filter = self.console_filter.trim().to_ascii_lowercase();
        let visible_log = if filter.is_empty() {
            self.game_log.clone()
        } else {
            self.game_log
                .lines()
                .filter(|line| line.to_ascii_lowercase().contains(&filter))
                .collect::<Vec<_>>()
                .join("\n")
        };

        if visible_log.trim().is_empty() {
            ui.painter().text(
                output.center(),
                Align2::CENTER_CENTER,
                if self.game_log.is_empty() {
                    "Запустите игру, чтобы увидеть вывод консоли"
                } else {
                    "По этому фильтру ничего не найдено"
                },
                FontId::monospace(12.0),
                palette.muted,
            );
        } else {
            ui.allocate_new_ui(
                egui::UiBuilder::new()
                    .max_rect(output.shrink2(Vec2::new(9.0, 8.0)))
                    .layout(Layout::top_down(Align::Min)),
                |ui| {
                    egui::ScrollArea::both()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(&visible_log)
                                    .monospace()
                                    .size(11.5)
                                    .color(palette.console_text),
                            );
                        });
                },
            );
        }

        ui.painter()
            .rect_filled(bottom, CornerRadius::ZERO, palette.surface);
        ui.painter().line_segment(
            [bottom.left_top(), bottom.right_top()],
            Stroke::new(1.0, palette.border),
        );
        ui.painter().text(
            Pos2::new(bottom.left() + 7.0, bottom.center().y),
            Align2::LEFT_CENTER,
            self.selected_build()
                .map(|build| format!("Сборка: {}", build.name))
                .unwrap_or_else(|| "Нет данных о сборке".into()),
            FontId::proportional(10.5),
            palette.muted,
        );
        let ram = if self.busy == Busy::Idle {
            0.0
        } else {
            self.ram_mb as f32 / 1024.0
        };
        ui.painter().text(
            Pos2::new(bottom.right() - 26.0, bottom.center().y),
            Align2::RIGHT_CENTER,
            format!("RAM {ram:.1} GB"),
            FontId::proportional(10.5),
            palette.muted,
        );
        let arrow = Pos2::new(bottom.right() - 9.0, bottom.center().y);
        ui.painter().line_segment(
            [arrow - Vec2::new(0.0, 5.0), arrow + Vec2::new(0.0, 3.0)],
            Stroke::new(1.2, palette.accent),
        );
        ui.painter().line_segment(
            [arrow + Vec2::new(0.0, 3.0), arrow + Vec2::new(-3.0, 0.0)],
            Stroke::new(1.2, palette.accent),
        );
        ui.painter().line_segment(
            [arrow + Vec2::new(0.0, 3.0), arrow + Vec2::new(3.0, 0.0)],
            Stroke::new(1.2, palette.accent),
        );
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        match self.settings_tab {
            SettingsTab::General => self.draw_general_settings(ui, rect, palette),
            SettingsTab::Accounts => self.draw_accounts_settings(ui, rect, palette),
            SettingsTab::About => self.draw_about(ui, rect, palette),
        }
    }

    fn draw_general_settings(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        let content = rect.shrink2(Vec2::new(24.0, 18.0));
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(840.0);
                        section_title(ui, "ВНЕШНИЙ ВИД", palette.muted);
                        ui.label(
                            RichText::new(
                                "Для части изменений может потребоваться перезапуск лаунчера.",
                            )
                            .size(12.0)
                            .color(palette.muted),
                        );
                        ui.add_space(10.0);
                        setting_row(ui, "Тема:", |ui| {
                            let previous = self.config.theme;
                            egui::ComboBox::from_id_salt("settings_theme")
                                .selected_text(match self.config.theme {
                                    Theme::Dark => "Стандартная тёмная",
                                    Theme::Light => "Светлая",
                                })
                                .width(220.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.config.theme,
                                        Theme::Dark,
                                        "Стандартная тёмная",
                                    );
                                    ui.selectable_value(
                                        &mut self.config.theme,
                                        Theme::Light,
                                        "Светлая",
                                    );
                                });
                            if previous != self.config.theme {
                                configure_style(ui.ctx(), self.config.theme);
                                let _ = self.config.save();
                            }
                        });

                        ui.add_space(22.0);
                        section_title(ui, "НАСТРОЙКИ JAVA", palette.muted);
                        ui.checkbox(
                            &mut self.config.auto_ram,
                            "Автоматическое определение RAM",
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(!self.config.auto_ram, |ui| {
                                let mut ram = self.ram_mb as f32;
                                if ui
                                    .add(
                                        egui::Slider::new(&mut ram, 1024.0..=16384.0)
                                            .step_by(512.0)
                                            .show_value(false),
                                    )
                                    .changed()
                                {
                                    self.ram_mb = ((ram / 512.0).round() as u32 * 512)
                                        .clamp(1024, 16384);
                                }
                            });
                            ui.label(
                                RichText::new(format!("{:.1} GB", self.ram_mb as f32 / 1024.0))
                                    .color(palette.text),
                            );
                        });
                        ui.add_space(10.0);
                        ui.label("Java 17 / Java 21");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.java_path)
                                    .desired_width(570.0)
                                    .hint_text("Авто (будет найдена автоматически)"),
                            );
                            if ui.button("Определить").clicked() {
                                self.refresh_java_label();
                            }
                        });
                        ui.label(
                            RichText::new(&self.java_label)
                                .size(11.0)
                                .color(palette.muted),
                        );

                        ui.add_space(22.0);
                        section_title(ui, "СЕРВЕР И SKINSRESTORER", palette.muted);
                        setting_row(ui, "Название:", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.server_name)
                                    .desired_width(310.0),
                            );
                        });
                        setting_row(ui, "Адрес сервера:", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.server_address)
                                    .desired_width(310.0)
                                    .hint_text("play.example.ru"),
                            );
                        });
                        ui.label(
                            RichText::new(
                                "Выбранный скин автоматически применяется через SkinsRestorer при входе \
                                 с NeoForge 1.21.1. Локальные PNG загружаются через MineSkin.",
                            )
                            .size(11.0)
                            .color(palette.muted),
                        );

                        ui.add_space(22.0);
                        section_title(ui, "ХРАНЕНИЕ", palette.muted);
                        ui.label("Папка архивов и игровых инстансов");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.storage_path_edit)
                                    .desired_width(570.0),
                            );
                            if ui.button("Выбрать…").clicked() {
                                if let Some(path) = select_folder() {
                                    self.storage_path_edit =
                                        path.to_string_lossy().to_string();
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    self.busy == Busy::Idle,
                                    egui::Button::new("Применить"),
                                )
                                .clicked()
                            {
                                match self.apply_storage_path() {
                                    Ok(()) => {
                                        self.status = "Папка сборок изменена".into();
                                        self.detail = builds_root().display().to_string();
                                    }
                                    Err(error) => {
                                        self.status = "Не удалось изменить папку".into();
                                        self.detail = error;
                                    }
                                }
                            }
                            if ui.button("Открыть папку сборок").clicked() {
                                let _ = open_path(&builds_root());
                            }
                            if ui.button("Открыть галерею").clicked() {
                                let _ = open_path(&screenshots_dir());
                            }
                        });

                        ui.add_space(22.0);
                        ui.horizontal(|ui| {
                            if ui.button("Сохранить настройки").clicked() {
                                self.save_prefs();
                                self.refresh_java_label();
                                self.account_message = "Настройки сохранены".into();
                            }
                            if ui
                                .add_enabled(
                                    self.busy == Busy::Idle,
                                    egui::Button::new("Обновить сборки"),
                                )
                                .clicked()
                            {
                                self.reload_builds();
                            }
                            if ui.button("Текущая сборка").clicked() {
                                if let Some(build) = self.selected_build() {
                                    let directory = drive::build_game_dir(&build.id);
                                    let _ = std::fs::create_dir_all(&directory);
                                    let _ = open_path(&directory);
                                } else {
                                    let _ = open_path(&instances_dir());
                                }
                            }
                        });
                    });
            },
        );
    }

    fn draw_accounts_settings(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        let content = rect.shrink2(Vec2::new(24.0, 20.0));
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.label(
                    RichText::new("Учётные записи")
                        .size(24.0)
                        .strong()
                        .color(palette.text),
                );
                ui.label(
                    RichText::new(
                        "Лаунчер хранит только офлайн-ник и настройки скина — никаких паролей.",
                    )
                    .color(palette.muted),
                );
                ui.add_space(16.0);

                let editor_width = (content.width() - 322.0).clamp(370.0, 570.0);
                ui.horizontal_top(|ui| {
                    Frame::NONE
                        .fill(palette.surface)
                        .stroke(Stroke::new(1.0, palette.border))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(270.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("Профили")
                                        .strong()
                                        .color(palette.text),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.button("+ Добавить").clicked() {
                                        self.start_account_edit(None);
                                    }
                                });
                            });
                            ui.separator();
                            let accounts = self.config.accounts.clone();
                            for (index, account) in accounts.iter().enumerate() {
                                let active = self.config.active_account == index;
                                let label = if active {
                                    format!("●  {}", account.username)
                                } else {
                                    format!("○  {}", account.username)
                                };
                                if ui
                                    .selectable_label(active, label)
                                    .on_hover_text("Сделать активным")
                                    .clicked()
                                {
                                    self.activate_account(index);
                                    self.start_account_edit(Some(index));
                                }
                                ui.label(
                                    RichText::new(if account.skin_source.trim().is_empty() {
                                        "Скин не указан"
                                    } else {
                                        "Скин настроен"
                                    })
                                    .size(10.5)
                                    .color(palette.muted),
                                );
                                ui.add_space(6.0);
                            }
                        });

                    ui.add_space(16.0);
                    Frame::NONE
                        .fill(palette.surface)
                        .stroke(Stroke::new(1.0, palette.border))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::same(18))
                        .show(ui, |ui| {
                            ui.set_width(editor_width);
                            ui.label(
                                RichText::new(if self.editing_account.is_some() {
                                    "Редактирование профиля"
                                } else {
                                    "Новый профиль"
                                })
                                .size(18.0)
                                .strong(),
                            );
                            ui.add_space(12.0);
                            ui.label("Ник");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.account_draft.username)
                                    .desired_width(editor_width - 36.0)
                                    .char_limit(16)
                                    .hint_text("Player"),
                            );
                            ui.add_space(8.0);
                            ui.label("Скин: ник Minecraft или URL");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.account_draft.skin_source)
                                    .desired_width(editor_width - 36.0)
                                    .hint_text("Notch или https://…/skin.png"),
                            );
                            ui.add_space(8.0);
                            egui::ComboBox::from_id_salt("account_skin_model")
                                .selected_text(self.account_draft.skin_model.label())
                                .width(230.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.account_draft.skin_model,
                                        SkinModel::Classic,
                                        SkinModel::Classic.label(),
                                    );
                                    ui.selectable_value(
                                        &mut self.account_draft.skin_model,
                                        SkinModel::Slim,
                                        SkinModel::Slim.label(),
                                    );
                                });
                            ui.add_space(16.0);
                            ui.horizontal(|ui| {
                                if ui.button("Сохранить").clicked() {
                                    self.save_account_draft();
                                }
                                if let Some(index) = self.editing_account {
                                    if self.config.active_account != index
                                        && ui.button("Сделать активным").clicked()
                                    {
                                        self.activate_account(index);
                                    }
                                    if ui
                                        .add_enabled(
                                            self.config.accounts.len() > 1,
                                            egui::Button::new("Удалить"),
                                        )
                                        .clicked()
                                    {
                                        self.delete_account(index);
                                    }
                                }
                            });
                            if !self.account_message.is_empty() {
                                ui.add_space(10.0);
                                ui.label(
                                    RichText::new(&self.account_message)
                                        .size(12.0)
                                        .color(palette.accent_text),
                                );
                            }
                        });
                });
            },
        );
    }

    fn draw_about(&mut self, ui: &mut egui::Ui, rect: Rect, palette: Palette) {
        let card = Rect::from_center_size(
            rect.center() - Vec2::new(0.0, 30.0),
            Vec2::new(570.0_f32.min(rect.width() - 48.0), 360.0),
        );
        ui.painter()
            .rect_filled(card, CornerRadius::same(14), palette.surface);
        ui.painter().rect_stroke(
            card,
            CornerRadius::same(14),
            Stroke::new(1.0, palette.border),
            egui::StrokeKind::Inside,
        );
        let logo = Rect::from_center_size(
            card.center_top() + Vec2::new(0.0, 72.0),
            Vec2::splat(52.0),
        );
        draw_logo(ui.painter(), logo);
        ui.painter().text(
            card.center_top() + Vec2::new(0.0, 122.0),
            Align2::CENTER_CENTER,
            "MineLauncher",
            FontId::proportional(26.0),
            palette.text,
        );
        ui.painter().text(
            card.center_top() + Vec2::new(0.0, 153.0),
            Align2::CENTER_CENTER,
            "Версия 1.0.0 · Rust + egui",
            FontId::proportional(13.0),
            palette.muted,
        );
        ui.painter().text(
            card.center_top() + Vec2::new(0.0, 205.0),
            Align2::CENTER_CENTER,
            "Лаунчер для приватной Minecraft-сборки",
            FontId::proportional(15.0),
            palette.text,
        );
        ui.painter().text(
            card.center_top() + Vec2::new(0.0, 234.0),
            Align2::CENTER_CENTER,
            "Сборки Google Drive · NeoForge · офлайн-профили · SkinRestorer",
            FontId::proportional(12.0),
            palette.muted,
        );
        ui.painter().text(
            card.center_bottom() - Vec2::new(0.0, 38.0),
            Align2::CENTER_CENTER,
            "Новости Minecraft намеренно не загружаются.",
            FontId::proportional(12.0),
            palette.accent_text,
        );
    }
}

impl eframe::App for MineLauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        if self.busy != Busy::Idle {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
        if self.page == Page::Console && self.last_log_refresh.elapsed() > Duration::from_secs(1) {
            self.refresh_game_log();
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui| self.draw_shell(ui, ctx));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_prefs();
    }
}

fn prepare_instance_game_dir(
    root: &Path,
    game: &Path,
) -> Result<(), mine_launcher::error::LauncherError> {
    std::fs::create_dir_all(game)?;
    if game == root {
        return Ok(());
    }
    for name in [
        "mods",
        "config",
        "resourcepacks",
        "shaderpacks",
        "saves",
        "defaultconfigs",
        "options.txt",
        "optionsof.txt",
        "servers.dat",
    ] {
        let source = root.join(name);
        let destination = game.join(name);
        if source.exists() && source != destination && !destination.exists() {
            if source.is_dir() {
                copy_dir_recursive(&source, &destination)?;
            } else {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&source, &destination)?;
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(
    source: &Path,
    destination: &Path,
) -> Result<(), mine_launcher::error::LauncherError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            std::fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn guess_mc_version(value: &str) -> Option<String> {
    let regex = regex_lite::Regex::new(r"(1\.\d{1,2}(?:\.\d{1,2})?)").ok()?;
    regex
        .captures(value)
        .and_then(|captures| captures.get(1).map(|value| value.as_str().to_string()))
}

fn launch_game_in_dir(
    version_id: &str,
    username: &str,
    ram_mb: u32,
    java_path: &str,
    game_dir_override: &Path,
) -> Result<std::process::Child, mine_launcher::error::LauncherError> {
    mine_launcher::launch::launch_game_with_dir(
        version_id,
        username,
        ram_mb,
        java_path,
        game_dir_override,
    )
}

fn empty_account() -> AccountConfig {
    AccountConfig {
        username: String::new(),
        skin_source: String::new(),
        skin_model: SkinModel::Classic,
    }
}

fn skin_command(account: &AccountConfig) -> String {
    let source = account.skin_source.trim().replace('"', "");
    if source.is_empty() {
        "/skin update".into()
    } else if source.starts_with("https://") || source.starts_with("http://") {
        format!(
            "/skin url \"{}\" {}",
            source,
            account.skin_model.command_value()
        )
    } else {
        format!("/skin set {source}")
    }
}

fn is_local_skin_source(source: &str) -> bool {
    let source = source.trim();
    !source.is_empty()
        && !source.starts_with("https://")
        && !source.starts_with("http://")
        && (Path::new(source).is_absolute()
            || source.to_ascii_lowercase().ends_with(".png")
            || source.contains('\\')
            || source.contains('/'))
}

fn can_apply_skin_source(source: &str) -> bool {
    !source.trim().is_empty()
}

fn validate_skin_file(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err("Выбранный файл не найден".into());
    }
    let is_png = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"));
    if !is_png {
        return Err("Выберите скин в формате PNG".into());
    }
    let (width, height) =
        image::image_dimensions(path).map_err(|_| "Не удалось прочитать PNG-файл".to_string())?;
    if (width, height) != (64, 64) && (width, height) != (64, 32) {
        return Err(format!(
            "Неверный размер скина: {width}×{height}. Нужен PNG 64×64 или 64×32"
        ));
    }
    Ok(())
}

fn skin_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("skin.png")
        .to_string()
}

fn java_status(configured: &str) -> String {
    match find_java(configured) {
        Some(path) => {
            let version = java_version_string(&path)
                .unwrap_or_else(|| path.to_string_lossy().to_string());
            format!("Найдена: {version}")
        }
        None => "Java не найдена — установите Java 17 или 21".into(),
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} ГБ", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} МБ", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0} КБ", bytes / KIB)
    } else {
        format!("{bytes:.0} Б")
    }
}

fn configured_builds_root(config: &Config) -> Option<PathBuf> {
    let path = config.builds_directory.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn display_builds_root(config: &Config) -> String {
    configured_builds_root(config)
        .unwrap_or_else(builds_root)
        .to_string_lossy()
        .to_string()
}

fn section_title(ui: &mut egui::Ui, title: &str, color: Color32) {
    ui.label(RichText::new(title).size(11.0).strong().color(color));
    ui.add_space(4.0);
}

fn setting_row(ui: &mut egui::Ui, label: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(40.0);
        ui.add_sized(Vec2::new(108.0, 34.0), egui::Label::new(label));
        content(ui);
    });
}

fn top_tab(
    ui: &mut egui::Ui,
    rect: Rect,
    label: &str,
    selected: bool,
    palette: Palette,
) -> egui::Response {
    let response = ui.interact(rect, ui.id().with(("top_tab", label)), Sense::click());
    let color = if selected || response.hovered() {
        palette.text
    } else {
        palette.muted
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(14.0),
        color,
    );
    response
}

fn animated_tab_indicator(
    ui: &mut egui::Ui,
    id_source: &'static str,
    target: Rect,
    palette: Palette,
) {
    let left = ui.ctx().animate_value_with_time(
        ui.id().with((id_source, "indicator_left")),
        target.left() + 8.0,
        0.24,
    );
    let width = ui.ctx().animate_value_with_time(
        ui.id().with((id_source, "indicator_width")),
        target.width() - 16.0,
        0.24,
    );
    let indicator = Rect::from_min_size(
        Pos2::new(left, target.bottom() - 3.0),
        Vec2::new(width.max(8.0), 3.0),
    );
    ui.painter().rect_filled(
        indicator.expand2(Vec2::new(2.0, 1.0)),
        CornerRadius::same(2),
        color_with_alpha(palette.accent, 0.18),
    );
    ui.painter()
        .rect_filled(indicator, CornerRadius::same(2), palette.accent);
}

fn side_button(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: &str,
    label: &str,
    selected: bool,
    palette: Palette,
) -> egui::Response {
    let id = ui.id().with(("side", label));
    let response = ui.interact(rect, id, Sense::click());
    let selected_t = ui
        .ctx()
        .animate_bool_with_time(id.with("selected"), selected, 0.18);
    let hover_t = ui
        .ctx()
        .animate_bool_with_time(id.with("hovered"), response.hovered(), 0.12);

    if selected_t > 0.0 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::ZERO,
            color_with_alpha(palette.sidebar_selected, selected_t),
        );
        let bar_height = rect.height() * (0.35 + 0.65 * selected_t);
        ui.painter().rect_filled(
            Rect::from_center_size(
                Pos2::new(rect.left() + 1.5, rect.center().y),
                Vec2::new(3.0, bar_height),
            ),
            CornerRadius::ZERO,
            color_with_alpha(palette.accent, selected_t),
        );
    }
    if hover_t > 0.0 && selected_t < 1.0 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(
                palette.surface.r(),
                palette.surface.g(),
                palette.surface.b(),
                (95.0 * hover_t * (1.0 - selected_t)) as u8,
            ),
        );
    }
    let color = if selected || response.hovered() {
        palette.text
    } else {
        palette.muted
    };
    ui.painter().text(
        Pos2::new(rect.left() + 17.0, rect.center().y),
        Align2::LEFT_CENTER,
        icon,
        FontId::monospace(16.0),
        color,
    );
    ui.painter().text(
        Pos2::new(rect.left() + 48.0, rect.center().y),
        Align2::LEFT_CENTER,
        truncate(label, 19),
        FontId::proportional(13.0),
        color,
    );
    response
}

fn color_with_alpha(color: Color32, opacity: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (color.a() as f32 * opacity.clamp(0.0, 1.0)).round() as u8,
    )
}

fn draw_eye_icon(painter: &egui::Painter, center: Pos2, color: Color32) {
    painter.line_segment(
        [center + Vec2::new(-7.0, 0.0), center + Vec2::new(-2.5, -3.5)],
        Stroke::new(1.1, color),
    );
    painter.line_segment(
        [center + Vec2::new(-2.5, -3.5), center + Vec2::new(2.5, -3.5)],
        Stroke::new(1.1, color),
    );
    painter.line_segment(
        [center + Vec2::new(2.5, -3.5), center + Vec2::new(7.0, 0.0)],
        Stroke::new(1.1, color),
    );
    painter.line_segment(
        [center + Vec2::new(7.0, 0.0), center + Vec2::new(2.5, 3.5)],
        Stroke::new(1.1, color),
    );
    painter.line_segment(
        [center + Vec2::new(2.5, 3.5), center + Vec2::new(-2.5, 3.5)],
        Stroke::new(1.1, color),
    );
    painter.line_segment(
        [center + Vec2::new(-2.5, 3.5), center + Vec2::new(-7.0, 0.0)],
        Stroke::new(1.1, color),
    );
    painter.circle_filled(center, 2.0, color);
}

fn draw_shirt_icon(painter: &egui::Painter, center: Pos2, color: Color32) {
    let points = [
        center + Vec2::new(-7.0, -5.0),
        center + Vec2::new(-3.0, -7.0),
        center + Vec2::new(-1.5, -3.0),
        center + Vec2::new(1.5, -3.0),
        center + Vec2::new(3.0, -7.0),
        center + Vec2::new(7.0, -5.0),
        center + Vec2::new(5.0, 0.0),
        center + Vec2::new(3.5, -1.0),
        center + Vec2::new(3.5, 7.0),
        center + Vec2::new(-3.5, 7.0),
        center + Vec2::new(-3.5, -1.0),
        center + Vec2::new(-5.0, 0.0),
        center + Vec2::new(-7.0, -5.0),
    ];
    for segment in points.windows(2) {
        painter.line_segment([segment[0], segment[1]], Stroke::new(1.1, color));
    }
}

fn draw_logo(painter: &egui::Painter, rect: Rect) {
    let half = rect.size() / 2.0;
    painter.rect_filled(
        Rect::from_min_size(rect.min, half),
        CornerRadius::same(2),
        Color32::from_rgb(255, 87, 89),
    );
    painter.rect_filled(
        Rect::from_min_size(rect.min + Vec2::new(half.x, 0.0), half),
        CornerRadius::same(2),
        Color32::from_rgb(246, 167, 58),
    );
    painter.rect_filled(
        Rect::from_min_size(rect.min + Vec2::new(0.0, half.y), half),
        CornerRadius::same(2),
        Color32::from_rgb(110, 80, 235),
    );
    painter.rect_filled(
        Rect::from_min_size(rect.min + half, half),
        CornerRadius::same(2),
        Color32::from_rgb(61, 198, 147),
    );
}

fn draw_avatar(painter: &egui::Painter, rect: Rect, palette: Palette) {
    painter.rect_filled(rect, CornerRadius::same(3), Color32::from_rgb(218, 158, 96));
    let face = rect.shrink2(Vec2::new(7.0, 5.0));
    painter.rect_filled(face, CornerRadius::ZERO, Color32::from_rgb(241, 201, 161));
    let eye = Vec2::splat(4.0);
    painter.rect_filled(
        Rect::from_min_size(face.min + Vec2::new(4.0, 9.0), eye),
        CornerRadius::ZERO,
        Color32::from_rgb(53, 82, 65),
    );
    painter.rect_filled(
        Rect::from_min_size(face.right_top() + Vec2::new(-8.0, 9.0), eye),
        CornerRadius::ZERO,
        Color32::from_rgb(53, 82, 65),
    );
    painter.rect_filled(
        Rect::from_min_size(face.center_bottom() + Vec2::new(-4.0, -7.0), Vec2::new(8.0, 3.0)),
        CornerRadius::ZERO,
        palette.danger,
    );
}

fn draw_build_icon(painter: &egui::Painter, rect: Rect, palette: Palette) {
    painter.rect_filled(rect, CornerRadius::same(7), palette.code);
    painter.rect_stroke(
        rect,
        CornerRadius::same(7),
        Stroke::new(1.0, palette.border),
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(rect.center(), rect.width() * 0.29, Color32::from_rgb(196, 75, 28));
    painter.circle_stroke(
        rect.center(),
        rect.width() * 0.21,
        Stroke::new(4.0, Color32::from_rgb(242, 120, 38)),
    );
    for offset in [
        Vec2::new(0.0, -14.0),
        Vec2::new(0.0, 14.0),
        Vec2::new(-14.0, 0.0),
        Vec2::new(14.0, 0.0),
    ] {
        painter.circle_filled(rect.center() + offset, 4.0, Color32::from_rgb(225, 96, 29));
    }
}

fn paint_minecraft_placeholder(painter: &egui::Painter, rect: Rect, palette: Palette) {
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(19, 32, 29));
    let bands = [
        Color32::from_rgb(24, 48, 42),
        Color32::from_rgb(27, 57, 48),
        Color32::from_rgb(19, 43, 38),
        Color32::from_rgb(18, 34, 32),
    ];
    for (index, color) in bands.iter().enumerate() {
        let band = Rect::from_min_size(
            Pos2::new(rect.left(), rect.top() + rect.height() * index as f32 / 4.0),
            Vec2::new(rect.width(), rect.height() / 4.0 + 1.0),
        );
        painter.rect_filled(band, CornerRadius::ZERO, *color);
    }

    let block = (rect.width() / 18.0).clamp(38.0, 68.0);
    let baseline = rect.bottom() - block * 0.4;
    for column in 0..19 {
        let height = 2 + ((column * 7 + 3) % 6);
        for row in 0..height {
            let x = rect.left() + column as f32 * block - block * 0.3;
            let y = baseline - row as f32 * block;
            let color = if (column + row) % 4 == 0 {
                Color32::from_rgb(35, 76, 59)
            } else if (column + row) % 3 == 0 {
                Color32::from_rgb(32, 64, 54)
            } else {
                Color32::from_rgb(27, 55, 47)
            };
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x, y), Vec2::splat(block + 1.0)),
                CornerRadius::ZERO,
                color,
            );
            painter.line_segment(
                [Pos2::new(x, y), Pos2::new(x + block, y)],
                Stroke::new(1.0, Color32::from_black_alpha(45)),
            );
        }
    }
    painter.text(
        rect.center() - Vec2::new(0.0, 62.0),
        Align2::CENTER_CENTER,
        "MINE LAUNCHER",
        FontId::proportional(28.0),
        Color32::from_rgba_unmultiplied(255, 255, 255, 215),
    );
    painter.text(
        rect.center() - Vec2::new(0.0, 29.0),
        Align2::CENTER_CENTER,
        "ВАШ СКРИНШОТ БУДЕТ ЗДЕСЬ",
        FontId::proportional(11.0),
        palette.accent,
    );
}

fn load_steve_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let image = image::load_from_memory(include_bytes!(
        "../assets/minecraft/textures/entity/player/wide/steve.png"
    ))
    .expect("embedded Steve skin must be a valid PNG")
    .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    ctx.load_texture(
        "minecraft:entity/player/wide/steve",
        color_image,
        egui::TextureOptions::NEAREST,
    )
}

fn load_skin_texture(ctx: &egui::Context, path: &Path) -> Result<egui::TextureHandle, String> {
    validate_skin_file(path)?;
    let mut image = image::open(path)
        .map_err(|_| "Не удалось прочитать PNG-файл".to_string())?
        .to_rgba8();

    // Старые скины 64×32 используют одну текстуру для обеих рук и ног.
    // Дублируем эти области в позиции современного формата 64×64, чтобы
    // превью не теряло половину модели.
    if image.height() == 32 {
        let mut expanded = image::RgbaImage::new(64, 64);
        image::imageops::overlay(&mut expanded, &image, 0, 0);
        for y in 0..16 {
            for x in 0..16 {
                expanded.put_pixel(x + 16, y + 48, *image.get_pixel(x, y + 16));
                expanded.put_pixel(x + 32, y + 48, *image.get_pixel(x + 40, y + 16));
            }
        }
        image = expanded;
    }

    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Ok(ctx.load_texture(
        format!("skin:{}", path.to_string_lossy()),
        color_image,
        egui::TextureOptions::NEAREST,
    ))
}

#[derive(Clone, Copy)]
struct SkinFaces {
    front: [f32; 4],
    right: [f32; 4],
    top: [f32; 4],
}

fn draw_skin_avatar_3d(
    painter: &egui::Painter,
    texture: egui::TextureId,
    rect: Rect,
    _palette: Palette,
) {
    // Minecraft's classic model is 16 px wide and 32 px tall. Keep those exact
    // proportions and only add a small isometric depth so the preview reads as 3D.
    let unit = (rect.height() / 34.0)
        .min(rect.width() / 18.0)
        .clamp(1.7, 10.5);
    let depth = unit * 1.25;
    let model_size = Vec2::new(unit * 16.0 + depth, unit * 32.0 + depth * 0.55);
    let origin = rect.center() - model_size * 0.5 + Vec2::new(-depth * 0.15, depth * 0.28);

    let shadow = Rect::from_center_size(
        Pos2::new(
            rect.center().x + depth * 0.35,
            origin.y + unit * 32.0 + depth * 0.2,
        ),
        Vec2::new(unit * 12.5, unit * 1.25),
    );
    painter.rect_filled(
        shadow,
        CornerRadius::same((unit * 0.6).round().clamp(1.0, 255.0) as u8),
        Color32::from_rgba_unmultiplied(0, 0, 0, 72),
    );

    let head = Rect::from_min_size(origin + Vec2::new(unit * 4.0, 0.0), Vec2::splat(unit * 8.0));
    let body = Rect::from_min_size(
        origin + Vec2::new(unit * 4.0, unit * 8.0),
        Vec2::new(unit * 8.0, unit * 12.0),
    );
    let screen_left_arm = Rect::from_min_size(
        origin + Vec2::new(0.0, unit * 8.0),
        Vec2::new(unit * 4.0, unit * 12.0),
    );
    let screen_right_arm = Rect::from_min_size(
        origin + Vec2::new(unit * 12.0, unit * 8.0),
        Vec2::new(unit * 4.0, unit * 12.0),
    );
    let screen_left_leg = Rect::from_min_size(
        origin + Vec2::new(unit * 4.0, unit * 20.0),
        Vec2::new(unit * 4.0, unit * 12.0),
    );
    let screen_right_leg = Rect::from_min_size(
        origin + Vec2::new(unit * 8.0, unit * 20.0),
        Vec2::new(unit * 4.0, unit * 12.0),
    );

    // Render the outer face of each arm away from the torso. Using the same
    // depth direction for both arms makes the left arm fold into the body.
    draw_skin_cuboid(
        painter,
        texture,
        screen_right_arm,
        depth * 0.52,
        SkinFaces {
            front: [36.0, 52.0, 40.0, 64.0],
            right: [40.0, 52.0, 44.0, 64.0],
            top: [36.0, 48.0, 40.0, 52.0],
        },
    );
    draw_skin_cuboid(
        painter,
        texture,
        screen_right_leg,
        depth * 0.52,
        SkinFaces {
            front: [20.0, 52.0, 24.0, 64.0],
            right: [16.0, 52.0, 20.0, 64.0],
            top: [20.0, 48.0, 24.0, 52.0],
        },
    );
    draw_skin_cuboid(
        painter,
        texture,
        screen_left_leg,
        depth * 0.52,
        SkinFaces {
            front: [4.0, 20.0, 8.0, 32.0],
            right: [0.0, 20.0, 4.0, 32.0],
            top: [4.0, 16.0, 8.0, 20.0],
        },
    );
    draw_skin_cuboid(
        painter,
        texture,
        body,
        depth,
        SkinFaces {
            front: [20.0, 20.0, 28.0, 32.0],
            right: [16.0, 20.0, 20.0, 32.0],
            top: [20.0, 16.0, 28.0, 20.0],
        },
    );
    draw_skin_cuboid(
        painter,
        texture,
        screen_left_arm,
        -depth * 0.52,
        SkinFaces {
            front: [44.0, 20.0, 48.0, 32.0],
            right: [40.0, 20.0, 44.0, 32.0],
            top: [44.0, 16.0, 48.0, 20.0],
        },
    );
    draw_skin_cuboid(
        painter,
        texture,
        head,
        depth,
        SkinFaces {
            front: [8.0, 8.0, 16.0, 16.0],
            right: [0.0, 8.0, 8.0, 16.0],
            top: [8.0, 0.0, 16.0, 8.0],
        },
    );
}

fn draw_skin_cuboid(
    painter: &egui::Painter,
    texture: egui::TextureId,
    front: Rect,
    depth: f32,
    faces: SkinFaces,
) {
    let offset = Vec2::new(depth, -depth.abs() * 0.55);
    let mut mesh = egui::Mesh::with_texture(texture);

    add_skin_quad(
        &mut mesh,
        [
            front.left_top() + offset,
            front.right_top() + offset,
            front.left_top(),
            front.right_top(),
        ],
        faces.top,
        Color32::WHITE,
    );
    let side = if depth >= 0.0 {
        [
            front.right_top(),
            front.right_top() + offset,
            front.right_bottom(),
            front.right_bottom() + offset,
        ]
    } else {
        [
            front.left_top() + offset,
            front.left_top(),
            front.left_bottom() + offset,
            front.left_bottom(),
        ]
    };
    add_skin_quad(
        &mut mesh,
        side,
        faces.right,
        Color32::from_rgb(174, 174, 174),
    );
    add_skin_quad(
        &mut mesh,
        [
            front.left_top(),
            front.right_top(),
            front.left_bottom(),
            front.right_bottom(),
        ],
        faces.front,
        Color32::WHITE,
    );
    painter.add(egui::Shape::mesh(mesh));
}

fn add_skin_quad(
    mesh: &mut egui::Mesh,
    points: [Pos2; 4],
    texture_pixels: [f32; 4],
    tint: Color32,
) {
    let index = mesh.vertices.len() as u32;
    let [u0, v0, u1, v1] = texture_pixels;
    let uvs = [
        Pos2::new(u0 / 64.0, v0 / 64.0),
        Pos2::new(u1 / 64.0, v0 / 64.0),
        Pos2::new(u0 / 64.0, v1 / 64.0),
        Pos2::new(u1 / 64.0, v1 / 64.0),
    ];
    for (pos, uv) in points.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos,
            uv,
            color: tint,
        });
    }
    mesh.add_triangle(index, index + 1, index + 2);
    mesh.add_triangle(index + 2, index + 1, index + 3);
}

fn draw_dashed_rect(painter: &egui::Painter, rect: Rect, color: Color32) {
    let dash = 6.0;
    let gap = 5.0;
    let stroke = Stroke::new(1.0, color);
    let mut x = rect.left();
    while x < rect.right() {
        painter.line_segment(
            [
                Pos2::new(x, rect.top()),
                Pos2::new((x + dash).min(rect.right()), rect.top()),
            ],
            stroke,
        );
        painter.line_segment(
            [
                Pos2::new(x, rect.bottom()),
                Pos2::new((x + dash).min(rect.right()), rect.bottom()),
            ],
            stroke,
        );
        x += dash + gap;
    }
    let mut y = rect.top();
    while y < rect.bottom() {
        painter.line_segment(
            [
                Pos2::new(rect.left(), y),
                Pos2::new(rect.left(), (y + dash).min(rect.bottom())),
            ],
            stroke,
        );
        painter.line_segment(
            [
                Pos2::new(rect.right(), y),
                Pos2::new(rect.right(), (y + dash).min(rect.bottom())),
            ],
            stroke,
        );
        y += dash + gap;
    }
}

fn draw_empty_gallery(painter: &egui::Painter, rect: Rect, palette: Palette) {
    let center = rect.center() - Vec2::new(0.0, 22.0);
    let card = Rect::from_center_size(
        center,
        Vec2::new(540.0_f32.min(rect.width() - 20.0), 260.0_f32.min(rect.height() - 20.0)),
    );
    painter.rect_filled(card, CornerRadius::same(14), palette.surface);
    painter.rect_stroke(
        card,
        CornerRadius::same(14),
        Stroke::new(1.0, palette.border),
        egui::StrokeKind::Inside,
    );
    let icon = Rect::from_center_size(card.center_top() + Vec2::new(0.0, 82.0), Vec2::new(82.0, 58.0));
    painter.rect_stroke(
        icon,
        CornerRadius::same(8),
        Stroke::new(2.0, palette.accent),
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(icon.left_top() + Vec2::new(20.0, 18.0), 6.0, palette.accent);
    painter.line_segment(
        [
            icon.left_bottom() + Vec2::new(8.0, -10.0),
            icon.center() + Vec2::new(4.0, 4.0),
        ],
        Stroke::new(2.0, palette.accent),
    );
    painter.line_segment(
        [
            icon.center() + Vec2::new(4.0, 4.0),
            icon.right_bottom() + Vec2::new(-8.0, -10.0),
        ],
        Stroke::new(2.0, palette.accent),
    );
    painter.text(
        card.center() + Vec2::new(0.0, 30.0),
        Align2::CENTER_CENTER,
        "В галерее пока нет скриншотов",
        FontId::proportional(17.0),
        palette.text,
    );
    painter.text(
        card.center() + Vec2::new(0.0, 60.0),
        Align2::CENTER_CENTER,
                    "Добавьте PNG или JPG в папку screenshots",
        FontId::proportional(12.0),
        palette.muted,
    );
}

fn paint_cover(painter: &egui::Painter, texture: egui::TextureId, rect: Rect, aspect: f32) {
    let rect_aspect = rect.width() / rect.height().max(1.0);
    let uv = if aspect > rect_aspect {
        let visible = rect_aspect / aspect;
        let margin = (1.0 - visible) / 2.0;
        Rect::from_min_max(Pos2::new(margin, 0.0), Pos2::new(1.0 - margin, 1.0))
    } else {
        let visible = aspect / rect_aspect;
        let margin = (1.0 - visible) / 2.0;
        Rect::from_min_max(Pos2::new(0.0, margin), Pos2::new(1.0, 1.0 - margin))
    };
    painter.image(texture, rect, uv, Color32::WHITE);
}

fn open_path(path: &Path) -> std::io::Result<()> {
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

#[cfg(windows)]
fn select_folder() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = concat!(
        "Add-Type -AssemblyName System.Windows.Forms; ",
        "$dialog = New-Object System.Windows.Forms.FolderBrowserDialog; ",
        "$dialog.Description = 'Выберите папку MineLauncher'; ",
        "$dialog.ShowNewFolderButton = $true; ",
        "if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { ",
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ",
        "Write-Output $dialog.SelectedPath }"
    );
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!selected.is_empty()).then(|| PathBuf::from(selected))
}

#[cfg(windows)]
fn select_skin_file() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = concat!(
        "Add-Type -AssemblyName System.Windows.Forms; ",
        "$dialog = New-Object System.Windows.Forms.OpenFileDialog; ",
        "$dialog.Title = 'Выберите скин Minecraft'; ",
        "$dialog.Filter = 'PNG-скины (*.png)|*.png'; ",
        "$dialog.CheckFileExists = $true; ",
        "$dialog.Multiselect = $false; ",
        "if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { ",
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ",
        "Write-Output $dialog.FileName }"
    );
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!selected.is_empty()).then(|| PathBuf::from(selected))
}

#[cfg(not(windows))]
fn select_folder() -> Option<PathBuf> {
    let output = std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--directory",
            "--title=Папка MineLauncher",
        ])
        .output()
        .ok()?;
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !selected.is_empty()).then(|| PathBuf::from(selected))
}

#[cfg(not(windows))]
fn select_skin_file() -> Option<PathBuf> {
    let output = std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--title=Выберите скин Minecraft",
            "--file-filter=PNG-скины | *.png",
        ])
        .output()
        .ok()?;
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !selected.is_empty()).then(|| PathBuf::from(selected))
}

#[derive(Clone, Copy)]
struct Palette {
    background: Color32,
    titlebar: Color32,
    sidebar: Color32,
    sidebar_profile: Color32,
    sidebar_selected: Color32,
    surface: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    disabled: Color32,
    accent: Color32,
    accent_hover: Color32,
    accent_dim: Color32,
    accent_text: Color32,
    danger: Color32,
    progress_track: Color32,
    code: Color32,
    console: Color32,
    console_text: Color32,
}

impl Palette {
    fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                background: Color32::from_rgb(23, 24, 24),
                titlebar: Color32::from_rgb(18, 19, 19),
                sidebar: Color32::from_rgb(18, 19, 19),
                sidebar_profile: Color32::from_rgb(25, 26, 26),
                sidebar_selected: Color32::from_rgb(24, 46, 38),
                surface: Color32::from_rgb(28, 29, 29),
                border: Color32::from_rgb(48, 50, 50),
                text: Color32::from_rgb(242, 244, 243),
                muted: Color32::from_rgb(136, 143, 141),
                disabled: Color32::from_rgb(60, 63, 62),
                accent: Color32::from_rgb(42, 188, 128),
                accent_hover: Color32::from_rgb(49, 211, 143),
                accent_dim: Color32::from_rgb(41, 109, 83),
                accent_text: Color32::from_rgb(82, 222, 161),
                danger: Color32::from_rgb(212, 76, 80),
                progress_track: Color32::from_rgb(49, 54, 52),
                code: Color32::from_rgb(17, 19, 18),
                console: Color32::from_rgb(11, 13, 12),
                console_text: Color32::from_rgb(196, 208, 201),
            },
            Theme::Light => Self {
                background: Color32::from_rgb(238, 241, 240),
                titlebar: Color32::from_rgb(255, 255, 255),
                sidebar: Color32::from_rgb(248, 249, 249),
                sidebar_profile: Color32::from_rgb(255, 255, 255),
                sidebar_selected: Color32::from_rgb(218, 241, 232),
                surface: Color32::from_rgb(255, 255, 255),
                border: Color32::from_rgb(210, 216, 213),
                text: Color32::from_rgb(29, 35, 32),
                muted: Color32::from_rgb(103, 113, 108),
                disabled: Color32::from_rgb(211, 216, 214),
                accent: Color32::from_rgb(28, 154, 97),
                accent_hover: Color32::from_rgb(34, 177, 111),
                accent_dim: Color32::from_rgb(139, 205, 174),
                accent_text: Color32::from_rgb(17, 125, 77),
                danger: Color32::from_rgb(196, 61, 67),
                progress_track: Color32::from_rgb(207, 216, 212),
                code: Color32::from_rgb(235, 240, 237),
                console: Color32::from_rgb(25, 29, 27),
                console_text: Color32::from_rgb(215, 225, 219),
            },
        }
    }
}

fn configure_style(ctx: &egui::Context, theme: Theme) {
    let palette = Palette::for_theme(theme);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(9.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::proportional(14.0),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::proportional(13.5),
    );
    ctx.set_style(style);

    let mut visuals = match theme {
        Theme::Dark => egui::Visuals::dark(),
        Theme::Light => egui::Visuals::light(),
    };
    visuals.override_text_color = Some(palette.text);
    visuals.panel_fill = palette.background;
    visuals.window_fill = palette.surface;
    visuals.extreme_bg_color = palette.code;
    visuals.faint_bg_color = palette.surface;
    visuals.widgets.inactive.bg_fill = palette.surface;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, palette.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.hovered.bg_fill = palette.progress_track;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, palette.accent_dim);
    visuals.widgets.active.bg_fill = palette.accent;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.selection.bg_fill = palette.accent;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    ctx.set_visuals(visuals);
}
