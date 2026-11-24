//! ROME Studio entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ROME Studio")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ROME Studio",
        options,
        Box::new(|_cc| Ok(Box::<StudioApp>::default())),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Default)]
struct StudioApp;

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("ROME Studio");
            ui.label("Scaffold build. Panels arrive in phase 1.");
        });
    }
}
