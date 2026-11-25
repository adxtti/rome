//! Panic hook that writes a crash report next to the logs.

use std::backtrace::Backtrace;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::diagnostics::Diagnostics;
use crate::util::UtcTime;

static LAST_CRASH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Path of the most recent crash report written by this process.
pub fn last_crash_report() -> Option<PathBuf> {
    LAST_CRASH.lock().ok().and_then(|g| g.clone())
}

/// Installs the hook. The previous hook (stderr message) still runs.
pub fn install(crash_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_owned();
        let payload = panic_message(info.payload());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let backtrace = Backtrace::force_capture();
        log::error!("panic on thread '{thread_name}' at {location}: {payload}");
        crate::logging::flush();
        let report = build_report(&thread_name, &location, &payload, &backtrace.to_string());
        match write_report(&crash_dir, &report) {
            Ok(path) => {
                log::error!("crash report written to {}", path.display());
                if let Ok(mut g) = LAST_CRASH.lock() {
                    *g = Some(path);
                }
            }
            Err(e) => log::error!(
                "could not write crash report to {}: {e}",
                crash_dir.display()
            ),
        }
        crate::logging::flush();
        previous(info);
    }));
}

/// Extracts the panic message from a payload.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// Formats a crash report (pure; used by the hook and tests).
pub fn build_report(thread: &str, location: &str, message: &str, backtrace: &str) -> String {
    let diag = Diagnostics::collect(200);
    format!(
        "{} crash report\nTime: {}\nThread: {thread}\nLocation: {location}\nMessage: {message}\n\n\
         --- diagnostics ---\n{}\n--- backtrace ---\n{backtrace}\n",
        crate::APP_NAME,
        UtcTime::now().iso(),
        diag.to_text(),
    )
}

fn write_report(dir: &Path, report: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        "crash-{}-{}.txt",
        UtcTime::now().file_stamp(),
        std::process::id()
    ));
    fs::write(&path, report)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_sections() {
        let r = build_report("main", "src/x.rs:1:2", "boom", "frame 0");
        assert!(r.contains("Thread: main"));
        assert!(r.contains("Message: boom"));
        assert!(r.contains("--- diagnostics ---"));
        assert!(r.contains("--- backtrace ---\nframe 0"));
    }

    #[test]
    fn writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_report(dir.path(), "x").unwrap();
        assert_eq!(fs::read_to_string(p).unwrap(), "x");
    }

    #[test]
    fn payloads() {
        let a: Box<dyn std::any::Any + Send> = Box::new("s");
        let b: Box<dyn std::any::Any + Send> = Box::new(String::from("t"));
        let c: Box<dyn std::any::Any + Send> = Box::new(5u8);
        assert_eq!(panic_message(a.as_ref()), "s");
        assert_eq!(panic_message(b.as_ref()), "t");
        assert!(panic_message(c.as_ref()).contains("non-string"));
    }
}
