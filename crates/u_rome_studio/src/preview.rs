//! Preview registry. Decoder crates plug in by implementing [`Previewer`]
//! and registering it in [`PreviewRegistry::with_builtin`] (or via
//! [`PreviewRegistry::register`] from the app).
//!
//! A previewer is chosen per entry by [`Previewer::supports`] and
//! [`Previewer::priority`]; the user can switch between all matching
//! previewers in the Preview tab. Heavy decoding should be spawned on the
//! [`JobSystem`] from [`Previewer::open`] and polled in [`PreviewView::ui`].

use std::sync::Arc;

use eframe::egui;
use u_rome_core::{AssetKind, detect};

use crate::backend::{ByteSource, EntryInfo, read_vec};
use crate::jobs::JobSystem;

/// GPU access for previewers that render (textures, models).
#[derive(Clone, Default)]
pub enum GpuContext {
    Wgpu(eframe::egui_wgpu::RenderState),
    Glow(Arc<eframe::glow::Context>),
    #[default]
    None,
}

/// Everything a previewer gets for one entry.
#[derive(Clone)]
pub struct PreviewRequest {
    pub entry: EntryInfo,
    pub source: Arc<dyn ByteSource>,
    pub jobs: JobSystem,
    pub gpu: GpuContext,
}

/// Live preview UI for one entry.
pub trait PreviewView {
    fn ui(&mut self, ui: &mut egui::Ui);
}

/// A factory of preview views for some asset kinds.
pub trait Previewer: Send + Sync {
    /// Stable identifier (`placeholder`, `xml-text`, `texture`, ...).
    fn id(&self) -> &'static str;
    /// Name shown in the previewer selector.
    fn label(&self) -> &str;
    fn supports(&self, entry: &EntryInfo) -> bool;
    /// Higher wins when several previewers support an entry.
    fn priority(&self) -> i32 {
        0
    }
    fn open(&self, request: PreviewRequest) -> Box<dyn PreviewView>;
}

/// Ordered set of previewers.
#[derive(Clone, Default)]
pub struct PreviewRegistry {
    previewers: Vec<Arc<dyn Previewer>>,
}

impl PreviewRegistry {
    /// Built-in previewers: placeholder for every kind, XML text view.
    pub fn with_builtin() -> Self {
        let mut r = Self::default();
        r.register(Arc::new(PlaceholderPreviewer));
        r.register(Arc::new(XmlTextPreviewer));
        r
    }

    pub fn register(&mut self, p: Arc<dyn Previewer>) {
        log::debug!("previewer registered: {} ({})", p.label(), p.id());
        self.previewers.push(p);
    }

    /// Previewers supporting `entry`, best first.
    pub fn candidates(&self, entry: &EntryInfo) -> Vec<Arc<dyn Previewer>> {
        let mut v: Vec<_> = self
            .previewers
            .iter()
            .filter(|p| p.supports(entry))
            .cloned()
            .collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.priority()));
        v
    }

    pub fn best(&self, entry: &EntryInfo) -> Option<Arc<dyn Previewer>> {
        self.candidates(entry).into_iter().next()
    }

    pub fn len(&self) -> usize {
        self.previewers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.previewers.is_empty()
    }
}

/// Which decoder crate will handle a kind (shown in the placeholder).
pub fn planned_decoder(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Archive => "u_rome_rpf9 (browse it in the Explorer)",
        AssetKind::TextureStore | AssetKind::TextureTable => "u_rome_texture",
        AssetKind::Pso => "u_rome_pso",
        AssetKind::MetaResource => "u_rome_map / u_rome_pso",
        AssetKind::Audio => "u_rome_audio",
        AssetKind::Video => "none planned (Bink 2 is proprietary)",
        AssetKind::MeshBlob => "u_rome_model + u_rome_render",
        AssetKind::Xml => "built-in text view",
        AssetKind::Unknown => "none (unidentified format)",
    }
}

/// Fallback for every entry: kind facts and the leading bytes.
pub struct PlaceholderPreviewer;

struct PlaceholderView {
    entry: EntryInfo,
    head: Result<Vec<u8>, String>,
    detected: AssetKind,
}

impl Previewer for PlaceholderPreviewer {
    fn id(&self) -> &'static str {
        "placeholder"
    }

    fn label(&self) -> &str {
        "Summary"
    }

    fn supports(&self, _entry: &EntryInfo) -> bool {
        true
    }

    fn priority(&self) -> i32 {
        -100
    }

    fn open(&self, request: PreviewRequest) -> Box<dyn PreviewView> {
        let head = read_vec(request.source.as_ref(), 0, 64, 64).map_err(|e| e.to_string());
        let detected = head.as_deref().map(detect).unwrap_or(AssetKind::Unknown);
        Box::new(PlaceholderView {
            entry: request.entry,
            head,
            detected,
        })
    }
}

impl PreviewView for PlaceholderView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading(self.entry.kind.label());
        ui.label(
            egui::RichText::new(format!(
                "No previewer for this kind yet. Planned decoder: {}.",
                planned_decoder(self.entry.kind)
            ))
            .weak(),
        );
        ui.add_space(8.0);
        egui::Grid::new("placeholder_grid")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label("Listed kind");
                ui.label(self.entry.kind.label());
                ui.end_row();
                ui.label("Detected from bytes");
                let same = self.detected == self.entry.kind;
                let text = egui::RichText::new(self.detected.label());
                ui.label(if same {
                    text
                } else {
                    text.color(ui.visuals().warn_fg_color)
                });
                ui.end_row();
                ui.label("Size");
                ui.label(crate::util::format_size(self.entry.size));
                ui.end_row();
            });
        ui.add_space(8.0);
        match &self.head {
            Ok(b) => {
                ui.label("First bytes");
                for (i, chunk) in b.chunks(16).enumerate() {
                    let (h, a) = crate::hex::format_row(chunk, 16);
                    ui.monospace(format!("{:08X}  {h}  {a}", i * 16));
                }
            }
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("read failed: {e}"));
            }
        }
    }
}

/// UTF-8 text view for XML entries (first 2 MiB).
pub struct XmlTextPreviewer;

/// Text preview limit.
pub const MAX_TEXT_PREVIEW: usize = 2 * 1024 * 1024;

struct TextView {
    text: String,
    truncated: bool,
}

impl Previewer for XmlTextPreviewer {
    fn id(&self) -> &'static str {
        "xml-text"
    }

    fn label(&self) -> &str {
        "Text"
    }

    fn supports(&self, entry: &EntryInfo) -> bool {
        entry.kind == AssetKind::Xml
    }

    fn priority(&self) -> i32 {
        10
    }

    fn open(&self, request: PreviewRequest) -> Box<dyn PreviewView> {
        let len = request.source.len();
        let text = match read_vec(request.source.as_ref(), 0, len, MAX_TEXT_PREVIEW) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => format!("read failed: {e}"),
        };
        Box::new(TextView {
            text,
            truncated: len > MAX_TEXT_PREVIEW as u64,
        })
    }
}

impl PreviewView for TextView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        if self.truncated {
            ui.label(
                egui::RichText::new(format!(
                    "Showing the first {}.",
                    crate::util::format_size(MAX_TEXT_PREVIEW as u64)
                ))
                .weak(),
            );
        }
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.text.as_str())
                        .code_editor()
                        .desired_width(f32::INFINITY),
                );
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Codec, MemorySource};

    fn entry(kind: AssetKind) -> EntryInfo {
        EntryInfo {
            id: 1,
            parent: None,
            name: Some("x".into()),
            name_hash: None,
            kind,
            codec: Codec::Stored,
            offset: 0,
            stored_size: 4,
            size: 4,
            child_count: None,
            provisional: false,
        }
    }

    struct Fake;
    impl Previewer for Fake {
        fn id(&self) -> &'static str {
            "fake-texture"
        }
        fn label(&self) -> &str {
            "Texture"
        }
        fn supports(&self, e: &EntryInfo) -> bool {
            e.kind == AssetKind::TextureTable
        }
        fn priority(&self) -> i32 {
            50
        }
        fn open(&self, r: PreviewRequest) -> Box<dyn PreviewView> {
            PlaceholderPreviewer.open(r)
        }
    }

    #[test]
    fn dispatch_by_kind() {
        let mut r = PreviewRegistry::with_builtin();
        assert_eq!(r.best(&entry(AssetKind::Xml)).unwrap().id(), "xml-text");
        assert_eq!(
            r.best(&entry(AssetKind::Audio)).unwrap().id(),
            "placeholder"
        );
        r.register(Arc::new(Fake));
        let c = r.candidates(&entry(AssetKind::TextureTable));
        let ids: Vec<_> = c.iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["fake-texture", "placeholder"]);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn views_open_without_panicking() {
        let req = PreviewRequest {
            entry: entry(AssetKind::Xml),
            source: Arc::new(MemorySource::new(b"<?xml?><a/>".to_vec())),
            jobs: JobSystem::new(1),
            gpu: GpuContext::None,
        };
        let _ = XmlTextPreviewer.open(req.clone());
        let _ = PlaceholderPreviewer.open(req.clone());
        req.jobs.shutdown();
    }
}
