//! TTPF / RDS5 / ETPF texture stores: chunk tables, AMD Gfx10 detiling, BCPack and BCn decoding.
//!
//! GTA VI (Xbox Series build 1.0.2.1) keeps most textures in `.ttpf` files:
//! an `RDS5` chunk store whose `ETPF` table describes each texture. Chunks are
//! stored, zlib or BCPack (the Xbox Series MDU rANS layer over BCn blocks);
//! texture data is AMD Gfx10 tiled.
//!
//! # Usage
//!
//! ```no_run
//! use u_rome_texture::{MipSelection, TextureStore, export_png};
//!
//! # fn main() -> u_rome_texture::Result<()> {
//! let bytes = std::fs::read("vehicles.ttpf").unwrap();
//! let store = TextureStore::parse(&bytes)?;
//! for (i, t) in store.textures().iter().enumerate() {
//!     println!("{:016x} {}x{} {} mips {}", t.hash, t.width, t.height, t.format, t.mip_count);
//!     match store.decode(&bytes, i, MipSelection::PreferPlain) {
//!         Ok(image) => {
//!             let png = export_png(&image, 0)?;
//!             // image.provisional: BCPack or a disputed field was involved.
//!         }
//!         // Error::Unsupported { codec, format } for BCPack BC2-BC7 and unknown formats.
//!         Err(e) => eprintln!("texture {i}: {e}"),
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # API
//!
//! * [`TextureStore::parse`] / [`TextureStore::parse_with`]: read an `RDS5`
//!   store or a bare `ETPF` table (metadata only; keep the bytes for decoding).
//! * [`TextureStore::textures`]: [`TextureInfo`] per texture (hash, size, mip
//!   count, [`DxgiFormat`], array size, tile mode and [`Swizzle`], per-mip
//!   [`MipInfo`] with chunk [`Codec`]s, `provisional` and `notes`).
//! * [`TextureStore::decode`] / [`TextureStore::decode_with`]: RGBA8
//!   [`u_rome_core::Image`]; [`TextureStore::decode_raw`]: detiled BCn data.
//! * [`export_png`], [`export_dds`] (RGBA8), [`export_dds_raw`] (lossless BCn).
//! * Helpers: [`decode_bcn`], [`detile`], [`tile`], [`mip_chain`].
//!
//! # Provisional output
//!
//! Sources disagree on several fields; each disagreement is documented where
//! it is handled ([`store`], [`gfx10`]) and marks affected textures
//! `provisional` with a note. BCPack BC1 decoding uses bcpack-rs's fitted
//! tables and is always provisional; [`MipSelection::PreferPlain`] (the
//! default) skips to the largest mip stored as exact zlib or stored data.
//!
//! # Credits
//!
//! * [bcpack-rs](https://github.com/Ahmet-Dedeler/bcpack-rs) by Ahmet Dedeler
//!   (MIT OR Apache-2.0): BCPack BC1 decoder (a dependency) and the RDS5 /
//!   Gfx10 mip-tail logic adapted in [`gfx10`] and [`store`].
//! * Format facts: ttpf-studio FORMATS.md (MIT), SentinelInteractive
//!   `ttpf-rds5-etpf.md` (MIT), GTA6-Asset-Info `formats/05-06` (facts only).
//! * [bcdec_rs](https://crates.io/crates/bcdec_rs) (MIT) for BC1-BC7 blocks.

pub mod bcn;
pub mod error;
pub mod export;
pub mod format;
pub mod gfx10;
pub mod store;

pub use bcn::decode_bcn;
pub use error::{Error, Result};
pub use export::{RawTexture, export_dds, export_dds_raw, export_png};
pub use format::{Codec, DxgiFormat, FormatLayout, PixelKind};
pub use gfx10::{
    MipChain, MipPlacement, Swizzle, TileGeometry, detile, mip_chain, tile, tiled_size,
};
pub use store::{
    ChunkInfo, DecodeOptions, IndexLayout, MipInfo, MipSelection, ParseOptions, ResourceKind,
    StoreKind, TextureInfo, TextureStore, TileRule,
};
