//! Preview tab: dispatches the selected entry to a registered previewer.

use std::sync::Arc;

use eframe::egui;

use crate::app::Studio;
use crate::backend::{ByteSource, EntryInfo};
use crate::jobs::JobSystem;
use crate::preview::{GpuContext, PreviewRegistry, PreviewRequest, PreviewView, Previewer};

#[derive(Default)]
pub struct PreviewState {
    request: Option<PreviewRequest>,
    candidates: Vec<Arc<dyn Previewer>>,
    active: usize,
    view: Option<Box<dyn PreviewView>>,
}

impl PreviewState {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn open(
        &mut self,
        registry: &PreviewRegistry,
        entry: EntryInfo,
        source: Arc<dyn ByteSource>,
        jobs: JobSystem,
        gpu: GpuContext,
    ) {
        self.candidates = registry.candidates(&entry);
        self.request = Some(PreviewRequest {
            entry,
            source,
            jobs,
            gpu,
        });
        self.activate(0);
    }

    fn activate(&mut self, i: usize) {
        self.active = i;
        self.view = match (self.candidates.get(i), &self.request) {
            (Some(p), Some(r)) => {
                log::debug!("preview of {} with {}", r.entry.display_name(), p.id());
                Some(p.open(r.clone()))
            }
            _ => None,
        };
    }
}

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let st = &mut studio.preview;
    let Some(req) = &st.request else {
        ui.add_space(8.0);
        let msg = if studio.selection.is_loading() {
            "Loading entry data..."
        } else if let Some(e) = &studio.selection.source_error {
            e.as_str()
        } else {
            "Select an entry to preview it."
        };
        ui.label(egui::RichText::new(msg).weak());
        return;
    };
    let kind = req.entry.kind;
    let mut switch = None;
    ui.horizontal(|ui| {
        ui.label(super::kind_label(ui, kind));
        ui.label(format!("Detected kind: {}", kind.label()));
        if req.entry.provisional {
            ui.colored_label(ui.visuals().warn_fg_color, "provisional");
        }
        if st.candidates.len() > 1 {
            ui.separator();
            ui.label("View:");
            for (i, p) in st.candidates.iter().enumerate() {
                if ui.selectable_label(i == st.active, p.label()).clicked() && i != st.active {
                    switch = Some(i);
                }
            }
        }
    });
    ui.separator();
    if let Some(i) = switch {
        st.activate(i);
    }
    match &mut st.view {
        Some(v) => v.ui(ui),
        None => {
            ui.label("No previewer registered.");
        }
    }
}
