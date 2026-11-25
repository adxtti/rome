//! ROME Studio: the desktop asset viewer shell.
//!
//! The studio is written against the [`backend::Backend`] trait so the whole
//! UI works before the decoder crates land. [`backend::mock::MockBackend`]
//! generates a synthetic install; an integrator adds the real RPF9 backend by
//! implementing [`backend::BackendProvider`] and registering it in
//! [`app::StudioApp::new`] (see `providers()` in `app.rs`).
//!
//! Layout:
//! - [`run`]: process entry (logging, panic hook, renderer selection with fallback).
//! - [`backend`]: data access trait, byte sources, mock install.
//! - [`jobs`]: background worker pool with progress and cancellation.
//! - [`search`], [`hex`]: pure, unit-tested logic behind the Search and Hex panels.
//! - [`preview`]: the `Previewer` registry decoders plug into.
//! - [`app`] and [`ui`]: the egui application.

// pub mod app;
pub mod backend;
pub mod crash;
pub mod diagnostics;
// pub mod hex;
pub mod jobs;
pub mod logging;
pub mod paths;
// pub mod preview;
// pub mod renderer;
// pub mod run;
// pub mod search;
// pub mod settings;
// pub mod ui;
pub mod util;

// pub use run::run;

/// Application name used for window titles and storage keys.
pub const APP_NAME: &str = "ROME Studio";

/// Crate version baked in at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
