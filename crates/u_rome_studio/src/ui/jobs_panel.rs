//! Jobs tab: background tasks with progress and cancel buttons.

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::app::Studio;
use crate::jobs::JobState;

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let jobs = studio.jobs.clone();
    let snaps = jobs.snapshot();
    ui.horizontal(|ui| {
        let active = snaps.iter().filter(|j| !j.state.is_terminal()).count();
        ui.label(format!("{active} active, {} total", snaps.len()));
        if ui.button("Clear finished").clicked() {
            jobs.clear_finished();
        }
        if ui
            .add_enabled(active > 0, egui::Button::new("Cancel all"))
            .clicked()
        {
            jobs.cancel_all();
        }
    });
    ui.separator();
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink([false, false])
        .column(Column::initial(240.0).at_least(80.0).clip(true))
        .column(Column::initial(70.0))
        .column(Column::initial(160.0))
        .column(Column::initial(60.0))
        .column(Column::remainder().clip(true))
        .column(Column::exact(60.0))
        .header(row_h, |mut h| {
            for t in ["Task", "State", "Progress", "Time", "Status", ""] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            // Newest first.
            let rows: Vec<_> = snaps.iter().rev().collect();
            body.rows(row_h, rows.len(), |mut row| {
                let Some(j) = rows.get(row.index()) else {
                    return;
                };
                row.col(|ui| {
                    ui.label(&j.name);
                });
                row.col(|ui| {
                    let c = match &j.state {
                        JobState::Failed(_) => ui.visuals().error_fg_color,
                        JobState::Cancelled => ui.visuals().warn_fg_color,
                        JobState::Finished => ui.visuals().weak_text_color(),
                        _ => ui.visuals().text_color(),
                    };
                    ui.colored_label(c, j.state.label());
                });
                row.col(|ui| {
                    let bar = match (j.progress, &j.state) {
                        (Some(p), _) => egui::ProgressBar::new(p).show_percentage(),
                        (None, JobState::Running) => egui::ProgressBar::new(0.0).animate(true),
                        (None, _) => egui::ProgressBar::new(0.0),
                    };
                    ui.add(bar.desired_height(12.0));
                });
                row.col(|ui| {
                    ui.label(format!("{:.1}s", j.elapsed.as_secs_f32()));
                });
                row.col(|ui| {
                    let text = match &j.state {
                        JobState::Failed(e) => e.clone(),
                        _ => j.status.clone(),
                    };
                    ui.label(text);
                });
                row.col(|ui| {
                    if !j.state.is_terminal() && ui.small_button("Cancel").clicked() {
                        jobs.cancel(j.id);
                    }
                });
            });
        });
}
