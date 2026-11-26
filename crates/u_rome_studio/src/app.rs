//! The eframe application: shared state ([`Studio`]), menus, status bar,
//! dock, settings window and keyboard shortcuts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_dock::{DockArea, DockState};

use crate::backend::mock::MockProvider;
use crate::backend::{Backend, BackendProvider, ByteSource, EntryDetails, EntryId, EntryInfo};
use crate::diagnostics::{self, Diagnostics};
use crate::jobs::{JobHandle, JobSystem};
use crate::preview::{GpuContext, PreviewRegistry};
use crate::search::SearchIndex;
use crate::settings::{Settings, Theme};
use crate::ui::explorer::ExplorerState;
use crate::ui::hex_view::HexState;
use crate::ui::log_panel::LogViewState;
use crate::ui::preview_panel::PreviewState;
use crate::ui::search_panel::SearchState;
use crate::ui::{self, Tab, style};
use crate::{paths, util};

const DOCK_KEY: &str = "dock_v1";

/// Backend providers in priority order. The integrator inserts the real RPF9
/// provider before the mock; the first provider whose `accepts` returns true
/// for the chosen folder is used.
pub fn providers() -> Vec<Arc<dyn BackendProvider>> {
    vec![
        Arc::new(crate::backend::index::IndexProvider::default()),
        Arc::new(MockProvider::default()),
    ]
}

/// Result of a quick sanity check on a chosen install folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameDirCheck {
    pub exists: bool,
    pub rpf_files: usize,
    pub scanned_entries: usize,
}

/// Counts `.rpf` files up to 4 levels deep (at most 20k directory entries),
/// so a wrong folder can be flagged before a long scan.
pub fn check_game_dir(root: &Path) -> GameDirCheck {
    let mut out = GameDirCheck {
        exists: root.is_dir(),
        ..Default::default()
    };
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            out.scanned_entries += 1;
            if out.scanned_entries > 20_000 {
                return out;
            }
            let p = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && depth < 4 {
                stack.push((p, depth + 1));
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("rpf")) {
                out.rpf_files += 1;
            }
        }
    }
    out
}

/// The current selection and its loaded data.
#[derive(Default)]
pub struct Selection {
    pub id: Option<EntryId>,
    pub info: Option<EntryInfo>,
    pub details: Option<Result<EntryDetails, String>>,
    pub source: Option<Arc<dyn ByteSource>>,
    pub source_error: Option<String>,
    open_job: Option<JobHandle<Arc<dyn ByteSource>>>,
}

impl Selection {
    pub fn is_loading(&self) -> bool {
        self.open_job.is_some()
    }
}

/// All state panels read and mutate (everything except the dock itself).
pub struct Studio {
    pub settings: Settings,
    settings_path: PathBuf,
    settings_saved: Settings,
    pub jobs: JobSystem,
    providers: Vec<Arc<dyn BackendProvider>>,
    pub backend: Option<Arc<dyn Backend>>,
    pub backend_status: String,
    pending_backend: Option<JobHandle<Arc<dyn Backend>>>,
    pub index: Option<Arc<SearchIndex>>,
    pending_index: Option<JobHandle<SearchIndex>>,
    pub index_generation: u64,
    pub explorer: ExplorerState,
    pub search: SearchState,
    pub selection: Selection,
    pub hex: HexState,
    pub preview: PreviewState,
    pub log_view: LogViewState,
    pub previews: PreviewRegistry,
    pub gpu: GpuContext,
    /// Tab the next frame should bring to front.
    pub focus_tab: Option<Tab>,
    pub show_settings: bool,
    pub show_about: bool,
    notice: Option<(String, Instant)>,
}

impl Studio {
    pub fn new(settings: Settings, settings_path: PathBuf, gpu: GpuContext) -> Self {
        let bytes_per_row = settings.hex_bytes_per_row as usize;
        Self {
            settings_saved: settings.clone(),
            settings,
            settings_path,
            jobs: JobSystem::with_default_workers(),
            providers: providers(),
            backend: None,
            backend_status: "No install loaded".into(),
            pending_backend: None,
            index: None,
            pending_index: None,
            index_generation: 0,
            explorer: ExplorerState::default(),
            search: SearchState::default(),
            selection: Selection::default(),
            hex: HexState::new(bytes_per_row),
            preview: PreviewState::default(),
            log_view: LogViewState::default(),
            previews: PreviewRegistry::with_builtin(),
            gpu,
            focus_tab: None,
            show_settings: false,
            show_about: false,
            notice: None,
        }
    }

    /// Shows a transient message in the status bar (and logs it).
    pub fn notify(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.notice = Some((msg, Instant::now()));
    }

    /// Opens (or re-scans) an install with the first accepting provider.
    pub fn open_install(&mut self, root: Option<PathBuf>) {
        if let Some(job) = self.pending_backend.take() {
            job.cancel();
        }
        if let Some(r) = &root {
            let check = check_game_dir(r);
            log::info!("install folder {}: {check:?}", r.display());
            if !check.exists {
                self.notify(format!("Folder does not exist: {}", r.display()));
                return;
            }
            if check.rpf_files == 0 {
                log::warn!(
                    "no .rpf archives found under {} (scanned {} entries); is this the game folder?",
                    r.display(),
                    check.scanned_entries
                );
            }
        }
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| p.accepts(root.as_deref()))
            .cloned()
        else {
            self.notify("No backend accepts this folder");
            return;
        };
        let label = match &root {
            Some(r) => format!("Open install {}", r.display()),
            None => "Generate demo install".to_owned(),
        };
        log::info!("{label} with provider {}", provider.name());
        self.backend_status = format!("Loading ({})...", provider.name());
        self.pending_backend = Some(self.jobs.spawn(label, move |ctx| {
            provider.open(root.as_deref(), ctx).map_err(Into::into)
        }));
    }

    pub fn rescan(&mut self) {
        let root = self
            .backend
            .as_ref()
            .and_then(|b| b.root_path())
            .or_else(|| self.settings.game_dir.clone());
        self.open_install(root);
    }

    fn set_backend(&mut self, backend: Arc<dyn Backend>) {
        let stats = backend.stats();
        self.backend_status = format!(
            "{}: {} archives, {} entries",
            backend.label(),
            util::format_count(stats.archives),
            util::format_count(stats.entries)
        );
        diagnostics::set_backend_label(self.backend_status.clone());
        self.explorer.reset(backend.as_ref());
        self.selection = Selection::default();
        self.hex.set_source(None, String::new());
        self.preview.clear();
        self.index = None;
        self.search.invalidate();
        if let Some(j) = self.pending_index.take() {
            j.cancel();
        }
        let b = Arc::clone(&backend);
        self.pending_index = Some(self.jobs.spawn("Build search index", move |ctx| {
            SearchIndex::from_backend(b.as_ref(), ctx).map_err(Into::into)
        }));
        self.backend = Some(backend);
    }

    /// Selects an entry: loads details now, decoded bytes on a worker.
    pub fn select(&mut self, id: EntryId) {
        if self.selection.id == Some(id) {
            return;
        }
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if let Some(j) = self.selection.open_job.take() {
            j.cancel();
        }
        let info = backend.entry(id).map_err(|e| e.to_string());
        let details = backend.details(id).map_err(|e| {
            log::warn!("details for entry {id:#x} failed: {e}");
            e.to_string()
        });
        self.selection = Selection {
            id: Some(id),
            info: info.as_ref().ok().cloned(),
            details: Some(details),
            ..Default::default()
        };
        self.hex.set_source(None, String::new());
        self.preview.clear();
        let name = info
            .as_ref()
            .map(EntryInfo::display_name)
            .unwrap_or_else(|_| format!("{id:#x}"));
        self.selection.open_job = Some(
            self.jobs
                .spawn(format!("Open {}", util::truncate(&name, 48)), move |ctx| {
                    backend.open(id, ctx).map_err(Into::into)
                }),
        );
    }

    /// Selects and reveals an entry in the Explorer tree.
    pub fn reveal(&mut self, id: EntryId) {
        self.select(id);
        if let Some(b) = self.backend.clone() {
            self.explorer.reveal(b.as_ref(), id);
        }
        self.focus_tab = Some(Tab::Explorer);
    }

    /// Polls background jobs (called every frame on the UI thread).
    fn poll(&mut self) {
        if let Some(r) = self.pending_backend.as_ref().and_then(JobHandle::try_take) {
            self.pending_backend = None;
            match r {
                Ok(b) => self.set_backend(b),
                Err(e) => {
                    self.backend_status = format!("Load failed: {e}");
                    log::error!("opening install failed: {e}");
                }
            }
        }
        if let Some(r) = self.pending_index.as_ref().and_then(JobHandle::try_take) {
            self.pending_index = None;
            match r {
                Ok(idx) => {
                    log::info!("search index ready: {} entries", idx.len());
                    self.index = Some(Arc::new(idx));
                    self.index_generation += 1;
                    self.search.invalidate();
                }
                Err(e) => log::warn!("search index failed: {e}"),
            }
        }
        if let Some(r) = self
            .selection
            .open_job
            .as_ref()
            .and_then(JobHandle::try_take)
        {
            self.selection.open_job = None;
            match r {
                Ok(src) => {
                    let label = self
                        .selection
                        .info
                        .as_ref()
                        .map(EntryInfo::display_name)
                        .unwrap_or_default();
                    self.selection.source = Some(Arc::clone(&src));
                    self.hex.set_source(Some(Arc::clone(&src)), label);
                    if let Some(info) = self.selection.info.clone() {
                        self.preview.open(
                            &self.previews,
                            info,
                            src,
                            self.jobs.clone(),
                            self.gpu.clone(),
                        );
                    }
                }
                Err(e) => {
                    log::warn!("opening entry failed: {e}");
                    self.selection.source_error = Some(e.to_string());
                }
            }
        }
        self.explorer.poll();
        self.hex.poll();
    }

    pub fn index_status(&self) -> String {
        match (&self.index, &self.pending_index) {
            (_, Some(j)) => match j.progress() {
                Some(p) => format!("Indexing {:.0}%", p * 100.0),
                None => "Indexing...".into(),
            },
            (Some(i), None) => format!("{} indexed", util::format_count(i.len() as u64)),
            (None, None) => "No index".into(),
        }
    }

    pub fn is_loading_backend(&self) -> bool {
        self.pending_backend.is_some()
    }

    fn save_settings_if_changed(&mut self) {
        if self.settings != self.settings_saved {
            match self.settings.save_to(&self.settings_path) {
                Ok(()) => {
                    log::debug!("settings saved to {}", self.settings_path.display());
                    self.settings_saved = self.settings.clone();
                }
                Err(e) => log::warn!(
                    "could not save settings to {}: {e}",
                    self.settings_path.display()
                ),
            }
        }
    }

    pub fn copy_diagnostics(&mut self, ctx: &egui::Context) {
        let mut d = Diagnostics::collect(200);
        d.extra.push((
            "Game folder".into(),
            self.settings
                .game_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "not set".into()),
        ));
        d.extra.push(("Index".into(), self.index_status()));
        d.extra.push((
            "Settings file".into(),
            self.settings_path.display().to_string(),
        ));
        d.extra.push((
            "Jobs".into(),
            format!("{} active", self.jobs.active_count()),
        ));
        ctx.copy_text(d.to_text());
        self.notify("Diagnostics copied to the clipboard");
    }
}

/// The eframe app.
pub struct StudioApp {
    dock: DockState<Tab>,
    studio: Studio,
    first_frame: bool,
}

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings) -> Self {
        style::apply_density(&cc.egui_ctx);
        style::apply_theme(&cc.egui_ctx, settings.theme);
        cc.egui_ctx.set_zoom_factor(settings.ui_scale);

        let gpu = if let Some(rs) = &cc.wgpu_render_state {
            GpuContext::Wgpu(rs.clone())
        } else if let Some(gl) = &cc.gl {
            GpuContext::Glow(Arc::clone(gl))
        } else {
            GpuContext::None
        };

        let dock = cc
            .storage
            .and_then(|s| eframe::get_value::<DockState<Tab>>(s, DOCK_KEY))
            .filter(|d| d.iter_all_tabs().count() > 0)
            .unwrap_or_else(ui::default_dock);

        let mut studio = Studio::new(settings, paths::settings_file(), gpu);
        let ctx = cc.egui_ctx.clone();
        studio.jobs.set_notifier(move || ctx.request_repaint());

        match studio.settings.game_dir.clone() {
            Some(dir) => studio.open_install(Some(dir)),
            None if studio.settings.demo_data_on_start => studio.open_install(None),
            None => {}
        }
        Self {
            dock,
            studio,
            first_frame: true,
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let find = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
        let goto = KeyboardShortcut::new(Modifiers::COMMAND, Key::G);
        let rescan = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
        let open = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
        let (f, g, r, o) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&find),
                i.consume_shortcut(&goto),
                i.consume_shortcut(&rescan),
                i.consume_shortcut(&open),
            )
        });
        if f {
            self.studio.search.request_focus = true;
            self.studio.focus_tab = Some(Tab::Search);
        }
        if g {
            self.studio.hex.request_goto_focus = true;
            self.studio.focus_tab = Some(Tab::Hex);
        }
        if r {
            self.studio.rescan();
        }
        if o {
            self.pick_game_folder();
        }
    }

    fn pick_game_folder(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Select your GTA VI install folder");
        if let Some(dir) = &self.studio.settings.game_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(dir) = dialog.pick_folder() {
            self.studio.settings.game_dir = Some(dir.clone());
            self.studio.open_install(Some(dir));
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            if crate::brand::wordmark(ui, 14.0)
                .on_hover_text("About ROME Studio")
                .interact(egui::Sense::click())
                .clicked()
            {
                self.studio.show_about = true;
            }
            ui.add_space(6.0);
            ui.menu_button("File", |ui| {
                if ui
                    .add(egui::Button::new("Open game folder...").shortcut_text(shortcut(ui, "O")))
                    .clicked()
                {
                    ui.close();
                    self.pick_game_folder();
                }
                if ui
                    .add(egui::Button::new("Rescan").shortcut_text(shortcut(ui, "R")))
                    .clicked()
                {
                    ui.close();
                    self.studio.rescan();
                }
                if ui.button("Load demo data").clicked() {
                    ui.close();
                    self.studio.open_install(None);
                }
                ui.separator();
                if ui.button("Settings...").clicked() {
                    ui.close();
                    self.studio.show_settings = true;
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.close();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                for tab in Tab::ALL {
                    let mut open = self.dock.find_tab(&tab).is_some();
                    if ui.checkbox(&mut open, tab.title()).clicked() {
                        ui::toggle_tab(&mut self.dock, tab);
                    }
                }
                ui.separator();
                if ui.button("Reset layout").clicked() {
                    ui.close();
                    self.dock = ui::default_dock();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Copy diagnostics").clicked() {
                    ui.close();
                    let ctx = ui.ctx().clone();
                    self.studio.copy_diagnostics(&ctx);
                }
                if ui.button("Open logs folder").clicked() {
                    ui.close();
                    open_in_file_manager(&paths::logs_dir());
                }
                if ui.button("Open crash reports folder").clicked() {
                    ui.close();
                    open_in_file_manager(&paths::crash_dir());
                }
                ui.separator();
                if ui.button("About ROME Studio").clicked() {
                    ui.close();
                    self.studio.show_about = true;
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let s = &self.studio;
            ui.label(&s.backend_status);
            ui.separator();
            ui.label(s.index_status());
            ui.separator();
            let snaps = s.jobs.snapshot();
            let running: Vec<_> = snaps.iter().filter(|j| !j.state.is_terminal()).collect();
            if running.is_empty() {
                ui.label(egui::RichText::new("Idle").weak());
            } else {
                let first = running[0];
                let text = if running.len() > 1 {
                    format!("{} (+{} more)", first.name, running.len() - 1)
                } else {
                    first.name.clone()
                };
                ui.label(util::truncate(&text, 60));
                let bar = match first.progress {
                    Some(p) => egui::ProgressBar::new(p).show_percentage(),
                    None => egui::ProgressBar::new(0.0).animate(true),
                };
                ui.add(bar.desired_width(120.0).desired_height(12.0));
            }
            if let Some((msg, at)) = &self.studio.notice {
                if at.elapsed() < Duration::from_secs(6) {
                    ui.separator();
                    ui.label(egui::RichText::new(msg).strong());
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                } else {
                    self.studio.notice = None;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = diagnostics::render_info();
                let text = match &r {
                    Some(r) => format!("{} / {} / {}", r.renderer, r.backend, r.adapter),
                    None => "renderer unknown".into(),
                };
                ui.label(egui::RichText::new(util::truncate(&text, 70)).weak())
                    .on_hover_text(
                        "Renderer / graphics API / adapter. Help > Copy diagnostics for details.",
                    );
            });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.studio.show_settings;
        let mut reopen: Option<Option<PathBuf>> = None;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                let s = &mut self.studio.settings;
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Game install folder");
                        ui.horizontal(|ui| {
                            let text = s
                                .game_dir
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "not set (demo data)".into());
                            ui.label(egui::RichText::new(util::truncate(&text, 48)).monospace())
                                .on_hover_text(&text);
                            if ui.button("Browse...").clicked()
                                && let Some(dir) = rfd::FileDialog::new()
                                    .set_title("Select your GTA VI install folder")
                                    .pick_folder()
                            {
                                s.game_dir = Some(dir.clone());
                                reopen = Some(Some(dir));
                            }
                            if s.game_dir.is_some() && ui.button("Clear").clicked() {
                                s.game_dir = None;
                            }
                        });
                        ui.end_row();

                        ui.label("Index database");
                        ui.horizontal(|ui| {
                            let path = s.index_db_path().display().to_string();
                            ui.label(egui::RichText::new(util::truncate(&path, 48)).monospace())
                                .on_hover_text(&path);
                            if ui.button("Change...").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .set_title("Index database location")
                                    .set_file_name("index.sqlite")
                                    .save_file()
                            {
                                s.index_db = Some(p);
                            }
                            if s.index_db.is_some() && ui.button("Default").clicked() {
                                s.index_db = None;
                            }
                        });
                        ui.end_row();

                        ui.label("Theme");
                        let before = s.theme;
                        egui::ComboBox::from_id_salt("theme")
                            .selected_text(s.theme.label())
                            .show_ui(ui, |ui| {
                                for t in Theme::ALL {
                                    ui.selectable_value(&mut s.theme, t, t.label());
                                }
                            });
                        if before != s.theme {
                            style::apply_theme(ui.ctx(), s.theme);
                        }
                        ui.end_row();

                        ui.label("UI scale");
                        if ui
                            .add(egui::Slider::new(&mut s.ui_scale, 0.75..=2.0).step_by(0.05))
                            .drag_stopped()
                        {
                            ui.ctx().set_zoom_factor(s.ui_scale);
                        }
                        ui.end_row();

                        ui.label("Renderer (next launch)");
                        egui::ComboBox::from_id_salt("renderer")
                            .selected_text(s.renderer.label())
                            .show_ui(ui, |ui| {
                                for r in crate::renderer::RendererPref::ALL {
                                    ui.selectable_value(&mut s.renderer, r, r.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Hex bytes per row");
                        ui.horizontal(|ui| {
                            for n in [8u32, 16, 32] {
                                ui.selectable_value(&mut s.hex_bytes_per_row, n, n.to_string());
                            }
                        });
                        ui.end_row();

                        ui.label("Demo data");
                        ui.checkbox(
                            &mut s.demo_data_on_start,
                            "Load synthetic data when no game folder is set",
                        );
                        ui.end_row();
                    });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!(
                        "Saved to {}. --renderer and {} override the renderer setting.",
                        self.studio.settings_path.display(),
                        crate::renderer::RENDERER_ENV
                    ))
                    .small()
                    .weak(),
                );
            });
        self.studio.show_settings = open;
        self.studio
            .hex
            .set_bytes_per_row(self.studio.settings.hex_bytes_per_row as usize);
        if let Some(dir) = reopen {
            self.studio.open_install(dir);
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("About ROME Studio")
            .open(&mut self.studio.show_about)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                crate::brand::wordmark(ui, 56.0);
                ui.add_space(4.0);
                ui.heading(format!("{} {}", crate::APP_NAME, crate::VERSION));
                ui.label("Asset viewer for your own GTA VI install (u_rome, unofficial ROME).");
                ui.label(
                    "Licensed MIT OR Apache-2.0. Not affiliated with Rockstar Games or Take-Two.",
                );
                ui.add_space(6.0);
                ui.label(format!("Data folder: {}", paths::data_dir().display()));
                if let Some(p) = crate::logging::log_file_path() {
                    ui.label(format!("Log file: {}", p.display()));
                }
            });
    }

    fn mock_banner(&self, ui: &mut egui::Ui) {
        if self.studio.backend.as_ref().is_some_and(|b| b.is_mock()) {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Demo data: no game folder is open, so Studio shows a synthetic install. \
                         Use File > Open game folder to scan your own copy.",
                    )
                    .color(ui.visuals().warn_fg_color)
                    .small(),
                );
            });
        }
    }
}

fn shortcut(ui: &egui::Ui, key: &str) -> String {
    let m = if ui.ctx().os() == egui::os::OperatingSystem::Mac {
        "Cmd"
    } else {
        "Ctrl"
    };
    format!("{m}+{key}")
}

/// Opens a folder in Explorer / Finder (creating it first).
pub fn open_in_file_manager(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(dir).spawn() {
        log::warn!("could not open {}: {e}", dir.display());
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.first_frame {
            self.first_frame = false;
            crate::run::mark_first_frame();
        }
        let ctx = ui.ctx().clone();
        self.studio.poll();
        self.shortcuts(&ctx);
        if let Some(tab) = self.studio.focus_tab.take() {
            ui::focus_tab(&mut self.dock, tab);
        }

        egui::Panel::top("menu_bar").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0))
            .show(ui, |ui| {
                self.mock_banner(ui);
                let style = egui_dock::Style::from_egui(ui.style());
                DockArea::new(&mut self.dock)
                    .style(style)
                    .show_leaf_collapse_buttons(false)
                    .show_inside(
                        ui,
                        &mut ui::Viewer {
                            studio: &mut self.studio,
                        },
                    );
            });

        self.settings_window(&ctx);
        self.about_window(&ctx);
        if !self.studio.show_settings {
            self.studio.save_settings_if_changed();
        }
        if self.studio.jobs.active_count() > 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, DOCK_KEY, &self.dock);
        self.studio.save_settings_if_changed();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        log::info!("shutting down");
        self.studio.save_settings_if_changed();
        self.studio.jobs.shutdown();
        crate::logging::flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_dir_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("x64/levels")).unwrap();
        std::fs::write(dir.path().join("common.rpf"), b"").unwrap();
        std::fs::write(dir.path().join("x64/levels/a.RPF"), b"").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"").unwrap();
        let c = check_game_dir(dir.path());
        assert!(c.exists);
        assert_eq!(c.rpf_files, 2);
        let missing = check_game_dir(&dir.path().join("nope"));
        assert!(!missing.exists);
        assert_eq!(missing.rpf_files, 0);
    }

    #[test]
    fn studio_loads_mock_and_selects() {
        let mut s = Studio::new(
            Settings::default(),
            std::env::temp_dir().join("u_rome_test_settings.json"),
            GpuContext::None,
        );
        s.open_install(None);
        let deadline = Instant::now() + Duration::from_secs(60);
        while (s.backend.is_none() || s.index.is_none()) && Instant::now() < deadline {
            s.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        let backend = s.backend.clone().expect("mock backend loaded");
        assert!(backend.is_mock());
        assert!(s.index.as_ref().unwrap().len() > 100_000);
        let root = backend.roots().unwrap()[0].clone();
        let child = backend.children(root.id).unwrap()[0].clone();
        s.select(child.id);
        while s.selection.is_loading() && Instant::now() < deadline {
            s.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(s.selection.source.is_some());
        assert_eq!(
            s.selection
                .details
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .chain
                .len(),
            2
        );
        s.jobs.shutdown();
    }
}
