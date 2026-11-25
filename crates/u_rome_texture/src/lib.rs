//! TTPF / RDS5 / ETPF texture stores: chunk tables, AMD Gfx10 detiling, BCPack and BCn decoding.

pub mod bcn;
pub mod error;
pub mod export;
pub mod format;
pub mod gfx10;

pub use bcn::decode_bcn;
pub use error::{Error, Result};
pub use export::{RawTexture, export_dds, export_dds_raw, export_png};
pub use format::{Codec, DxgiFormat, FormatLayout, PixelKind};
pub use gfx10::{
    MipChain, MipPlacement, Swizzle, TileGeometry, detile, mip_chain, tile, tiled_size,
};
