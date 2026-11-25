//! User settings, stored as JSON in the data dir so they can be read before
//! the window (and renderer) exists and edited by hand if needed.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::renderer::RendererPref;

/// Current schema version; bump and migrate in [`Settings::load_from`].
pub const SETTINGS_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::Dark, Theme::Light, Theme::System];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::System => "Follow system",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// The user's own GTA VI install folder.
    pub game_dir: Option<PathBuf>,
    /// Index database location; `None` means [`crate::paths::default_index_db`].
    pub index_db: Option<PathBuf>,
    pub theme: Theme,
    /// UI zoom factor.
    pub ui_scale: f32,
    /// Renderer used at next launch (CLI flag and env var override it).
    pub renderer: RendererPref,
    /// Hex view bytes per row (8, 16 or 32).
    pub hex_bytes_per_row: u32,
    /// Generate a synthetic install when no game folder is set.
    pub demo_data_on_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            game_dir: None,
            index_db: None,
            theme: Theme::Dark,
            ui_scale: 1.0,
            renderer: RendererPref::Auto,
            hex_bytes_per_row: 16,
            demo_data_on_start: true,
        }
    }
}

impl Settings {
    /// Effective index database path.
    pub fn index_db_path(&self) -> PathBuf {
        self.index_db
            .clone()
            .unwrap_or_else(crate::paths::default_index_db)
    }

    /// Clamps values a hand-edited file might have broken.
    pub fn sanitize(&mut self) {
        if !self.ui_scale.is_finite() {
            self.ui_scale = 1.0;
        }
        self.ui_scale = self.ui_scale.clamp(0.5, 3.0);
        if ![8, 16, 32].contains(&self.hex_bytes_per_row) {
            self.hex_bytes_per_row = 16;
        }
        self.version = SETTINGS_VERSION;
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Parses JSON; unknown fields are ignored and missing ones defaulted.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let mut v: Self = serde_json::from_str(s)?;
        v.sanitize();
        Ok(v)
    }

    /// Loads from `path`. A missing file gives defaults; a corrupt one is
    /// backed up next to it and defaults are used, with a warning.
    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => match Self::from_json(&text) {
                Ok(s) => {
                    log::info!("settings loaded from {}", path.display());
                    s
                }
                Err(e) => {
                    let backup = path.with_extension("json.bad");
                    let _ = fs::copy(path, &backup);
                    log::warn!(
                        "settings file {} is invalid ({e}); using defaults, copy kept at {}",
                        path.display(),
                        backup.display()
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!(
                    "cannot read settings {}: {e}; using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Writes atomically (temp file + rename).
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.to_json())?;
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Settings {
            game_dir: Some(PathBuf::from("C:/Games/GTA VI")),
            theme: Theme::Light,
            renderer: RendererPref::Glow,
            ui_scale: 1.25,
            ..Default::default()
        };
        let back = Settings::from_json(&s.to_json()).unwrap();
        assert_eq!(s, back);
        assert!(s.to_json().contains("\"renderer\": \"glow\""));
    }

    #[test]
    fn tolerant_parsing() {
        let s = Settings::from_json(r#"{"theme":"system","future_field":42}"#).unwrap();
        assert_eq!(s.theme, Theme::System);
        assert_eq!(s.hex_bytes_per_row, 16);
        let s = Settings::from_json(r#"{"ui_scale": 99.0, "hex_bytes_per_row": 7}"#).unwrap();
        assert_eq!(s.ui_scale, 3.0);
        assert_eq!(s.hex_bytes_per_row, 16);
        assert!(Settings::from_json("not json").is_err());
    }

    #[test]
    fn file_io() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("studio_settings.json");
        assert_eq!(Settings::load_from(&p), Settings::default());
        let s = Settings {
            hex_bytes_per_row: 32,
            ..Default::default()
        };
        s.save_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p), s);
        fs::write(&p, "{broken").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        assert!(p.with_extension("json.bad").exists());
    }

    #[test]
    fn index_db_default() {
        let s = Settings::default();
        assert!(s.index_db_path().ends_with("index.sqlite"));
        let s = Settings {
            index_db: Some(PathBuf::from("/x/y.db")),
            ..Default::default()
        };
        assert_eq!(s.index_db_path(), PathBuf::from("/x/y.db"));
    }
}
