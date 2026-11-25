//! Rendering widgets for ROME Studio.
//!
//! - [`ModelView`]: 3D model viewport (wgpu through `egui_wgpu` callbacks).
//! - [`ImageView`]: 2D image viewer (pure egui).
//!
//! GPU-free math (camera, framing, picking, mesh preparation) lives in
//! [`camera`] and [`prep`] and is unit-tested without a GPU.

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
