//! Model decoding: quantised vertex streams, meshlet indices, LOD groups, skeletons, glTF export.
//!
//! Decodes GTA VI (Xbox Series build 1.0.2.1) model resources into
//! [`u_rome_core::Model`] and exports glTF 2.0 binary.
//!
//! # API
//!
//! | Function | Certainty | Notes |
//! |---|---|---|
//! | [`decode_vertex_buffer`]`(&[u8]) -> Result<VertexData>` | high | one mesh blob (magic `0x4042`, or `0x3063` for float positions) |
//! | [`decode_index_buffer`]`(&[u8]) -> Result<Vec<u32>>` | high | one meshlet index buffer (descriptor flags `0x1380`) |
//! | [`decode_index_buffer_detailed`] | high | same, with meshlets and drop counts |
//! | [`decode_drawable_pages`]`(ResourcePages) -> Result<DecodedModel>` | best effort | system and graphics pages given separately |
//! | [`decode_drawable`]`(&[u8]) -> Result<DecodedModel>` | best effort | system page followed by graphics page; the split is inferred |
//! | [`export_glb`]`(&Model, lod, &[(material, Image)]) -> Result<Vec<u8>>` | n/a | glTF binary of one LOD |
//! | [`export_glb_with`] | n/a | same with [`GlbOptions`] (colour attribute, skin) |
//!
//! [`DecodedModel`] carries `warnings` (with byte offsets) and a
//! `provisional` flag that is set whenever an inferred layout was used.
//!
//! # Sources
//!
//! Facts come from the public format notes in TrinitronClient/GTA6-Asset-Info
//! (`formats/02-resources.md`, `formats/07-models.md`,
//! `structure/06-models-shaders.md`) and the SixNG README. Both are GPL
//! documentation: only facts were used, no code. Fields taken from the GTA V
//! layout by analogy are marked in the code and set `provisional`.
//!
//! # Coordinates
//!
//! Decoded data stays in RAGE convention (right-handed, Z-up, metres).
//! [`glb`] converts to glTF Y-up: `gltf(x, y, z) = rage(x, z, -y)`.
//!
//! # Robustness
//!
//! Game data is untrusted. Nothing here panics on malformed input; every
//! count read from a header is capped ([`vertex::MAX_VERTICES`],
//! [`index::MAX_TRIANGLES`], [`drawable::MAX_BONES`], ...) and every read is
//! bounds-checked. Problems are logged with `log::warn!` / `log::debug!`.

pub mod drawable;
pub mod error;
pub mod glb;
pub mod index;
pub mod vertex;

#[cfg(test)]
mod testutil;

pub use drawable::{DecodedModel, ResourcePages, decode_drawable, decode_drawable_pages};
pub use error::{ModelError, Result};
pub use glb::{ColorExport, GlbOptions, export_glb, export_glb_with};
pub use index::{IndexBuffer, decode_index_buffer, decode_index_buffer_detailed};
pub use vertex::{VertexData, decode_vertex_buffer};
