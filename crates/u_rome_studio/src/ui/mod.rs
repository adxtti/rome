//! Panels and dock plumbing.

pub mod explorer;
pub mod hex_view;
pub mod inspector;
pub mod jobs_panel;
pub mod log_panel;
pub mod preview_panel;
pub mod search_panel;
pub mod style;

use eframe::egui;
use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};
use u_rome_core::AssetKind;

use crate::app::Studio;

/// Dockable panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tab {
    Explorer,
    Search,
    Inspector,
    Hex,
    Preview,
    Log,
    Jobs,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::Explorer,
        Tab::Search,
        Tab::Inspector,
        Tab::Hex,
        Tab::Preview,
        Tab::Log,
        Tab::Jobs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Explorer => "Explorer",
            Tab::Search => "Search",
            Tab::Inspector => "Inspector",
            Tab::Hex => "Hex",
            Tab::Preview => "Preview",
            Tab::Log => "Log",
            Tab::Jobs => "Jobs",
        }
    }
}

/// Default layout: navigation left, content centre, details right, output bottom.
pub fn default_dock() -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Preview, Tab::Hex]);
    let surface = dock.main_surface_mut();
    let [centre, _left] =
        surface.split_left(NodeIndex::root(), 0.27, vec![Tab::Explorer, Tab::Search]);
    let [centre, _right] = surface.split_right(centre, 0.72, vec![Tab::Inspector]);
    let [_centre, _bottom] = surface.split_below(centre, 0.72, vec![Tab::Log, Tab::Jobs]);
    dock
}

/// Makes `tab` visible (adding it if it was closed) and focuses it.
pub fn focus_tab(dock: &mut DockState<Tab>, tab: Tab) {
    if dock.find_tab(&tab).is_none() {
        dock.push_to_first_leaf(tab);
    }
    if let Some(path) = dock.find_tab(&tab) {
        let _ = dock.set_active_tab(path);
        dock.set_focused_node_and_surface(path.node_path());
    }
}

/// Toggles a tab's presence in the dock.
pub fn toggle_tab(dock: &mut DockState<Tab>, tab: Tab) {
    match dock.find_tab(&tab) {
        Some(path) => {
            dock.remove_tab(path);
        }
        None => focus_tab(dock, tab),
    }
}

/// Colour for a kind tag.
pub fn kind_color(kind: AssetKind, dark: bool) -> egui::Color32 {
    use egui::Color32 as C;
    let (d, l) = match kind {
        AssetKind::Archive => (C::from_rgb(230, 190, 90), C::from_rgb(150, 105, 0)),
        AssetKind::TextureStore | AssetKind::TextureTable => {
            (C::from_rgb(110, 190, 240), C::from_rgb(20, 100, 170))
        }
        AssetKind::Pso | AssetKind::MetaResource => {
            (C::from_rgb(180, 150, 240), C::from_rgb(100, 60, 170))
        }
        AssetKind::Audio => (C::from_rgb(120, 210, 140), C::from_rgb(20, 130, 50)),
        AssetKind::Video => (C::from_rgb(240, 130, 120), C::from_rgb(170, 40, 30)),
        AssetKind::MeshBlob => (C::from_rgb(240, 170, 110), C::from_rgb(170, 90, 20)),
        AssetKind::Xml => (C::from_rgb(170, 200, 200), C::from_rgb(60, 110, 110)),
        AssetKind::Unknown => (C::from_gray(140), C::from_gray(110)),
    };
    if dark { d } else { l }
}

/// Fixed-width coloured kind tag, e.g. `[MESH]`.
pub fn kind_label(ui: &egui::Ui, kind: AssetKind) -> egui::RichText {
    egui::RichText::new(format!("{:<4}", crate::search::kind_tag(kind)))
        .monospace()
        .color(kind_color(kind, ui.visuals().dark_mode))
}

/// Routes dock tabs to panel functions.
pub struct Viewer<'a> {
    pub studio: &'a mut Studio,
}

impl egui_dock::TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        let extra = match tab {
            Tab::Jobs => {
                let n = self.studio.jobs.active_count();
                (n > 0).then(|| format!(" ({n})"))
            }
            Tab::Search => self
                .studio
                .search
                .result_count()
                .map(|n| format!(" ({})", crate::util::format_count(n as u64))),
            _ => None,
        };
        format!("{}{}", tab.title(), extra.unwrap_or_default()).into()
    }

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::unique(("studio_tab", *tab))
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Explorer => explorer::show(self.studio, ui),
            Tab::Search => search_panel::show(self.studio, ui),
            Tab::Inspector => inspector::show(self.studio, ui),
            Tab::Hex => hex_view::show(self.studio, ui),
            Tab::Preview => preview_panel::show(self.studio, ui),
            Tab::Log => log_panel::show(self.studio, ui),
            Tab::Jobs => jobs_panel::show(self.studio, ui),
        }
    }

    fn scroll_bars(&self, tab: &Tab) -> [bool; 2] {
        match tab {
            Tab::Inspector => [false, true],
            _ => [false, false],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_has_every_tab_once() {
        let d = default_dock();
        for t in Tab::ALL {
            assert_eq!(
                d.iter_all_tabs().filter(|(_, x)| **x == t).count(),
                1,
                "{t:?}"
            );
        }
    }

    #[test]
    fn toggling() {
        let mut d = default_dock();
        toggle_tab(&mut d, Tab::Log);
        assert!(d.find_tab(&Tab::Log).is_none());
        toggle_tab(&mut d, Tab::Log);
        assert!(d.find_tab(&Tab::Log).is_some());
        focus_tab(&mut d, Tab::Hex);
        assert!(d.find_tab(&Tab::Hex).is_some());
    }

    #[test]
    fn dock_serialises() {
        let d = default_dock();
        // eframe persists with RON (JSON cannot hold the infinite rects in a dock).
        let text = ron::to_string(&d).unwrap();
        let back: DockState<Tab> = ron::from_str(&text).unwrap();
        assert_eq!(back.iter_all_tabs().count(), Tab::ALL.len());
    }
}
