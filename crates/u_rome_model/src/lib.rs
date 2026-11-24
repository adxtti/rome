//! Model decoding: quantised vertex streams, meshlet indices, LOD groups, skeletons, glTF export.

pub mod error;
pub mod index;
pub mod vertex;

#[cfg(test)]
mod testutil;

pub use error::{ModelError, Result};
pub use index::{IndexBuffer, decode_index_buffer, decode_index_buffer_detailed};
pub use vertex::{VertexData, decode_vertex_buffer};
