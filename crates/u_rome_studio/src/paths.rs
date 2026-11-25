//! Platform data locations.
//!
//! Everything Studio writes lives under one folder so users can find it:
//! - Windows: `%LOCALAPPDATA%\u_rome`
//! - macOS: `~/Library/Application Support/u_rome`
//! - Linux: `$XDG_DATA_HOME/u_rome`
//!
//! `U_ROME_DATA_DIR` overrides the root (portable installs, tests).

use std::path::PathBuf;

/// Environment variable that overrides [`data_dir`].
pub const DATA_DIR_ENV: &str = "U_ROME_DATA_DIR";

/// Root data folder. Shared with the CLI through `u_rome_install`, which
/// also honours [`DATA_DIR_ENV`].
pub fn data_dir() -> PathBuf {
    u_rome_install::data_dir()
}

/// Session log files.
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Crash reports written by the panic hook.
pub fn crash_dir() -> PathBuf {
    data_dir().join("crashes")
}

/// Studio settings (JSON, human-editable).
pub fn settings_file() -> PathBuf {
    data_dir().join("studio_settings.json")
}

/// eframe persistence folder (window geometry, dock layout).
pub fn state_dir() -> PathBuf {
    data_dir().join("studio_state")
}

/// Default location of the asset index database. Matches
/// `u_rome_install::default_index_path()` so Studio and the CLI share one index.
pub fn default_index_db() -> PathBuf {
    data_dir().join("index.sqlite")
}
