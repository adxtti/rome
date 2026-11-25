//! Inspector: facts about the selected entry.

use eframe::egui;

use crate::app::Studio;
use crate::backend::EntryInfo;
use crate::util::{format_count, format_size};

fn row(ui: &mut egui::Ui, k: &str, v: impl Into<egui::WidgetText>) {
    ui.label(egui::RichText::new(k).weak());
    ui.label(v);
    ui.end_row();
}

fn mono(s: impl Into<String>) -> egui::RichText {
    egui::RichText::new(s.into()).monospace()
}

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let Some(id) = studio.selection.id else {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Select an entry in the Explorer or Search.").weak());
        return;
    };
    let Some(e) = studio.selection.info.clone() else {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Entry {id:#x} could not be read."),
        );
        return;
    };
    let mut select = None;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(super::kind_label(ui, e.kind));
        ui.heading(e.display_name());
    });
    if e.provisional {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "Provisional: part of this entry's metadata relies on uncertain format facts.",
        );
    }
    ui.add_space(4.0);
    egui::Grid::new("inspector_grid")
        .num_columns(2)
        .striped(true)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            row(
                ui,
                "Name",
                e.name.clone().unwrap_or_else(|| "(unknown)".into()),
            );
            row(
                ui,
                "Name hash",
                mono(
                    e.name_hash
                        .map(|h| h.to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
            );
            row(ui, "Kind", format!("{} ({:?})", e.kind.label(), e.kind));
            row(ui, "Codec", e.codec.to_string());
            row(
                ui,
                "Size",
                format!("{} ({} bytes)", format_size(e.size), format_count(e.size)),
            );
            row(
                ui,
                "Stored size",
                format!(
                    "{} ({} bytes)",
                    format_size(e.stored_size),
                    format_count(e.stored_size)
                ),
            );
            row(ui, "Offset in parent", mono(format!("{:#x}", e.offset)));
            if let Some(n) = e.child_count {
                row(ui, "Children", format_count(u64::from(n)));
            }
            row(ui, "Entry id", mono(format!("{:#x}", e.id)));
            let prov = if e.provisional { "yes" } else { "no" };
            row(ui, "Provisional", prov);
            let data = if studio.selection.is_loading() {
                "loading...".to_owned()
            } else if let Some(err) = &studio.selection.source_error {
                format!("error: {err}")
            } else if let Some(s) = &studio.selection.source {
                format!("{} decoded", format_size(s.len()))
            } else {
                "-".into()
            };
            row(ui, "Data", data);
        });

    match &studio.selection.details {
        Some(Ok(d)) => {
            ui.add_space(8.0);
            ui.strong("Location");
            for (depth, link) in d.chain.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space(depth as f32 * 12.0);
                    ui.label(super::kind_label(ui, link.kind));
                    let label = location_label(link);
                    if link.id == e.id {
                        ui.label(egui::RichText::new(label).strong());
                    } else if ui.link(label).clicked() {
                        select = Some(link.id);
                    }
                });
            }
            if ui.small_button("Copy path").clicked() {
                let path: Vec<String> = d.chain.iter().map(EntryInfo::display_name).collect();
                ui.ctx().copy_text(path.join("/"));
            }
            if !d.header.is_empty() {
                ui.add_space(8.0);
                ui.strong("Header fields");
                egui::Grid::new("inspector_header")
                    .num_columns(3)
                    .striped(true)
                    .spacing([12.0, 3.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Offset").weak());
                        ui.label(egui::RichText::new("Field").weak());
                        ui.label(egui::RichText::new("Value").weak());
                        ui.end_row();
                        for f in &d.header {
                            ui.monospace(f.offset.map(|o| format!("{o:#06x}")).unwrap_or_default());
                            let mut name = egui::RichText::new(&f.name);
                            if f.provisional {
                                name = name.color(ui.visuals().warn_fg_color);
                            }
                            ui.label(name).on_hover_text(if f.provisional {
                                "Provisional: field meaning is not confirmed"
                            } else {
                                "Confirmed field"
                            });
                            ui.label(mono(&f.value));
                            ui.end_row();
                        }
                    });
            }
            if !d.notes.is_empty() {
                ui.add_space(8.0);
                ui.strong("Notes");
                for n in &d.notes {
                    ui.label(egui::RichText::new(n).weak());
                }
            }
        }
        Some(Err(err)) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Details failed: {err}"),
            );
        }
        None => {}
    }
    if let Some(id) = select {
        studio.reveal(id);
    }
}

fn location_label(e: &EntryInfo) -> String {
    if e.parent.is_some() {
        format!("{}  @{:#x}", e.display_name(), e.offset)
    } else {
        e.display_name()
    }
}
