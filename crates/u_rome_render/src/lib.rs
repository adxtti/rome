//! Rendering widgets for ROME Studio.
//!
//! - [`ModelView`]: 3D model viewport (wgpu through `egui_wgpu` callbacks).
//! - [`ImageView`]: 2D image viewer (pure egui).
//!
//! GPU-free math (camera, framing, picking, mesh preparation) lives in
//! [`camera`] and [`prep`] and is unit-tested without a GPU.

pub mod camera;
pub mod error;
pub mod image_view;
pub mod prep;

pub use camera::{Aabb, OrbitCamera, Ray};
pub use error::RenderError;
pub use image_view::{Channels, ImageView};
