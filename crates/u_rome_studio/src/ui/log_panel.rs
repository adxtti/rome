//! Log tab: the in-memory log buffer with level and text filters.

use eframe::egui;
use log::Level;

use crate::app::Studio;
use crate::logging::{self, LogLine};

pub struct LogViewState {
    min_level: Level,
    filter: String,
    follow: bool,
    lines: Vec<LogLine>,
    visible: Vec<usize>,
    seen: (u64, Level, String),
}

impl Default for LogViewState {
    fn default() -> Self {
        Self {
            min_level: Level::Debug,
            filter: String::new(),
            follow: true,
            lines: Vec::new(),
            visible: Vec::new(),
            seen: (u64::MAX, Level::Trace, String::new()),
        }
    }
}

/// Indices of lines at or above `min` whose text contains `filter` (case-insensitive).
pub fn filter_lines(lines: &[LogLine], min: Level, filter: &str) -> Vec<usize> {
    let f = filter.to_lowercase();
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.level <= min)
        .filter(|(_, l)| {
            f.is_empty()
                || l.message.to_lowercase().contains(&f)
                || l.target.to_lowercase().contains(&f)
        })
        .map(|(i, _)| i)
        .collect()
}

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let st = &mut studio.log_view;
    let buf = logging::buffer();
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("log_level")
            .width(80.0)
            .selected_text(st.min_level.as_str())
            .show_ui(ui, |ui| {
                for l in [
                    Level::Error,
                    Level::Warn,
                    Level::Info,
                    Level::Debug,
                    Level::Trace,
                ] {
                    ui.selectable_value(&mut st.min_level, l, l.as_str());
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut st.filter)
                .hint_text("Filter")
                .desired_width(180.0),
        );
        ui.checkbox(&mut st.follow, "Follow");
        if ui.button("Copy visible").clicked() {
            let text: Vec<String> = st
                .visible
                .iter()
                .filter_map(|&i| st.lines.get(i))
                .map(LogLine::to_text)
                .collect();
            ui.ctx().copy_text(text.join("\n"));
        }
        if ui.button("Clear").clicked() {
            buf.clear();
            st.seen.0 = u64::MAX;
        }
        if ui.button("Open folder").clicked() {
            crate::app::open_in_file_manager(&crate::paths::logs_dir());
        }
    });
    let key = (buf.generation(), st.min_level, st.filter.clone());
    if key != st.seen {
        st.lines = buf.snapshot();
        st.visible = filter_lines(&st.lines, st.min_level, &st.filter);
        st.seen = key;
    }
    ui.separator();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
    let vis = ui.visuals().clone();
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .stick_to_bottom(st.follow)
        .show_rows(ui, row_h, st.visible.len(), |ui, range| {
            for i in range {
                let Some(l) = st.visible.get(i).and_then(|&j| st.lines.get(j)) else {
                    continue;
                };
                let color = match l.level {
                    Level::Error => vis.error_fg_color,
                    Level::Warn => vis.warn_fg_color,
                    Level::Info => vis.text_color(),
                    _ => vis.weak_text_color(),
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(l.to_text()).monospace().color(color))
                        .extend(),
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(level: Level, msg: &str) -> LogLine {
        LogLine {
            seq: 0,
            time: String::new(),
            level,
            target: "u_rome_studio::x".into(),
            message: msg.into(),
        }
    }

    #[test]
    fn filters() {
        let lines = vec![
            line(Level::Error, "bad header"),
            line(Level::Debug, "detail"),
            line(Level::Info, "Header ok"),
        ];
        assert_eq!(filter_lines(&lines, Level::Info, ""), vec![0, 2]);
        assert_eq!(filter_lines(&lines, Level::Trace, "header"), vec![0, 2]);
        assert_eq!(filter_lines(&lines, Level::Error, "header"), vec![0]);
        assert_eq!(filter_lines(&lines, Level::Trace, "studio::x").len(), 3);
    }
}
