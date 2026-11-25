//! Process logger: stderr (via `env_logger`), a per-session file and an
//! in-memory ring buffer shown by the Log panel and copied into diagnostics.
//!
//! Filtering follows `RUST_LOG` when set; otherwise `info` globally and
//! `debug` for `u_rome*` crates, with noisy graphics crates held at `warn`.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::util::UtcTime;

/// Lines kept in memory for the Log panel.
pub const BUFFER_CAPACITY: usize = 20_000;
/// Session log files kept on disk; older ones are deleted at startup.
pub const KEEP_LOG_FILES: usize = 20;

/// One captured log record.
#[derive(Clone, Debug)]
pub struct LogLine {
    pub seq: u64,
    pub time: String,
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogLine {
    pub fn to_text(&self) -> String {
        format!(
            "{} {:<5} {}: {}",
            self.time, self.level, self.target, self.message
        )
    }
}

/// Bounded, thread-safe ring buffer of recent log lines.
pub struct LogBuffer {
    lines: Mutex<VecDeque<LogLine>>,
    capacity: usize,
    next_seq: AtomicU64,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(4096))),
            capacity: capacity.max(1),
            next_seq: AtomicU64::new(0),
        }
    }

    pub fn push(&self, level: Level, target: &str, message: String) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let line = LogLine {
            seq,
            time: UtcTime::now().clock(),
            level,
            target: target.to_owned(),
            message,
        };
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Total lines ever pushed; changes whenever a line arrives.
    pub fn generation(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed)
    }

    /// Copies all buffered lines.
    pub fn snapshot(&self) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines.iter().cloned().collect()
    }

    /// The last `n` lines as text.
    pub fn tail_text(&self, n: usize) -> Vec<String> {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let skip = lines.len().saturating_sub(n);
        lines.iter().skip(skip).map(LogLine::to_text).collect()
    }

    pub fn clear(&self) {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn len(&self) -> usize {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static BUFFER: OnceLock<LogBuffer> = OnceLock::new();
static LOGGER: OnceLock<StudioLogger> = OnceLock::new();
static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// The global in-memory log buffer (exists even before [`init`]).
pub fn buffer() -> &'static LogBuffer {
    BUFFER.get_or_init(|| LogBuffer::new(BUFFER_CAPACITY))
}

/// Path of this session's log file, if one could be created.
pub fn log_file_path() -> Option<&'static Path> {
    LOG_FILE.get().map(PathBuf::as_path)
}

struct StudioLogger {
    stderr: env_logger::Logger,
    file: Mutex<Option<BufWriter<File>>>,
}

impl Log for StudioLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.stderr.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.stderr.matches(record) {
            return;
        }
        self.stderr.log(record);
        let message = record.args().to_string();
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = writeln!(
                file,
                "{} {:<5} {}: {}",
                UtcTime::now().clock(),
                record.level(),
                record.target(),
                message
            );
            // Warnings and errors are flushed immediately so a crash or a
            // killed VM still leaves them on disk.
            if record.level() <= Level::Warn {
                let _ = file.flush();
            }
        }
        buffer().push(record.level(), record.target(), message);
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = file.flush();
        }
    }
}

fn build_filter() -> env_logger::Logger {
    let mut builder = env_logger::Builder::new();
    builder
        .filter_level(LevelFilter::Info)
        .filter_module("u_rome", LevelFilter::Debug)
        .filter_module("u_rome_studio", LevelFilter::Debug)
        .filter_module("wgpu_core", LevelFilter::Warn)
        .filter_module("wgpu_hal", LevelFilter::Warn)
        .filter_module("naga", LevelFilter::Warn)
        .filter_module("egui_wgpu", LevelFilter::Info)
        .filter_module("eframe", LevelFilter::Info);
    if let Ok(spec) = std::env::var("RUST_LOG")
        && !spec.trim().is_empty()
    {
        builder.parse_filters(&spec);
    }
    builder.build()
}

/// Installs the global logger. Safe to call more than once (later calls are
/// no-ops). Returns the log file path when a file could be opened; failure to
/// open the file is reported to stderr and the in-app buffer but is not fatal.
pub fn init(log_dir: &Path) -> Option<PathBuf> {
    let (file, path, file_error) = match open_session_file(log_dir) {
        Ok((f, p)) => (Some(BufWriter::new(f)), Some(p), None),
        Err(e) => (None, None, Some(e)),
    };
    let logger = StudioLogger {
        stderr: build_filter(),
        file: Mutex::new(file),
    };
    let max = logger.stderr.filter();
    if LOGGER.set(logger).is_err() {
        return log_file_path().map(Path::to_path_buf);
    }
    if let Some(logger) = LOGGER.get()
        && log::set_logger(logger).is_ok()
    {
        log::set_max_level(max);
    }
    if let Some(p) = &path {
        let _ = LOG_FILE.set(p.clone());
    }
    if let Some(e) = file_error {
        log::warn!(
            "could not create log file in {}: {e}; logging to memory only",
            log_dir.display()
        );
    }
    path
}

/// Flushes the log file (called by the panic hook and on exit).
pub fn flush() {
    if let Some(l) = LOGGER.get() {
        l.flush();
    }
}

fn open_session_file(dir: &Path) -> std::io::Result<(File, PathBuf)> {
    fs::create_dir_all(dir)?;
    prune_old_logs(dir, KEEP_LOG_FILES.saturating_sub(1));
    let stamp = UtcTime::now().file_stamp();
    let pid = std::process::id();
    let path = dir.join(format!("studio-{stamp}-{pid}.log"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok((file, path))
}

/// Deletes the oldest `studio-*.log` files so at most `keep` remain.
pub fn prune_old_logs(dir: &Path, keep: usize) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut logs: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("studio-") && n.ends_with(".log"))
        })
        .collect();
    // Names embed a sortable UTC stamp, so lexical order is chronological.
    logs.sort();
    let excess = logs.len().saturating_sub(keep);
    for old in logs.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_is_bounded() {
        let b = LogBuffer::new(3);
        for i in 0..5 {
            b.push(Level::Info, "t", format!("m{i}"));
        }
        let snap = b.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "m2");
        assert_eq!(snap[2].seq, 4);
        assert_eq!(b.generation(), 5);
        let tail = b.tail_text(2);
        assert_eq!(tail.len(), 2);
        assert!(tail[1].ends_with("t: m4"));
        b.clear();
        assert!(b.is_empty());
    }

    #[test]
    fn prunes_old_logs() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            fs::write(
                dir.path().join(format!("studio-2026010{i}-000000-1.log")),
                b"x",
            )
            .unwrap();
        }
        fs::write(dir.path().join("other.txt"), b"x").unwrap();
        prune_old_logs(dir.path(), 2);
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "other.txt",
                "studio-20260103-000000-1.log",
                "studio-20260104-000000-1.log"
            ]
        );
    }
}
