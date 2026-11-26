//! Process entry: logging, panic hook, settings, renderer attempts with
//! fallback, and a native message box when nothing works.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

use crate::app::StudioApp;
use crate::diagnostics::{self, Diagnostics};
use crate::renderer::{self, RendererKind};
use crate::settings::Settings;
use crate::util::UtcTime;
use crate::{APP_NAME, VERSION, crash, logging, paths};

static FIRST_FRAME: AtomicBool = AtomicBool::new(false);

/// Called by the app on its first frame: after this, a renderer error is a
/// runtime failure, not a startup failure, and no fallback is attempted.
pub fn mark_first_frame() {
    if !FIRST_FRAME.swap(true, Ordering::SeqCst) {
        log::info!("first frame rendered");
    }
}

const USAGE: &str = "ROME Studio

Usage: u_rome_studio [--renderer auto|wgpu|glow]

Options:
  --renderer <r>   auto (wgpu, then OpenGL), wgpu or glow
  --version        print the version
  --help           print this help

Environment:
  U_ROME_RENDERER  same as --renderer
  U_ROME_DATA_DIR  override the data folder (logs, settings, crash reports)
  WGPU_BACKEND     restrict wgpu backends (dx12, vulkan, gl, metal)
  RUST_LOG         log filter (default: info, debug for u_rome crates)
";

fn native_options(kind: RendererKind) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{APP_NAME} {VERSION}"))
            .with_app_id("u_rome_studio")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([720.0, 420.0])
            .with_icon(crate::brand::window_icon().unwrap_or_default()),
        renderer: kind.to_eframe(),
        wgpu_options: renderer::wgpu_configuration(),
        persistence_path: Some(paths::state_dir()),
        ..Default::default()
    }
}

/// Runs Studio. Returns the process exit code.
pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{APP_NAME} {VERSION}");
        return ExitCode::SUCCESS;
    }

    let log_file = logging::init(&paths::logs_dir());
    crash::install(paths::crash_dir());
    log::info!(
        "{APP_NAME} {VERSION} starting ({}, {} build)",
        diagnostics::os_description(),
        std::env::consts::ARCH
    );
    log::info!("data dir: {}", paths::data_dir().display());
    if let Some(p) = &log_file {
        log::info!("log file: {}", p.display());
    }

    let settings = Settings::load_from(&paths::settings_file());
    let cli = renderer::cli_renderer(&args);
    let env = std::env::var(renderer::RENDERER_ENV).ok();
    let (pref, source, warnings) =
        renderer::resolve(cli.as_deref(), env.as_deref(), settings.renderer);
    for w in warnings {
        log::warn!("{w}");
    }
    log::info!("renderer preference: {pref:?} (from {source})");

    let mut failures: Vec<String> = Vec::new();
    for kind in pref.plan() {
        log::info!("starting with the {kind} renderer");
        FIRST_FRAME.store(false, Ordering::SeqCst);
        let requested = format!("{pref:?} (from {source})");
        let failed_so_far = failures.clone();
        let settings_for_app = settings.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            eframe::run_native(
                APP_NAME,
                native_options(kind),
                Box::new(move |cc| {
                    let mut info = renderer::render_info_from(cc);
                    info.requested = requested;
                    info.failed_attempts = failed_so_far;
                    log::info!(
                        "renderer ready: {} / {} / {} ({}, driver {})",
                        info.renderer,
                        info.backend,
                        info.adapter,
                        info.device_type,
                        info.driver
                    );
                    diagnostics::set_render_info(info);
                    Ok(Box::new(StudioApp::new(cc, settings_for_app)))
                }),
            )
        }));
        let err = match result {
            Ok(Ok(())) => {
                log::info!("exited normally");
                logging::flush();
                return ExitCode::SUCCESS;
            }
            Ok(Err(e)) => format!("{kind}: {e}"),
            Err(p) => format!("{kind}: panicked: {}", crash::panic_message(p.as_ref())),
        };
        log::error!("renderer attempt failed: {err}");
        diagnostics::push_failed_attempt(err.clone());
        failures.push(err);
        if FIRST_FRAME.load(Ordering::SeqCst) {
            // The app was running; do not reopen it with another renderer.
            break;
        }
    }

    let ran = FIRST_FRAME.load(Ordering::SeqCst);
    let report = write_startup_failure(&failures);
    let mut text = if ran {
        format!("{APP_NAME} stopped because of a graphics error.\n\n")
    } else {
        format!("{APP_NAME} could not start a graphics renderer.\n\n")
    };
    for f in &failures {
        text.push_str(f);
        text.push('\n');
    }
    text.push_str(
        "\nTry starting with \"--renderer glow\" or set U_ROME_RENDERER=glow. \
         In a virtual machine, enable 3D acceleration and update the guest tools.\n",
    );
    if let Some(p) = &report {
        text.push_str(&format!("\nDetails were written to:\n{}\n", p.display()));
    }
    if let Some(p) = logging::log_file_path() {
        text.push_str(&format!("Log file:\n{}\n", p.display()));
    }
    logging::flush();
    show_fatal(&text);
    ExitCode::FAILURE
}

fn write_startup_failure(failures: &[String]) -> Option<std::path::PathBuf> {
    let dir = paths::crash_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!(
        "renderer-failure-{}.txt",
        UtcTime::now().file_stamp()
    ));
    let mut s = format!("{APP_NAME} renderer failure\n\n");
    for f in failures {
        s.push_str(&format!("Attempt: {f}\n"));
    }
    s.push('\n');
    s.push_str(&Diagnostics::collect(300).to_text());
    match std::fs::write(&path, s) {
        Ok(()) => Some(path),
        Err(e) => {
            log::error!("could not write {}: {e}", path.display());
            None
        }
    }
}

fn show_fatal(text: &str) {
    eprintln!("{text}");
    let shown = catch_unwind(AssertUnwindSafe(|| {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title(format!("{APP_NAME} could not start"))
            .set_description(text)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }));
    if shown.is_err() {
        log::error!("native message box failed; see the log file");
    }
}
