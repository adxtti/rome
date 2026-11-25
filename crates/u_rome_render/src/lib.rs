//! Rendering widgets for ROME Studio.
//!
//! - [`ModelView`]: 3D model viewport (wgpu through `egui_wgpu` callbacks).
//! - [`ImageView`]: 2D image viewer (pure egui).
//!
//! GPU-free math (camera, framing, picking, mesh preparation) lives in
//! [`camera`] and [`prep`] and is unit-tested without a GPU.
//!
//! # Integration notes
//!
//! - Create `ModelView::new(cc.wgpu_render_state.as_ref())` once (for
//!   example in the app constructor) and keep it; it owns GPU state keyed in
//!   `egui_wgpu::CallbackResources` and frees it on drop. With the glow
//!   renderer pass `None`: the widget shows a "needs the wgpu renderer"
//!   message, still lists meshes and stats.
//! - Native line wireframe needs `wgpu::Features::POLYGON_MODE_LINE` on the
//!   device. eframe's default device descriptor does not request it; request
//!   it in `WgpuConfiguration` when `adapter.features()` has it. Otherwise
//!   (always on GL) a barycentric shader on expanded vertices is used.
//! - MSAA 4x is used when the adapter supports it for both colour and depth;
//!   set `U_ROME_NO_MSAA=1` to force it off (useful on flaky GL drivers).
//! - All shaders stay within WebGL2-class downlevel limits: uniform buffers
//!   only, no storage buffers, no compute, u32 indices below 2^24.

pub mod camera;
pub mod error;
mod gpu;
#[cfg(test)]
mod gpu_smoke;
pub mod image_view;
pub mod model_view;
pub mod prep;

pub use camera::{Aabb, OrbitCamera, Ray};
pub use error::RenderError;
pub use gpu::Capabilities;
pub use image_view::{Channels, ImageView};
pub use model_view::{ModelView, ShadingMode};
pub use prep::SkeletonSource;

/// Re-exported so hosts use exactly the egui/wgpu versions this crate was built against.
pub use egui;
pub use egui_wgpu;
