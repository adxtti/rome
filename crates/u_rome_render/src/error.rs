//! Error type for the render crate.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RenderError {
    #[error("image has invalid dimensions {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },

    #[error("image mip {level} missing (image has {count} mips)")]
    MissingMip { level: usize, count: usize },

    #[error(
        "image mip {level} ({width}x{height}): expected {expected} bytes of RGBA8, got {actual}"
    )]
    MipSize {
        level: usize,
        width: u32,
        height: u32,
        expected: u64,
        actual: usize,
    },

    #[error("texture {width}x{height} exceeds the device limit of {limit} at every mip level")]
    TextureTooLarge { width: u32, height: u32, limit: u32 },

    #[error("material index {index} is above the supported maximum {max}")]
    MaterialIndex { index: usize, max: usize },

    #[error("3D viewport needs the wgpu renderer (no wgpu render state available)")]
    NoGpu,
}
