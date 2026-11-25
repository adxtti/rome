//! 3D model viewport widget.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::{Color32, Pos2, Rect, Response, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_wgpu::wgpu;
use glam::{Vec2, Vec3};
use u_rome_core::{Image, Model, Skeleton};

use crate::camera::{Aabb, OrbitCamera, gizmo_axes, ray_mesh, screen_to_ndc};
use crate::error::RenderError;
use crate::gpu::{
    Capabilities, FrameUniform, GpuMesh, Pipelines, RenderResources, SceneCallback, Targets,
};
use crate::prep::{
    LineVertex, MAX_WIRE_TRIANGLES, SKELETON_PROVISIONAL, SkeletonSource, expand_wireframe,
    grid_lines, prepare_mesh, skeleton_lines,
};

/// Highest material index accepted by [`ModelView::set_material_texture`].
pub const MAX_MATERIAL_INDEX: usize = u16::MAX as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShadingMode {
    /// Directional key light plus hemispheric ambient, base-colour texture if set.
    #[default]
    Lit,
    Normals,
    UvChecker,
    VertexColors,
    Wireframe,
}

impl ShadingMode {
    pub const ALL: [Self; 5] = [
        Self::Lit,
        Self::Normals,
        Self::UvChecker,
        Self::VertexColors,
        Self::Wireframe,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lit => "Lit",
            Self::Normals => "Normals",
            Self::UvChecker => "UV checker",
            Self::VertexColors => "Vertex colours",
            Self::Wireframe => "Wireframe",
        }
    }

    fn shader_id(self) -> u32 {
        match self {
            Self::Lit | Self::Wireframe => 0,
            Self::Normals => 1,
            Self::UvChecker => 2,
            Self::VertexColors => 3,
        }
    }
}

/// CPU-side data kept for stats, picking and the wireframe fallback.
struct CpuMesh {
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
    material: Option<usize>,
    bounds: Option<Aabb>,
}

struct CpuModel {
    name: Option<String>,
    lods: Vec<Vec<CpuMesh>>,
    skeleton: Option<Skeleton>,
    material_count: usize,
}

struct Gpu {
    rs: egui_wgpu::RenderState,
    view_id: u64,
    caps: Capabilities,
    texture_id: Option<egui::TextureId>,
    target_size: [u32; 2],
}

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

/// Interactive 3D viewport for a [`u_rome_core::Model`].
///
/// Controls: left drag orbits, right or middle drag pans, wheel zooms,
/// F (or the Frame button) frames the visible meshes, double-click sets the
/// orbit pivot to the surface under the cursor.
pub struct ModelView {
    gpu: Option<Gpu>,
    /// Why the GPU path is unavailable, shown in the viewport.
    gpu_message: Option<String>,
    model: Option<CpuModel>,
    pub camera: OrbitCamera,
    pub mode: ShadingMode,
    lod: usize,
    visible: Vec<Vec<bool>>,
    pub show_grid: bool,
    pub show_skeleton: bool,
    pub show_side_panel: bool,
    skeleton_source: SkeletonSource,
    skeleton_cache: Option<(SkeletonSource, Vec<LineVertex>)>,
    issues: Vec<String>,
    material_status: BTreeMap<usize, String>,
    frame_pending: bool,
}

impl ModelView {
    /// Create a viewport. Pass `frame.wgpu_render_state()` (eframe) or
    /// `None`; without a wgpu render state (glow renderer) the widget shows
    /// an explanatory message instead of rendering.
    pub fn new(render_state: Option<&egui_wgpu::RenderState>) -> Self {
        let mut view = Self {
            gpu: None,
            gpu_message: Some(
                "3D viewport needs the wgpu renderer. Studio is running on another \
                 renderer (for example glow), so models cannot be drawn."
                    .into(),
            ),
            model: None,
            camera: OrbitCamera::default(),
            mode: ShadingMode::default(),
            lod: 0,
            visible: Vec::new(),
            show_grid: true,
            show_skeleton: false,
            show_side_panel: true,
            skeleton_source: SkeletonSource::default(),
            skeleton_cache: None,
            issues: Vec::new(),
            material_status: BTreeMap::new(),
            frame_pending: true,
        };
        if let Some(rs) = render_state {
            match Self::init_gpu(rs) {
                Ok(gpu) => {
                    view.gpu = Some(gpu);
                    view.gpu_message = None;
                }
                Err(e) => {
                    log::warn!("render: 3D viewport disabled: {e}");
                    view.gpu_message = Some(format!("3D viewport could not start: {e}"));
                }
            }
        }
        view
    }

    fn init_gpu(rs: &egui_wgpu::RenderState) -> Result<Gpu, String> {
        let view_id = NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed);
        let mut renderer = rs.renderer.write();
        let res = &mut renderer.callback_resources;
        if !res.contains::<RenderResources>() {
            let caps = Capabilities::detect(rs);
            let pipelines = Pipelines::new(&rs.device, &rs.queue, caps)?;
            res.insert(RenderResources {
                pipelines,
                views: Default::default(),
            });
        }
        let Some(resources) = res.get_mut::<RenderResources>() else {
            return Err("render resources missing".into());
        };
        resources.add_view(&rs.device, view_id)?;
        Ok(Gpu {
            rs: rs.clone(),
            view_id,
            caps: resources.pipelines.caps.clone(),
            texture_id: None,
            target_size: [0, 0],
        })
    }

    /// GPU capabilities in use, if the viewport is running on wgpu.
    pub fn capabilities(&self) -> Option<&Capabilities> {
        self.gpu.as_ref().map(|g| &g.caps)
    }

    /// Problems found in the current model (also logged as warnings).
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    pub fn lod(&self) -> usize {
        self.lod
    }

    pub fn set_lod(&mut self, lod: usize) {
        let count = self.model.as_ref().map_or(0, |m| m.lods.len());
        self.lod = lod.min(count.saturating_sub(1));
    }

    /// Frame the camera on the visible meshes at the next `show`.
    pub fn frame(&mut self) {
        self.frame_pending = true;
    }

    /// Replace the displayed model. Invalid meshes are skipped and reported
    /// through [`Self::issues`]; material textures are cleared.
    pub fn set_model(&mut self, model: &Model) {
        self.issues.clear();
        self.material_status.clear();
        self.skeleton_cache = None;
        let mut prepared = Vec::with_capacity(model.lods.len());
        for (l, lod) in model.lods.iter().enumerate() {
            let meshes: Vec<_> = lod
                .meshes
                .iter()
                .enumerate()
                .map(|(m, mesh)| prepare_mesh(mesh, &format!("lod {l} mesh {m}")))
                .collect();
            prepared.push(meshes);
        }
        if model.lods.is_empty() {
            self.issues.push("model has no LODs".into());
        }
        for p in prepared.iter().flatten() {
            self.issues.extend(p.issues.iter().cloned());
        }
        if let Some(mat) = prepared
            .iter()
            .flatten()
            .filter_map(|p| p.material)
            .find(|&m| m >= model.materials.len())
        {
            self.issues.push(format!(
                "mesh references material {mat} but the model has {} materials",
                model.materials.len()
            ));
        }

        if let Some(gpu) = &self.gpu {
            let mut renderer = gpu.rs.renderer.write();
            if let Some(res) = renderer.callback_resources.get_mut::<RenderResources>() {
                res.clear_model(gpu.view_id);
                let caps = res.pipelines.caps.clone();
                let uploaded: Vec<Vec<Option<GpuMesh>>> = prepared
                    .iter()
                    .enumerate()
                    .map(|(l, lod)| {
                        lod.iter()
                            .enumerate()
                            .map(|(m, p)| {
                                GpuMesh::upload(
                                    &gpu.rs.device,
                                    &caps,
                                    p,
                                    &format!("lod {l} mesh {m}"),
                                )
                            })
                            .collect()
                    })
                    .collect();
                if let Some(view) = res.views.get_mut(&gpu.view_id) {
                    view.meshes = uploaded;
                }
            }
        }

        let lods: Vec<Vec<CpuMesh>> = prepared
            .into_iter()
            .map(|lod| {
                lod.into_iter()
                    .map(|p| CpuMesh {
                        positions: p.vertices.iter().map(|v| v.position).collect(),
                        indices: p.indices,
                        material: p.material,
                        bounds: p.bounds,
                    })
                    .collect()
            })
            .collect();
        self.visible = lods.iter().map(|l| vec![true; l.len()]).collect();
        self.model = Some(CpuModel {
            name: model.name.clone(),
            lods,
            skeleton: model.skeleton.clone(),
            material_count: model.materials.len(),
        });
        self.lod = 0;
        self.frame_pending = true;
    }

    /// Remove the model.
    pub fn clear(&mut self) {
        self.model = None;
        self.visible.clear();
        self.issues.clear();
        self.material_status.clear();
        self.skeleton_cache = None;
        if let Some(gpu) = &self.gpu {
            let mut renderer = gpu.rs.renderer.write();
            if let Some(res) = renderer.callback_resources.get_mut::<RenderResources>() {
                res.clear_model(gpu.view_id);
            }
        }
    }

    /// Use `image` as the base-colour texture of material `material_index`.
    pub fn set_material_texture(
        &mut self,
        material_index: usize,
        image: &Image,
    ) -> Result<(), RenderError> {
        if material_index > MAX_MATERIAL_INDEX {
            return Err(RenderError::MaterialIndex {
                index: material_index,
                max: MAX_MATERIAL_INDEX,
            });
        }
        let gpu = self.gpu.as_ref().ok_or(RenderError::NoGpu)?;
        let result = {
            let mut renderer = gpu.rs.renderer.write();
            match renderer.callback_resources.get_mut::<RenderResources>() {
                Some(res) => res.upload_material(
                    &gpu.rs.device,
                    &gpu.rs.queue,
                    gpu.view_id,
                    material_index,
                    image,
                ),
                None => Err(RenderError::NoGpu),
            }
        };
        match &result {
            Ok(()) => {
                let mut s = format!("{}x{}", image.width, image.height);
                if image.provisional {
                    s.push_str(" (provisional)");
                }
                self.material_status.insert(material_index, s);
            }
            Err(e) => {
                log::warn!("render: material {material_index}: {e}");
                self.material_status
                    .insert(material_index, format!("failed: {e}"));
            }
        }
        result
    }

    fn lod_bounds(&self, only_visible: bool) -> Option<Aabb> {
        let model = self.model.as_ref()?;
        let lod = model.lods.get(self.lod)?;
        lod.iter()
            .enumerate()
            .filter(|(i, _)| !only_visible || self.is_visible(self.lod, *i))
            .filter_map(|(_, m)| m.bounds)
            .reduce(Aabb::union)
    }

    fn is_visible(&self, lod: usize, mesh: usize) -> bool {
        self.visible
            .get(lod)
            .and_then(|l| l.get(mesh))
            .copied()
            .unwrap_or(false)
    }

    pub fn show(&mut self, ui: &mut Ui) -> Response {
        ui.vertical(|ui| {
            self.toolbar(ui);
            let avail = ui.available_size();
            let side_w = if self.show_side_panel {
                (avail.x * 0.3).clamp(160.0, 280.0).min(avail.x * 0.5)
            } else {
                0.0
            };
            ui.horizontal_top(|ui| {
                let spacing = if side_w > 0.0 {
                    ui.spacing().item_spacing.x
                } else {
                    0.0
                };
                let size = vec2((avail.x - side_w - spacing).max(32.0), avail.y.max(32.0));
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
                self.viewport(ui, rect, &resp);
                if side_w > 0.0 {
                    ui.vertical(|ui| {
                        ui.set_width(side_w);
                        ui.set_max_height(avail.y);
                        egui::ScrollArea::vertical()
                            .id_salt("u_rome_model_side")
                            .show(ui, |ui| self.side_panel(ui));
                    });
                }
                resp
            })
            .inner
        })
        .inner
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("u_rome_shading")
                .selected_text(self.mode.label())
                .show_ui(ui, |ui| {
                    for m in ShadingMode::ALL {
                        ui.selectable_value(&mut self.mode, m, m.label());
                    }
                });
            let lod_count = self.model.as_ref().map_or(0, |m| m.lods.len());
            if lod_count > 0 {
                let before = self.lod;
                egui::ComboBox::from_id_salt("u_rome_lod")
                    .selected_text(format!("LOD {}", self.lod))
                    .show_ui(ui, |ui| {
                        for l in 0..lod_count {
                            ui.selectable_value(&mut self.lod, l, format!("LOD {l}"));
                        }
                    });
                if before != self.lod {
                    self.frame_pending = true;
                }
            }
            ui.checkbox(&mut self.show_grid, "Grid");
            let has_skel = self
                .model
                .as_ref()
                .and_then(|m| m.skeleton.as_ref())
                .is_some_and(|s| !s.bones.is_empty());
            ui.add_enabled(
                has_skel,
                egui::Checkbox::new(&mut self.show_skeleton, "Skeleton"),
            );
            if ui.button("Frame (F)").clicked() {
                self.frame_pending = true;
            }
            ui.toggle_value(&mut self.show_side_panel, "Details");
        });
    }

    fn handle_input(&mut self, ui: &Ui, rect: Rect, resp: &Response) {
        let delta = resp.drag_delta();
        let d = Vec2::new(delta.x, delta.y);
        if resp.dragged_by(egui::PointerButton::Primary) {
            self.camera.orbit(d);
        } else if resp.dragged_by(egui::PointerButton::Secondary)
            || resp.dragged_by(egui::PointerButton::Middle)
        {
            self.camera.pan(d, rect.height());
        }
        if resp.hovered() {
            let (scroll, pinch, f) = ui.input(|i| {
                (
                    i.smooth_scroll_delta.y,
                    i.zoom_delta(),
                    i.key_pressed(egui::Key::F),
                )
            });
            if scroll != 0.0 {
                self.camera.zoom(scroll);
            }
            if pinch != 1.0 && pinch.is_finite() && pinch > 0.0 {
                self.camera.distance /= pinch;
            }
            if f {
                self.frame_pending = true;
            }
        }
        if resp.double_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.pick_pivot(rect, p);
        }
    }

    fn pick_pivot(&mut self, rect: Rect, p: Pos2) {
        let Some(model) = &self.model else { return };
        let Some(lod) = model.lods.get(self.lod) else {
            return;
        };
        let radius = self.lod_bounds(true).map_or(1.0, |b| b.radius());
        let ndc = screen_to_ndc(
            Vec2::new(p.x, p.y),
            Vec2::new(rect.min.x, rect.min.y),
            Vec2::new(rect.width(), rect.height()),
        );
        let Some(ray) =
            self.camera
                .ray_from_ndc(ndc, rect.width() / rect.height().max(1.0), radius)
        else {
            return;
        };
        let hit = lod
            .iter()
            .enumerate()
            .filter(|(i, _)| self.is_visible(self.lod, *i))
            .filter_map(|(i, m)| ray_mesh(&ray, &m.positions, &m.indices).map(|t| (t, i)))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((t, mesh)) = hit {
            let point = ray.at(t);
            log::debug!("render: pivot set on mesh {mesh} at {point}");
            let eye = self.camera.eye();
            self.camera.target = point;
            self.camera.distance = (eye - point).length().max(1e-3);
        }
    }

    fn viewport(&mut self, ui: &mut Ui, rect: Rect, resp: &Response) {
        self.handle_input(ui, rect, resp);
        let aspect = rect.width() / rect.height().max(1.0);
        if self.frame_pending && rect.width() > 1.0 {
            let b = self.lod_bounds(true).or_else(|| self.lod_bounds(false));
            self.camera.frame_bounds(
                b.unwrap_or(Aabb {
                    min: Vec3::splat(-1.0),
                    max: Vec3::splat(1.0),
                }),
                aspect,
            );
            self.frame_pending = false;
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(28, 29, 32));
        if self.gpu.is_none() {
            let msg = self
                .gpu_message
                .as_deref()
                .unwrap_or("3D viewport unavailable");
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(15.0),
                Color32::from_rgb(240, 170, 90),
            );
            return;
        }
        if !ui.is_rect_visible(rect) {
            return;
        }
        self.ensure_wireframes();
        let callback = self.build_callback(ui, rect, aspect);
        let Some(gpu) = self.gpu.as_mut() else { return };
        let ppp = ui.ctx().pixels_per_point();
        let want = [
            ((rect.width() * ppp).round() as u32).clamp(1, gpu.caps.max_texture_dim),
            ((rect.height() * ppp).round() as u32).clamp(1, gpu.caps.max_texture_dim),
        ];
        if let Err(e) = ensure_targets(gpu, want) {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("Viewport target creation failed: {e}"),
                egui::FontId::proportional(14.0),
                Color32::from_rgb(240, 120, 100),
            );
            return;
        }
        let Some(tex) = gpu.texture_id else { return };
        let mut callback = callback;
        callback.viewport = want;
        painter.add(egui_wgpu::Callback::new_paint_callback(rect, callback));
        let uv = Rect::from_min_max(
            Pos2::ZERO,
            pos2(
                want[0] as f32 / gpu.target_size[0].max(1) as f32,
                want[1] as f32 / gpu.target_size[1].max(1) as f32,
            ),
        );
        painter.image(tex, rect, uv, Color32::WHITE);

        self.draw_gizmo(&painter, rect);
        self.draw_overlay_stats(&painter, rect);
    }

    /// Lazily build barycentric wireframe buffers for the current LOD when
    /// native line rasterisation is unavailable.
    fn ensure_wireframes(&mut self) {
        if self.mode != ShadingMode::Wireframe {
            return;
        }
        let (Some(gpu), Some(model)) = (&self.gpu, &self.model) else {
            return;
        };
        if gpu.caps.native_wireframe {
            return;
        }
        let Some(lod) = model.lods.get(self.lod) else {
            return;
        };
        let mut renderer = gpu.rs.renderer.write();
        let Some(res) = renderer.callback_resources.get_mut::<RenderResources>() else {
            return;
        };
        let caps = res.pipelines.caps.clone();
        let Some(view) = res.views.get_mut(&gpu.view_id) else {
            return;
        };
        let Some(gpu_lod) = view.meshes.get_mut(self.lod) else {
            return;
        };
        for (i, (cpu, slot)) in lod.iter().zip(gpu_lod.iter_mut()).enumerate() {
            let Some(gm) = slot else { continue };
            if gm.wire_attempted() {
                continue;
            }
            match expand_wireframe(&cpu.positions, &cpu.indices) {
                Some(verts) => gm.set_wire(&gpu.rs.device, &caps, &verts),
                None => {
                    let msg = format!(
                        "lod {} mesh {i}: more than {MAX_WIRE_TRIANGLES} triangles, barycentric wireframe skipped (enable POLYGON_MODE_LINE for native wireframe)",
                        self.lod
                    );
                    log::warn!("render: {msg}");
                    if !self.issues.contains(&msg) {
                        self.issues.push(msg);
                    }
                    // Mark as attempted so we do not retry every frame.
                    gm.set_wire(&gpu.rs.device, &caps, &[]);
                }
            }
        }
    }

    fn build_callback(&mut self, _ui: &Ui, _rect: Rect, aspect: f32) -> SceneCallback {
        let bounds = self.lod_bounds(true).or_else(|| self.lod_bounds(false));
        let radius = bounds.map_or(1.0, |b| b.radius()).max(1e-3);
        let cam = &self.camera;
        let light =
            (-cam.forward() * 0.7 + cam.up() * 0.6 - cam.right() * 0.4).normalize_or(Vec3::Z);
        let uniform = FrameUniform {
            view_proj: cam.view_proj(aspect, radius).to_cols_array_2d(),
            camera_pos: cam.eye().extend(1.0).to_array(),
            light_dir: light.extend(0.0).to_array(),
            wire_color: [0.82, 0.88, 1.0, 1.0],
            params: [self.mode.shader_id(), 0, 0, 0],
        };

        let mut lines = if self.show_grid {
            // Grid sized for the model but anchored at the world origin.
            let reach = bounds.map_or(radius, |b| b.center().truncate().length() + b.radius());
            grid_lines(reach.max(radius))
        } else {
            Vec::new()
        };
        let overlay_start = lines.len() as u32;
        if self.show_skeleton
            && let Some(skel) = self.model.as_ref().and_then(|m| m.skeleton.as_ref())
        {
            let source = self.skeleton_source;
            let cached = matches!(&self.skeleton_cache, Some((s, _)) if *s == source);
            if !cached {
                let l = skeleton_lines(skel, source, radius * 0.01, [255, 190, 40, 255]);
                self.skeleton_cache = Some((source, l));
            }
            if let Some((_, l)) = &self.skeleton_cache {
                lines.extend_from_slice(l);
            }
        }

        let draws = self
            .model
            .as_ref()
            .and_then(|m| m.lods.get(self.lod))
            .map(|lod| {
                (0..lod.len())
                    .filter(|&i| self.is_visible(self.lod, i))
                    .map(|i| (self.lod, i))
                    .collect()
            })
            .unwrap_or_default();

        SceneCallback {
            view_id: self.gpu.as_ref().map_or(0, |g| g.view_id),
            uniform,
            lines,
            overlay_start,
            draws,
            wireframe: self.mode == ShadingMode::Wireframe,
            clear: wgpu::Color {
                r: 28.0 / 255.0,
                g: 29.0 / 255.0,
                b: 32.0 / 255.0,
                a: 1.0,
            },
            viewport: [1, 1],
        }
    }

    fn draw_gizmo(&self, painter: &egui::Painter, rect: Rect) {
        let center = pos2(rect.left() + 42.0, rect.bottom() - 42.0);
        let colors = [
            Color32::from_rgb(230, 80, 80),
            Color32::from_rgb(90, 200, 90),
            Color32::from_rgb(90, 140, 240),
        ];
        painter.circle_filled(center, 34.0, Color32::from_black_alpha(90));
        for (axis, dir, _) in gizmo_axes(&self.camera) {
            let end = center + vec2(dir.x, dir.y) * 26.0;
            let color = colors.get(axis).copied().unwrap_or(Color32::WHITE);
            painter.line_segment([center, end], Stroke::new(2.0, color));
            painter.text(
                end + vec2(dir.x, dir.y) * 7.0,
                egui::Align2::CENTER_CENTER,
                ["X", "Y", "Z"].get(axis).copied().unwrap_or("?"),
                egui::FontId::proportional(11.0),
                color,
            );
        }
    }

    fn stats(&self) -> (usize, usize, usize, usize, usize) {
        let Some(lod) = self.model.as_ref().and_then(|m| m.lods.get(self.lod)) else {
            return (0, 0, 0, 0, 0);
        };
        let mut s = (0, lod.len(), 0, 0, 0);
        for (i, m) in lod.iter().enumerate() {
            if self.is_visible(self.lod, i) {
                s.0 += 1;
                s.2 += m.positions.len();
                s.3 += m.indices.len() / 3;
            }
            s.4 += m.indices.len() / 3;
        }
        s
    }

    fn draw_overlay_stats(&self, painter: &egui::Painter, rect: Rect) {
        let (vis, total, verts, tris, _) = self.stats();
        let text = format!(
            "LOD {}  meshes {vis}/{total}  vertices {verts}  triangles {tris}",
            self.lod
        );
        painter.text(
            rect.left_top() + vec2(8.0, 6.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::monospace(11.0),
            Color32::from_gray(200),
        );
    }

    fn side_panel(&mut self, ui: &mut Ui) {
        let Some(model) = &self.model else {
            ui.label("No model loaded");
            self.renderer_info(ui);
            return;
        };
        ui.strong(model.name.as_deref().unwrap_or("Unnamed model"));
        let (vis, total, verts, tris, all_tris) = self.stats();
        egui::Grid::new("u_rome_model_stats")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("LODs");
                ui.label(model.lods.len().to_string());
                ui.end_row();
                ui.label("Meshes");
                ui.label(format!("{vis} / {total}"));
                ui.end_row();
                ui.label("Vertices");
                ui.label(verts.to_string());
                ui.end_row();
                ui.label("Triangles");
                ui.label(format!("{tris} / {all_tris}"));
                ui.end_row();
                ui.label("Materials");
                ui.label(model.material_count.to_string());
                ui.end_row();
            });

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Meshes");
            if ui.small_button("All").clicked()
                && let Some(v) = self.visible.get_mut(self.lod)
            {
                v.fill(true);
            }
            if ui.small_button("None").clicked()
                && let Some(v) = self.visible.get_mut(self.lod)
            {
                v.fill(false);
            }
        });
        let lod_meshes = model.lods.get(self.lod).map_or(0, |l| l.len());
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt("u_rome_mesh_list")
            .max_height(220.0)
            .show_rows(ui, row_h, lod_meshes, |ui, range| {
                for i in range {
                    let Some(m) = model.lods.get(self.lod).and_then(|l| l.get(i)) else {
                        continue;
                    };
                    let mat = m
                        .material
                        .map_or_else(|| "-".to_string(), |x| x.to_string());
                    let label = format!(
                        "Mesh {i}  mat {mat}  {}v {}t",
                        m.positions.len(),
                        m.indices.len() / 3
                    );
                    if let Some(flag) = self.visible.get_mut(self.lod).and_then(|v| v.get_mut(i)) {
                        ui.checkbox(flag, label);
                    }
                }
            });

        if let Some(skel) = &model.skeleton {
            ui.separator();
            ui.horizontal(|ui| {
                ui.strong(format!("Skeleton: {} bones", skel.bones.len()));
                if SKELETON_PROVISIONAL {
                    crate::image_view::provisional_badge(ui).on_hover_text(
                        "Bone rotation order assumed [x, y, z, w]; unverified for GTA VI.",
                    );
                }
            });
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut self.skeleton_source,
                    SkeletonSource::LocalTransforms,
                    "Local TRS",
                );
                ui.radio_value(
                    &mut self.skeleton_source,
                    SkeletonSource::InverseBind,
                    "Inverse bind",
                );
            });
        }

        if !self.material_status.is_empty() {
            ui.separator();
            ui.strong("Material textures");
            for (i, s) in &self.material_status {
                ui.label(format!("{i}: {s}"));
            }
        }

        if !self.issues.is_empty() {
            ui.separator();
            egui::CollapsingHeader::new(
                RichText::new(format!("Issues ({})", self.issues.len()))
                    .color(Color32::from_rgb(240, 170, 90)),
            )
            .id_salt("u_rome_model_issues")
            .show(ui, |ui| {
                for s in self.issues.iter().take(200) {
                    ui.label(RichText::new(s).small());
                }
                if self.issues.len() > 200 {
                    ui.label(format!("... {} more", self.issues.len() - 200));
                }
            });
        }
        self.renderer_info(ui);
    }

    fn renderer_info(&self, ui: &mut Ui) {
        ui.separator();
        match &self.gpu {
            Some(g) => {
                ui.label(
                    RichText::new(format!(
                        "{:?} - {}\nMSAA {}x, wireframe {}",
                        g.caps.backend,
                        g.caps.adapter_name,
                        g.caps.samples,
                        if g.caps.native_wireframe {
                            "native lines"
                        } else {
                            "barycentric"
                        }
                    ))
                    .small()
                    .weak(),
                );
            }
            None => {
                ui.label(RichText::new("No wgpu renderer").small().weak());
            }
        }
    }
}

/// Size offscreen targets with headroom (multiples of 128 px) so window
/// resizes do not recreate textures every frame.
pub(crate) fn target_alloc_size(want: [u32; 2], current: [u32; 2], max_dim: u32) -> [u32; 2] {
    let fits = want[0] <= current[0]
        && want[1] <= current[1]
        && current[0] <= want[0].saturating_mul(2).max(256)
        && current[1] <= want[1].saturating_mul(2).max(256);
    if fits {
        return current;
    }
    want.map(|w| {
        w.max(1)
            .div_ceil(128)
            .saturating_mul(128)
            .min(max_dim.max(1))
    })
}

fn ensure_targets(gpu: &mut Gpu, want: [u32; 2]) -> Result<(), String> {
    let size = target_alloc_size(want, gpu.target_size, gpu.caps.max_texture_dim);
    if size == gpu.target_size && gpu.texture_id.is_some() {
        return Ok(());
    }
    let targets = Targets::new(&gpu.rs.device, size, gpu.caps.samples)?;
    let mut renderer = gpu.rs.renderer.write();
    let id = match gpu.texture_id {
        Some(id) => {
            renderer.update_egui_texture_from_wgpu_texture(
                &gpu.rs.device,
                &targets.color_view,
                wgpu::FilterMode::Linear,
                id,
            );
            id
        }
        None => renderer.register_native_texture(
            &gpu.rs.device,
            &targets.color_view,
            wgpu::FilterMode::Linear,
        ),
    };
    gpu.texture_id = Some(id);
    gpu.target_size = size;
    if let Some(view) = renderer
        .callback_resources
        .get_mut::<RenderResources>()
        .and_then(|r| r.views.get_mut(&gpu.view_id))
    {
        view.targets = Some(targets);
    }
    log::debug!("render: viewport targets {}x{}", size[0], size[1]);
    Ok(())
}

impl Drop for ModelView {
    fn drop(&mut self) {
        if let Some(gpu) = &self.gpu {
            let mut renderer = gpu.rs.renderer.write();
            if let Some(res) = renderer.callback_resources.get_mut::<RenderResources>() {
                res.views.remove(&gpu.view_id);
            }
            if let Some(id) = gpu.texture_id {
                renderer.free_texture(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::{Mesh, ModelLod};

    #[test]
    fn target_sizes_have_headroom() {
        assert_eq!(target_alloc_size([300, 200], [0, 0], 8192), [384, 256]);
        assert_eq!(target_alloc_size([310, 210], [384, 256], 8192), [384, 256]);
        assert_eq!(target_alloc_size([400, 210], [384, 256], 8192), [512, 256]);
        assert_eq!(target_alloc_size([9000, 10], [0, 0], 2048), [2048, 128]);
        // Shrinks when far smaller than the allocation.
        assert_eq!(
            target_alloc_size([100, 100], [1024, 1024], 8192),
            [128, 128]
        );
    }

    fn model() -> Model {
        let mesh = Mesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            indices: vec![0, 1, 2, 0, 1, 50],
            material: Some(3),
            ..Default::default()
        };
        Model {
            name: Some("test".into()),
            lods: vec![ModelLod {
                meshes: vec![mesh.clone(), Mesh::default()],
            }],
            materials: vec![],
            skeleton: None,
        }
    }

    #[test]
    fn without_gpu_model_still_loads() {
        let mut v = ModelView::new(None);
        v.set_model(&model());
        assert_eq!(v.stats(), (2, 2, 3, 1, 1));
        // Bad triangle, empty mesh, missing material.
        assert_eq!(v.issues().len(), 3);
        assert!(matches!(
            v.set_material_texture(
                0,
                &Image {
                    width: 1,
                    height: 1,
                    format: u_rome_core::ImageFormat::Rgba8,
                    mips: vec![vec![0; 4]],
                    provisional: false,
                }
            ),
            Err(RenderError::NoGpu)
        ));
        assert!(matches!(
            v.set_material_texture(
                usize::MAX,
                &Image {
                    width: 1,
                    height: 1,
                    format: u_rome_core::ImageFormat::Rgba8,
                    mips: vec![],
                    provisional: false,
                }
            ),
            Err(RenderError::MaterialIndex { .. })
        ));
        v.set_lod(10);
        assert_eq!(v.lod(), 0);
        let b = v.lod_bounds(true).unwrap();
        assert_eq!(b.max, Vec3::new(1.0, 1.0, 0.0));
    }

    #[test]
    fn widget_shows_fallback_without_gpu() {
        let ctx = egui::Context::default();
        let mut v = ModelView::new(None);
        v.set_model(&model());
        let mut iv = crate::ImageView::new();
        iv.set_image(&Image {
            width: 2,
            height: 2,
            format: u_rome_core::ImageFormat::Rgba8Srgb,
            mips: vec![vec![255; 16], vec![1, 2, 3]],
            provisional: true,
        });
        for _ in 0..2 {
            let mut out = ctx.run_ui(Default::default(), |ui| {
                v.show(ui);
                iv.show(ui);
            });
            // No painter backend in tests: drop texture uploads explicitly.
            out.textures_delta.clear();
        }
    }
}
