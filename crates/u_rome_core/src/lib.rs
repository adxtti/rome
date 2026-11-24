//! Shared types used across all u_rome crates.
//!
//! Decoder crates (`u_rome_texture`, `u_rome_model`, ...) produce the plain data
//! types defined here, and consumers (`u_rome_render`, `u_rome_studio`, the CLI)
//! only depend on these types, never on each other's internals.

pub mod asset;
pub mod bytes;
pub mod hash;
pub mod kind;

pub use asset::{Bone, Image, ImageFormat, Material, Mesh, Model, ModelLod, Skeleton};
pub use bytes::{ByteError, Endian, Reader};
pub use hash::{Hash32, joaat, joaat_lower};
pub use kind::{AssetKind, detect};
