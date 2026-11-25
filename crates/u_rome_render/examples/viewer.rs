//! Standalone demo of the render widgets with procedurally generated data.
//!
//! ```text
//! cargo run -p u_rome_render --example viewer
//! cargo run -p u_rome_render --example viewer -- --no-gpu   # fallback message
//! cargo run -p u_rome_render --example viewer -- --mode wireframe --skeleton
//! ```
//!
//! `U_ROME_VIEWER_EXIT_AFTER_FRAMES=N` closes the window after N frames
//! (used for smoke testing).

use std::f32::consts::{PI, TAU};

use eframe::egui;
use u_rome_core::{Bone, Image, ImageFormat, Material, Mesh, Model, ModelLod, Skeleton};
use u_rome_render::{ImageView, ModelView, ShadingMode};

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let no_gpu = std::env::args().any(|a| a == "--no-gpu");
    let exit_after = std::env::var("U_ROME_VIEWER_EXIT_AFTER_FRAMES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("u_rome_render viewer")
            .with_inner_size([1280.0, 760.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "u_rome_render viewer",
        options,
        Box::new(move |cc| {
            let rs = if no_gpu {
                None
            } else {
                cc.wgpu_render_state.as_ref()
            };
            Ok(Box::new(Demo::new(rs, exit_after)))
        }),
    )
}

struct Demo {
    model_view: ModelView,
    image_view: ImageView,
    frames: u64,
    exit_after: Option<u64>,
}

impl Demo {
    fn new(rs: Option<&u_rome_render::egui_wgpu::RenderState>, exit_after: Option<u64>) -> Self {
        let mut model_view = ModelView::new(rs);
        model_view.set_model(&demo_model());
        let args: Vec<String> = std::env::args().collect();
        if let Some(mode) = args
            .iter()
            .position(|a| a == "--mode")
            .and_then(|i| args.get(i + 1))
        {
            model_view.mode = match mode.as_str() {
                "normals" => ShadingMode::Normals,
                "uv" => ShadingMode::UvChecker,
                "colors" => ShadingMode::VertexColors,
                "wireframe" => ShadingMode::Wireframe,
                _ => ShadingMode::Lit,
            };
        }
        model_view.show_skeleton = args.iter().any(|a| a == "--skeleton");
        if let Err(e) = model_view.set_material_texture(0, &checker_texture(256)) {
            log::warn!("material texture not set: {e}");
        }
        let mut image_view = ImageView::new();
        image_view.set_image(&gradient_image(512, 256));
        Self {
            model_view,
            image_view,
            frames: 0,
            exit_after,
        }
    }
}

impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::right("image_panel")
            .resizable(true)
            .default_size(460.0)
            .show(ui, |ui| {
                ui.heading("ImageView");
                self.image_view.show(ui);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("ModelView");
            self.model_view.show(ui);
        });
        self.frames += 1;
        if let Some(n) = self.exit_after {
            ui.ctx().request_repaint();
            if self.frames >= n {
                log::info!("viewer: exiting after {} frames", self.frames);
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Procedural content
// ---------------------------------------------------------------------------

fn demo_model() -> Model {
    Model {
        name: Some("demo: skinned column + sphere".into()),
        lods: vec![
            ModelLod {
                meshes: vec![skinned_column(8), sphere([3.0, 0.0, 1.5], 1.0, 48, 24)],
            },
            ModelLod {
                meshes: vec![skinned_column(2), sphere([3.0, 0.0, 1.5], 1.0, 12, 6)],
            },
        ],
        materials: vec![
            Material {
                shader_name: Some("default".into()),
                ..Default::default()
            },
            Material::default(),
        ],
        skeleton: Some(demo_skeleton()),
    }
}

/// A 1 x 1 x 4 column standing on the origin, built from per-face quads with
/// `segments` rows along Z, skinned to three bones by height.
fn skinned_column(segments: u32) -> Mesh {
    let mut m = Mesh {
        material: Some(0),
        ..Default::default()
    };
    let (hx, hy, h) = (0.5f32, 0.5f32, 4.0f32);
    // (origin, u axis, v axis, normal) for each face.
    type Face = ([f32; 3], [f32; 3], [f32; 3], [f32; 3]);
    let faces: [Face; 6] = [
        (
            [-hx, -hy, 0.0],
            [2.0 * hx, 0.0, 0.0],
            [0.0, 0.0, h],
            [0.0, -1.0, 0.0],
        ),
        (
            [hx, hy, 0.0],
            [-2.0 * hx, 0.0, 0.0],
            [0.0, 0.0, h],
            [0.0, 1.0, 0.0],
        ),
        (
            [hx, -hy, 0.0],
            [0.0, 2.0 * hy, 0.0],
            [0.0, 0.0, h],
            [1.0, 0.0, 0.0],
        ),
        (
            [-hx, hy, 0.0],
            [0.0, -2.0 * hy, 0.0],
            [0.0, 0.0, h],
            [-1.0, 0.0, 0.0],
        ),
        (
            [-hx, hy, h],
            [2.0 * hx, 0.0, 0.0],
            [0.0, -2.0 * hy, 0.0],
            [0.0, 0.0, 1.0],
        ),
        (
            [-hx, -hy, 0.0],
            [2.0 * hx, 0.0, 0.0],
            [0.0, 2.0 * hy, 0.0],
            [0.0, 0.0, -1.0],
        ),
    ];
    for (o, u, v, n) in faces {
        let rows = if n[2] == 0.0 { segments } else { 1 };
        let base = m.positions.len() as u32;
        for j in 0..=rows {
            for i in 0..=1u32 {
                let (s, t) = (i as f32, j as f32 / rows as f32);
                let p = [
                    o[0] + u[0] * s + v[0] * t,
                    o[1] + u[1] * s + v[1] * t,
                    o[2] + u[2] * s + v[2] * t,
                ];
                m.positions.push(p);
                m.normals.push(n);
                m.uvs.push([s, 1.0 - t]);
                let z = (p[2] / h).clamp(0.0, 1.0);
                m.colors
                    .push([(z * 255.0) as u8, 120, (255.0 - z * 255.0) as u8, 255]);
                // Blend between bone 0 (base), 1 (middle) and 2 (top).
                let (j0, j1, w) = if z < 0.5 {
                    (0, 1, z * 2.0)
                } else {
                    (1, 2, (z - 0.5) * 2.0)
                };
                m.joints.push([j0, j1, 0, 0]);
                m.weights.push([1.0 - w, w, 0.0, 0.0]);
            }
        }
        for j in 0..rows {
            let a = base + j * 2;
            m.indices
                .extend_from_slice(&[a, a + 1, a + 3, a, a + 3, a + 2]);
        }
    }
    m
}

fn sphere(center: [f32; 3], r: f32, slices: u32, stacks: u32) -> Mesh {
    let mut m = Mesh {
        material: Some(1),
        ..Default::default()
    };
    for j in 0..=stacks {
        let v = j as f32 / stacks as f32;
        let phi = v * PI;
        for i in 0..=slices {
            let u = i as f32 / slices as f32;
            let theta = u * TAU;
            let n = [phi.sin() * theta.cos(), phi.sin() * theta.sin(), phi.cos()];
            m.positions.push([
                center[0] + n[0] * r,
                center[1] + n[1] * r,
                center[2] + n[2] * r,
            ]);
            m.normals.push(n);
            m.uvs.push([u, v]);
            m.colors.push([
                (u * 255.0) as u8,
                (v * 255.0) as u8,
                ((1.0 - u) * 255.0) as u8,
                255,
            ]);
        }
    }
    let row = slices + 1;
    for j in 0..stacks {
        for i in 0..slices {
            let a = j * row + i;
            let b = a + row;
            m.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    m
}

fn demo_skeleton() -> Skeleton {
    let bone = |name: &str, parent: Option<u16>, local_z: f32, world_z: f32| Bone {
        name: Some(name.into()),
        name_hash: u_rome_core::joaat_lower(name),
        parent,
        rotation: [0.0, 0.0, 0.0, 1.0],
        translation: [0.0, 0.0, local_z],
        scale: [1.0; 3],
        // Row-vector inverse bind: translation by -world in the last row.
        inverse_bind: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, -world_z, 1.0],
        ],
    };
    Skeleton {
        bones: vec![
            bone("root", None, 0.0, 0.0),
            bone("spine", Some(0), 2.0, 2.0),
            bone("head", Some(1), 2.0, 4.0),
        ],
    }
}

/// Box-filter mip chain for RGBA8 data.
fn mip_chain(w: u32, h: u32, base: Vec<u8>) -> Vec<Vec<u8>> {
    let mut mips = vec![base];
    let (mut cw, mut ch) = (w, h);
    while cw > 1 || ch > 1 {
        let (nw, nh) = ((cw / 2).max(1), (ch / 2).max(1));
        let prev = mips.last().expect("mip chain starts non-empty");
        let mut next = Vec::with_capacity((nw * nh * 4) as usize);
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut sum = 0u32;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let sx = (x * 2 + dx).min(cw - 1);
                        let sy = (y * 2 + dy).min(ch - 1);
                        sum += u32::from(prev[((sy * cw + sx) * 4 + c) as usize]);
                    }
                    next.push((sum / 4) as u8);
                }
            }
        }
        mips.push(next);
        (cw, ch) = (nw, nh);
    }
    mips
}

fn checker_texture(size: u32) -> Image {
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = ((x / 32) + (y / 32)) % 2 == 0;
            let (r, g, b) = if on {
                (230, 200, 120)
            } else {
                (60 + (x * 120 / size) as u8, 70, 90 + (y * 120 / size) as u8)
            };
            px.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Image {
        width: size,
        height: size,
        format: ImageFormat::Rgba8Srgb,
        mips: mip_chain(size, size, px),
        provisional: false,
    }
}

fn gradient_image(w: u32, h: u32) -> Image {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (x as f32 / (w - 1) as f32, y as f32 / (h - 1) as f32);
            let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt() * 2.0;
            px.extend_from_slice(&[
                (u * 255.0) as u8,
                (v * 255.0) as u8,
                ((1.0 - d).clamp(0.0, 1.0) * 255.0) as u8,
                (((u + v) * 0.5) * 255.0) as u8,
            ]);
        }
    }
    Image {
        width: w,
        height: h,
        format: ImageFormat::Rgba8,
        mips: mip_chain(w, h, px),
        // Exercise the provisional badge.
        provisional: true,
    }
}
