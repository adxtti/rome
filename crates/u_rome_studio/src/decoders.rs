//! Previewers backed by the decoder crates: textures, models, audio, PSO data
//! and map placements. Decoding runs on the job system; views poll for the
//! result and show errors inline so a failure on real data is visible and
//! copyable.

use std::sync::Arc;

use eframe::egui;
use u_rome_core::{AssetKind, Image};

use crate::backend::{ByteSource, EntryInfo, read_vec};
use crate::jobs::{JobError, JobHandle, JobSystem};
use crate::preview::{GpuContext, PreviewRegistry, PreviewRequest, PreviewView, Previewer};

/// Largest entry a previewer will load into memory.
const MAX_PREVIEW_BYTES: usize = 1 << 30;

pub fn register(registry: &mut PreviewRegistry) {
    registry.register(Arc::new(TexturePreviewer));
    registry.register(Arc::new(ModelPreviewer));
    registry.register(Arc::new(AudioPreviewer));
    registry.register(Arc::new(PsoPreviewer));
    registry.register(Arc::new(MapPreviewer));
}

/// A value being produced by a background job.
enum Pending<T> {
    Loading(JobHandle<T>),
    Ready(T),
    Failed(String),
}

impl<T: Send + 'static> Pending<T> {
    fn spawn(
        jobs: &JobSystem,
        name: impl Into<String>,
        f: impl FnOnce(&crate::jobs::JobContext) -> Result<T, JobError> + Send + 'static,
    ) -> Self {
        Pending::Loading(jobs.spawn(name, f))
    }

    /// Advance; returns true when a result arrived this frame.
    fn poll(&mut self, ui: &egui::Ui) -> bool {
        if let Pending::Loading(h) = self {
            match h.try_take() {
                Some(Ok(v)) => {
                    *self = Pending::Ready(v);
                    return true;
                }
                Some(Err(e)) => *self = Pending::Failed(e.to_string()),
                None => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        false
    }

    /// Shows spinner or error; returns the value when ready.
    fn show(&mut self, ui: &mut egui::Ui, what: &str) -> Option<&mut T> {
        match self {
            Pending::Loading(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Decoding {what}..."));
                });
                None
            }
            Pending::Failed(e) => {
                error_box(ui, &format!("Could not decode {what}"), e);
                None
            }
            Pending::Ready(v) => Some(v),
        }
    }
}

fn error_box(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.colored_label(ui.visuals().error_fg_color, title);
    ui.label(egui::RichText::new(detail).monospace());
    if ui.button("Copy error").clicked() {
        ui.ctx().copy_text(format!("{title}: {detail}"));
    }
}

fn provisional_badge(ui: &mut egui::Ui, provisional: bool) {
    if provisional {
        ui.label(
            egui::RichText::new("provisional")
                .small()
                .color(ui.visuals().warn_fg_color),
        )
        .on_hover_text("Part of this decode relies on format facts that are not confirmed yet.");
    }
}

fn read_all(source: &Arc<dyn ByteSource>) -> Result<Vec<u8>, JobError> {
    read_vec(source.as_ref(), 0, source.len(), MAX_PREVIEW_BYTES)
        .map_err(|e| JobError::Failed(e.to_string()))
}

fn save_dialog(name: &str, bytes: &[u8]) {
    let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() else {
        return;
    };
    match std::fs::write(&path, bytes) {
        Ok(()) => log::info!("saved {}", path.display()),
        Err(e) => log::error!("saving {} failed: {e}", path.display()),
    }
}

fn stem(entry: &EntryInfo) -> String {
    entry
        .name
        .as_deref()
        .and_then(|n| n.rsplit(['/', '\\']).next())
        .map(|n| n.split('.').next().unwrap_or(n).to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("entry_{:x}", entry.id))
}

// ---------------------------------------------------------------- textures

pub struct TexturePreviewer;

struct TextureData {
    bytes: Arc<Vec<u8>>,
    store: Arc<u_rome_texture::TextureStore>,
}

struct TextureView {
    entry: EntryInfo,
    jobs: JobSystem,
    data: Pending<TextureData>,
    selected: Option<usize>,
    image: Option<Pending<Arc<Image>>>,
    viewer: u_rome_render::ImageView,
}

impl Previewer for TexturePreviewer {
    fn id(&self) -> &'static str {
        "texture"
    }
    fn label(&self) -> &str {
        "Textures"
    }
    fn supports(&self, e: &EntryInfo) -> bool {
        matches!(e.kind, AssetKind::TextureStore | AssetKind::TextureTable)
    }
    fn priority(&self) -> i32 {
        50
    }
    fn open(&self, r: PreviewRequest) -> Box<dyn PreviewView> {
        let source = r.source.clone();
        let data = Pending::spawn(&r.jobs, "parse texture store", move |ctx| {
            ctx.set_status("reading texture store");
            let bytes = read_all(&source)?;
            let store = u_rome_texture::TextureStore::parse(&bytes)
                .map_err(|e| JobError::Failed(e.to_string()))?;
            Ok(TextureData {
                bytes: Arc::new(bytes),
                store: Arc::new(store),
            })
        });
        Box::new(TextureView {
            entry: r.entry,
            jobs: r.jobs,
            data,
            selected: None,
            image: None,
            viewer: u_rome_render::ImageView::new(),
        })
    }
}

impl TextureView {
    fn select(&mut self, index: usize) {
        let Pending::Ready(d) = &self.data else {
            return;
        };
        self.selected = Some(index);
        let bytes = d.bytes.clone();
        let store = d.store.clone();
        self.image = Some(Pending::spawn(
            &self.jobs,
            format!("decode texture {index}"),
            move |_| {
                store
                    .decode(&bytes, index, u_rome_texture::MipSelection::PreferPlain)
                    .map(Arc::new)
                    .map_err(|e| JobError::Failed(e.to_string()))
            },
        ));
    }
}

impl PreviewView for TextureView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        if self.data.poll(ui) && self.selected.is_none() {
            self.select(0);
        }
        let Some(data) = self.data.show(ui, "texture store") else {
            return;
        };
        let infos: Vec<u_rome_texture::TextureInfo> = data.store.textures().to_vec();
        if infos.is_empty() {
            ui.label("The store lists no textures.");
            return;
        }
        let mut pick = None;
        egui::Panel::left("texture_list")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                ui.label(format!("{} textures", infos.len()));
                egui::ScrollArea::vertical().show_rows(ui, 18.0, infos.len(), |ui, range| {
                    for i in range {
                        let t = &infos[i];
                        let label = format!(
                            "{:>4}  {}x{}  {:?}{}",
                            t.index,
                            t.width,
                            t.height,
                            t.format,
                            if t.provisional { "  *" } else { "" }
                        );
                        if ui
                            .selectable_label(
                                self.selected == Some(i),
                                egui::RichText::new(label).monospace(),
                            )
                            .on_hover_text(format!(
                                "hash {:016x}\nmips {}\n{}",
                                t.hash,
                                t.mip_count,
                                t.notes.join("\n")
                            ))
                            .clicked()
                        {
                            pick = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = pick {
            self.select(i);
        }
        let Some(pending) = &mut self.image else {
            return;
        };
        if pending.poll(ui)
            && let Pending::Ready(img) = pending
        {
            self.viewer.set_image_shared(img.clone());
        }
        let Some(img) = pending.show(ui, "texture") else {
            return;
        };
        let img = img.clone();
        ui.horizontal(|ui| {
            provisional_badge(ui, img.provisional);
            let name = format!("{}_{}", stem(&self.entry), self.selected.unwrap_or(0));
            if ui.button("Export PNG").clicked() {
                match u_rome_texture::export_png(&img, 0) {
                    Ok(png) => save_dialog(&format!("{name}.png"), &png),
                    Err(e) => log::error!("PNG export failed: {e}"),
                }
            }
            if ui.button("Export DDS").clicked() {
                match u_rome_texture::export_dds(&img) {
                    Ok(dds) => save_dialog(&format!("{name}.dds"), &dds),
                    Err(e) => log::error!("DDS export failed: {e}"),
                }
            }
        });
        self.viewer.show(ui);
    }
}

// ------------------------------------------------------------------ models

pub struct ModelPreviewer;

struct ModelView {
    entry: EntryInfo,
    data: Pending<u_rome_model::DecodedModel>,
    view: u_rome_render::ModelView,
    loaded: bool,
}

impl Previewer for ModelPreviewer {
    fn id(&self) -> &'static str {
        "model"
    }
    fn label(&self) -> &str {
        "Model"
    }
    fn supports(&self, e: &EntryInfo) -> bool {
        // Drawables have no reliable magic yet, so every unidentified entry
        // can be tried as one from the previewer selector.
        matches!(e.kind, AssetKind::MeshBlob | AssetKind::Unknown)
    }
    fn priority(&self) -> i32 {
        -200
    }
    fn open(&self, r: PreviewRequest) -> Box<dyn PreviewView> {
        let source = r.source.clone();
        let data = Pending::spawn(&r.jobs, "decode model", move |_| {
            let bytes = read_all(&source)?;
            u_rome_model::decode_drawable(&bytes).map_err(|e| JobError::Failed(e.to_string()))
        });
        let rs = match &r.gpu {
            GpuContext::Wgpu(rs) => Some(rs),
            _ => None,
        };
        Box::new(ModelView {
            entry: r.entry,
            data,
            view: u_rome_render::ModelView::new(rs),
            loaded: false,
        })
    }
}

impl PreviewView for ModelView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.data.poll(ui);
        let Some(decoded) = self.data.show(ui, "model") else {
            return;
        };
        if !self.loaded {
            self.view.set_model(&decoded.model);
            self.loaded = true;
        }
        ui.horizontal(|ui| {
            provisional_badge(ui, decoded.provisional);
            if ui.button("Export glTF").clicked() {
                let lod = self.view.lod();
                match u_rome_model::export_glb(&decoded.model, lod, &[]) {
                    Ok(glb) => save_dialog(&format!("{}_lod{lod}.glb", stem(&self.entry)), &glb),
                    Err(e) => log::error!("glTF export failed: {e}"),
                }
            }
            if !decoded.warnings.is_empty() {
                ui.label(format!("{} warnings", decoded.warnings.len()))
                    .on_hover_text(decoded.warnings.join("\n"));
            }
        });
        self.view.show(ui);
    }
}

// ------------------------------------------------------------------- audio

pub struct AudioPreviewer;

struct AudioView {
    entry: EntryInfo,
    jobs: JobSystem,
    bank: Pending<Arc<u_rome_audio::AwcBank>>,
    decoding: Option<(usize, Pending<u_rome_audio::Pcm>)>,
    output: Option<rodio::MixerDeviceSink>,
    player: Option<rodio::Player>,
    playing: Option<usize>,
    audio_error: Option<String>,
}

impl Previewer for AudioPreviewer {
    fn id(&self) -> &'static str {
        "audio"
    }
    fn label(&self) -> &str {
        "Audio"
    }
    fn supports(&self, e: &EntryInfo) -> bool {
        e.kind == AssetKind::Audio
    }
    fn priority(&self) -> i32 {
        50
    }
    fn open(&self, r: PreviewRequest) -> Box<dyn PreviewView> {
        let source = r.source.clone();
        let bank = Pending::spawn(&r.jobs, "parse audio bank", move |_| {
            let bytes = read_all(&source)?;
            u_rome_audio::AwcBank::parse(bytes)
                .map(Arc::new)
                .map_err(|e| JobError::Failed(e.to_string()))
        });
        Box::new(AudioView {
            entry: r.entry,
            jobs: r.jobs,
            bank,
            decoding: None,
            output: None,
            player: None,
            playing: None,
            audio_error: None,
        })
    }
}

impl AudioView {
    fn play(&mut self, pcm: &u_rome_audio::Pcm) {
        if self.output.is_none() {
            match rodio::DeviceSinkBuilder::open_default_sink() {
                Ok(s) => self.output = Some(s),
                Err(e) => {
                    self.audio_error = Some(format!("no audio output device: {e}"));
                    return;
                }
            }
        }
        let (Some(channels), Some(rate)) = (
            std::num::NonZero::new(pcm.channels),
            std::num::NonZero::new(pcm.sample_rate),
        ) else {
            self.audio_error = Some("stream has zero channels or sample rate".into());
            return;
        };
        let Some(out) = &self.output else { return };
        let samples: Vec<f32> = pcm.samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let player = rodio::Player::connect_new(out.mixer());
        player.append(rodio::buffer::SamplesBuffer::new(channels, rate, samples));
        self.player = Some(player);
    }

    fn stop(&mut self) {
        if let Some(p) = self.player.take() {
            p.stop();
        }
        self.playing = None;
    }
}

impl PreviewView for AudioView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.bank.poll(ui);
        let jobs = self.jobs.clone();
        let entry_stem = stem(&self.entry);
        let Some(bank) = self.bank.show(ui, "audio bank") else {
            return;
        };
        let bank = bank.clone();

        // Finish a pending decode and start playback.
        if let Some((idx, pending)) = &mut self.decoding {
            let idx = *idx;
            pending.poll(ui);
            match pending {
                Pending::Ready(pcm) => {
                    let pcm = std::mem::replace(
                        pcm,
                        u_rome_audio::Pcm {
                            samples: Vec::new(),
                            channels: 0,
                            sample_rate: 0,
                        },
                    );
                    self.decoding = None;
                    self.play(&pcm);
                    self.playing = Some(idx);
                }
                Pending::Failed(e) => {
                    self.audio_error = Some(format!("stream {idx}: {e}"));
                    self.decoding = None;
                }
                Pending::Loading(_) => {}
            }
        }
        if self.player.as_ref().is_some_and(|p| p.empty()) {
            self.player = None;
            self.playing = None;
        }
        if let Some(e) = &self.audio_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }

        let streams = bank.streams().to_vec();
        ui.label(format!("{} streams", streams.len()));
        let mut action: Option<(usize, &'static str)> = None;
        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .column(egui_extras::Column::auto())
            .column(egui_extras::Column::auto())
            .column(egui_extras::Column::auto())
            .column(egui_extras::Column::auto())
            .column(egui_extras::Column::auto())
            .column(egui_extras::Column::remainder())
            .header(18.0, |mut h| {
                for t in ["#", "id", "codec", "format", "length", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, streams.len(), |mut row| {
                    let i = row.index();
                    let s = &streams[i];
                    row.col(|ui| {
                        ui.label(i.to_string());
                    });
                    row.col(|ui| {
                        ui.monospace(format!("{:08x}", s.id));
                    });
                    row.col(|ui| {
                        ui.label(format!("{:?}", s.codec));
                        provisional_badge(ui, s.provisional);
                    });
                    row.col(|ui| {
                        ui.label(format!("{} Hz, {} ch", s.sample_rate, s.channels));
                    });
                    row.col(|ui| {
                        ui.label(format!("{:.1}s", s.duration_secs()));
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            let is_playing = self.playing == Some(i);
                            let busy = self.decoding.as_ref().is_some_and(|(d, _)| *d == i);
                            if busy {
                                ui.spinner();
                            } else if ui
                                .button(if is_playing { "Stop" } else { "Play" })
                                .clicked()
                            {
                                action = Some((i, if is_playing { "stop" } else { "play" }));
                            }
                            if s.codec == u_rome_audio::Codec::Opus && ui.button("Ogg").clicked() {
                                action = Some((i, "ogg"));
                            }
                            if ui.button("WAV").clicked() {
                                action = Some((i, "wav"));
                            }
                        });
                    });
                });
            });

        match action {
            Some((_, "stop")) => self.stop(),
            Some((i, "play")) => {
                self.stop();
                self.audio_error = None;
                let b = bank.clone();
                self.decoding = Some((
                    i,
                    Pending::spawn(&jobs, format!("decode audio stream {i}"), move |_| {
                        u_rome_audio::decode_pcm(&b, i).map_err(|e| JobError::Failed(e.to_string()))
                    }),
                ));
            }
            Some((i, "ogg")) => match bank.ogg_opus(i) {
                Ok(ogg) => save_dialog(&format!("{entry_stem}_{i:02}.ogg"), &ogg),
                Err(e) => self.audio_error = Some(format!("Ogg export failed: {e}")),
            },
            Some((i, "wav")) => match u_rome_audio::decode_pcm(&bank, i) {
                Ok(pcm) => save_dialog(
                    &format!("{entry_stem}_{i:02}.wav"),
                    &u_rome_audio::write_wav(&pcm),
                ),
                Err(e) => self.audio_error = Some(format!("WAV export failed: {e}")),
            },
            _ => {}
        }
    }
}

// --------------------------------------------------------------- PSO / JSON

pub struct PsoPreviewer;

struct JsonView {
    entry: EntryInfo,
    title: &'static str,
    data: Pending<(serde_json::Value, bool, Vec<String>)>,
}

impl Previewer for PsoPreviewer {
    fn id(&self) -> &'static str {
        "pso"
    }
    fn label(&self) -> &str {
        "PSO data"
    }
    fn supports(&self, e: &EntryInfo) -> bool {
        e.kind == AssetKind::Pso
    }
    fn priority(&self) -> i32 {
        50
    }
    fn open(&self, r: PreviewRequest) -> Box<dyn PreviewView> {
        let source = r.source.clone();
        let data = Pending::spawn(&r.jobs, "decode PSO document", move |_| {
            let bytes = read_all(&source)?;
            let doc = u_rome_pso::PsoDocument::parse(&bytes)
                .map_err(|e| JobError::Failed(e.to_string()))?;
            let mut names = u_rome_pso::HashMapNames::new();
            for n in u_rome_pso::KNOWN_NAMES {
                names.add(n);
            }
            let root = doc.root().map_err(|e| JobError::Failed(e.to_string()))?;
            let json = u_rome_pso::to_json_with_schema(&root, &names, Some(doc.schema()));
            let provisional = doc.root_info().is_some_and(|r| r.provisional);
            Ok((json, provisional, doc.warnings().to_vec()))
        });
        Box::new(JsonView {
            entry: r.entry,
            title: "PSO document",
            data,
        })
    }
}

/// Collapsible JSON tree with a node budget so huge documents stay responsive.
fn json_tree(ui: &mut egui::Ui, key: &str, v: &serde_json::Value, budget: &mut usize, path: &str) {
    if *budget == 0 {
        return;
    }
    *budget -= 1;
    match v {
        serde_json::Value::Object(map) => {
            egui::CollapsingHeader::new(format!("{key}  {{{}}}", map.len()))
                .id_salt(path)
                .default_open(path.len() < 12)
                .show(ui, |ui| {
                    for (k, child) in map {
                        json_tree(ui, k, child, budget, &format!("{path}/{k}"));
                    }
                });
        }
        serde_json::Value::Array(items) => {
            egui::CollapsingHeader::new(format!("{key}  [{}]", items.len()))
                .id_salt(path)
                .show(ui, |ui| {
                    for (i, child) in items.iter().enumerate().take(5000) {
                        json_tree(ui, &i.to_string(), child, budget, &format!("{path}/{i}"));
                    }
                    if items.len() > 5000 {
                        ui.label(format!(
                            "... {} more (export JSON to see all)",
                            items.len() - 5000
                        ));
                    }
                });
        }
        other => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{key}:")).strong());
                ui.monospace(other.to_string());
            });
        }
    }
}

impl PreviewView for JsonView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.data.poll(ui);
        let title = self.title;
        let Some((json, provisional, warnings)) = self.data.show(ui, title) else {
            return;
        };
        ui.horizontal(|ui| {
            provisional_badge(ui, *provisional);
            if ui.button("Export JSON").clicked() {
                match serde_json::to_vec_pretty(json) {
                    Ok(b) => save_dialog(&format!("{}.json", stem(&self.entry)), &b),
                    Err(e) => log::error!("JSON export failed: {e}"),
                }
            }
            if !warnings.is_empty() {
                ui.label(format!("{} warnings", warnings.len()))
                    .on_hover_text(warnings.join("\n"));
            }
        });
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut budget = 20_000usize;
                json_tree(ui, "root", json, &mut budget, "r");
                if budget == 0 {
                    ui.label("Display limit reached; export JSON to see everything.");
                }
            });
    }
}

// --------------------------------------------------------------------- map

pub struct MapPreviewer;

struct MapSummary {
    map: u_rome_map::MapData,
    raster: Option<Image>,
}

struct MapView {
    entry: EntryInfo,
    data: Pending<MapSummary>,
    viewer: u_rome_render::ImageView,
    shown: bool,
}

impl Previewer for MapPreviewer {
    fn id(&self) -> &'static str {
        "map"
    }
    fn label(&self) -> &str {
        "Map placements"
    }
    fn supports(&self, e: &EntryInfo) -> bool {
        e.kind == AssetKind::MetaResource
    }
    fn priority(&self) -> i32 {
        50
    }
    fn open(&self, r: PreviewRequest) -> Box<dyn PreviewView> {
        let source = r.source.clone();
        let data = Pending::spawn(&r.jobs, "decode map data", move |_| {
            let bytes = read_all(&source)?;
            let meta = u_rome_map::MetaResource::parse(&bytes)
                .map_err(|e| JobError::Failed(e.to_string()))?;
            let map = u_rome_map::MapData::from_meta(&meta)
                .map_err(|e| JobError::Failed(e.to_string()))?;
            let raster = u_rome_map::Bounds2::from_points(map.entities.iter().map(|p| p.position))
                .map(|b| u_rome_map::render_topdown(&map.entities, b, 512, 512));
            Ok(MapSummary { map, raster })
        });
        Box::new(MapView {
            entry: r.entry,
            data,
            viewer: u_rome_render::ImageView::new(),
            shown: false,
        })
    }
}

impl PreviewView for MapView {
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.data.poll(ui);
        let Some(s) = self.data.show(ui, "map data") else {
            return;
        };
        if !self.shown {
            if let Some(img) = &s.raster {
                self.viewer.set_image(img);
            }
            self.shown = true;
        }
        let m = &s.map;
        ui.horizontal(|ui| {
            provisional_badge(ui, m.provisional);
            ui.label(format!(
                "{}  parent {}  |  {} entities, {} prop batches, {} grass batches",
                m.name,
                m.parent,
                m.entities.len(),
                m.prop_batches.len(),
                m.grass_batches.len()
            ));
            if ui.button("Export JSON").clicked() {
                let rows: Vec<_> = m
                    .entities
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "archetype": p.archetype.to_string(),
                            "position": p.position,
                            "rotation": p.rotation,
                            "scale": [p.scale_xy, p.scale_z],
                            "lod_level": format!("{:?}", p.lod_level),
                            "parent_index": p.parent_index,
                            "lod_dist": p.lod_dist,
                        })
                    })
                    .collect();
                match serde_json::to_vec_pretty(&rows) {
                    Ok(b) => save_dialog(&format!("{}.json", stem(&self.entry)), &b),
                    Err(e) => log::error!("JSON export failed: {e}"),
                }
            }
        });
        if !m.warnings.is_empty() {
            ui.collapsing(format!("{} warnings", m.warnings.len()), |ui| {
                for w in &m.warnings {
                    ui.label(w);
                }
            });
        }
        egui::Panel::right("map_entities")
            .resizable(true)
            .default_size(360.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show_rows(ui, 16.0, m.entities.len(), |ui, range| {
                    for i in range {
                        let p = &m.entities[i];
                        ui.monospace(format!(
                            "{:>5} {} {:>8.1} {:>8.1} {:>7.1} {:?}",
                            i,
                            p.archetype,
                            p.position[0],
                            p.position[1],
                            p.position[2],
                            p.lod_level
                        ));
                    }
                });
            });
        if s.raster.is_some() {
            self.viewer.show(ui);
        } else {
            ui.label("No entities to plot.");
        }
    }
}
