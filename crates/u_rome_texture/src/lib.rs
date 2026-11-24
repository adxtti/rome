//! TTPF / RDS5 / ETPF texture stores: chunk tables, AMD Gfx10 detiling, BCPack and BCn decoding.

pub mod error;
pub mod format;
pub mod gfx10;

pub use error::{Error, Result};
pub use format::{Codec, DxgiFormat, FormatLayout, PixelKind};
pub use gfx10::{
    MipChain, MipPlacement, Swizzle, TileGeometry, detile, mip_chain, tile, tiled_size,
};
