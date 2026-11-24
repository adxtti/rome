//! Decoded asset data shared between decoders, the renderer and exporters.
//!
//! Coordinates are RAGE convention: right-handed, Z-up, metres. Exporters
//! convert (glTF is Y-up: `gltf(x, y, z) = rage(x, z, -y)`).

use crate::hash::Hash32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// 8-bit RGBA, straight alpha.
    Rgba8,
    /// 8-bit RGBA, sRGB-encoded colour.
    Rgba8Srgb,
}

/// A decoded image with its mip chain, largest first.
#[derive(Clone, Debug)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    /// `mips[0]` is full size; each level is `width * height * 4` bytes for that level.
    pub mips: Vec<Vec<u8>>,
    /// True when part of the decode is approximate (e.g. fitted BCPack tables).
    pub provisional: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Material {
    /// JOAAT of the lowercase shader name.
    pub shader: Hash32,
    pub shader_name: Option<String>,
    /// Texture references by 64-bit hash (key into a texture store).
    pub textures: Vec<(Hash32, u64)>,
}

/// One drawable surface.
#[derive(Clone, Debug, Default)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub tangents: Vec<[f32; 4]>,
    pub uvs: Vec<[f32; 2]>,
    pub colors: Vec<[u8; 4]>,
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
    pub material: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelLod {
    pub meshes: Vec<Mesh>,
}

#[derive(Clone, Debug)]
pub struct Bone {
    pub name: Option<String>,
    pub name_hash: Hash32,
    pub parent: Option<u16>,
    pub rotation: [f32; 4],
    pub translation: [f32; 3],
    pub scale: [f32; 3],
    /// Inverse bind matrix, row-vector convention (`p_bone = p_model * M`).
    pub inverse_bind: [[f32; 4]; 4],
}

#[derive(Clone, Debug, Default)]
pub struct Skeleton {
    pub bones: Vec<Bone>,
}

/// A decoded model. `lods[0]` is the highest detail.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub name: Option<String>,
    pub lods: Vec<ModelLod>,
    pub materials: Vec<Material>,
    pub skeleton: Option<Skeleton>,
}

impl Model {
    /// Axis-aligned bounds of LOD 0, if it has any vertices.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let mut it = self
            .lods
            .first()?
            .meshes
            .iter()
            .flat_map(|m| m.positions.iter());
        let first = *it.next()?;
        Some(it.fold((first, first), |(mut lo, mut hi), p| {
            for i in 0..3 {
                lo[i] = lo[i].min(p[i]);
                hi[i] = hi[i].max(p[i]);
            }
            (lo, hi)
        }))
    }
}
