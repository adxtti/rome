//! glTF 2.0 binary (.glb) export.
//!
//! Coordinates are converted from RAGE (right-handed, Z-up, metres) to glTF
//! (right-handed, Y-up, metres): `gltf(x, y, z) = rage(x, z, -y)`. This is a
//! proper rotation (-90 degrees about X), so triangle winding is kept.
//!
//! Layout of the written file:
//!
//! * one node + mesh (one triangle primitive) per [`Mesh`] of the chosen LOD,
//!   all at scene root (glTF ignores the transform of skinned mesh nodes);
//! * when the model has a skeleton: one node per bone under an `armature`
//!   node, and, if any mesh is skinned, a skin whose joints are the bones
//!   followed by the armature node (so every joint shares a common root;
//!   vertex joint indices are unchanged);
//! * one glTF material per model material, with the caller's image as an
//!   embedded PNG base colour texture.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use u_rome_core::{Bone, Image, ImageFormat, Mesh, Model};

use crate::error::{ModelError, Result};

/// What to do with the RGBA8 vertex colour stream.
///
/// In GTA VI the stream is not a display colour (blue is a tint palette
/// index, alpha a layer weight), so viewers that multiply base colour by
/// `COLOR_0` will show odd tints. The task contract exports it as `COLOR_0`
/// by default; Studio may prefer [`ColorExport::Custom`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorExport {
    /// Standard `COLOR_0` (normalised unsigned byte VEC4).
    #[default]
    Color0,
    /// Application-specific `_RAGE_COLOR` attribute, ignored by viewers.
    Custom,
    /// Do not export vertex colours.
    None,
}

/// Export options for [`export_glb_with`].
#[derive(Clone, Debug)]
pub struct GlbOptions {
    pub colors: ColorExport,
    /// Write the skin (joints, weights, inverse bind matrices).
    pub skin: bool,
}

impl Default for GlbOptions {
    fn default() -> Self {
        Self {
            colors: ColorExport::Color0,
            skin: true,
        }
    }
}

/// RAGE Z-up to glTF Y-up.
pub fn rage_to_gltf(p: [f32; 3]) -> [f32; 3] {
    [p[0], p[2], -p[1]]
}

/// Convert a RAGE row-vector inverse bind matrix (`p_bone = p_model * M`)
/// to a glTF inverse bind matrix in column-major order, including the axis
/// change.
pub fn inverse_bind_to_gltf(m: &[[f32; 4]; 4]) -> [f32; 16] {
    // Column-vector form A = M^T.
    let mut a = [[0f32; 4]; 4];
    for (r, row) in a.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = m[c][r];
        }
    }
    // B = C * A * C^T with C the axis change.
    const C: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mul = |x: &[[f32; 4]; 4], y: &[[f32; 4]; 4]| {
        let mut o = [[0f32; 4]; 4];
        for (r, row) in o.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                *v = (0..4).map(|k| x[r][k] * y[k][c]).sum();
            }
        }
        o
    };
    let mut ct = [[0f32; 4]; 4];
    for (r, row) in ct.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = C[c][r];
        }
    }
    let b = mul(&mul(&C, &a), &ct);
    let mut out = [0f32; 16];
    for (c, col) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        for (r, v) in col.iter_mut().enumerate() {
            *v = b[r][c];
        }
    }
    out
}

const IDENTITY16: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

const ARRAY_BUFFER: u32 = 34962;
const ELEMENT_ARRAY_BUFFER: u32 = 34963;
const FLOAT: u32 = 5126;
const UNSIGNED_BYTE: u32 = 5121;
const UNSIGNED_SHORT: u32 = 5123;
const UNSIGNED_INT: u32 = 5125;

#[derive(Default)]
struct Builder {
    bin: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Builder {
    fn view(&mut self, bytes: &[u8], target: Option<u32>) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let mut v = json!({
            "buffer": 0,
            "byteOffset": self.bin.len(),
            "byteLength": bytes.len(),
        });
        if let (Some(t), Some(obj)) = (target, v.as_object_mut()) {
            obj.insert("target".into(), json!(t));
        }
        self.bin.extend_from_slice(bytes);
        self.views.push(v);
        self.views.len() - 1
    }

    #[allow(clippy::too_many_arguments)]
    fn accessor(
        &mut self,
        bytes: &[u8],
        target: Option<u32>,
        component: u32,
        normalized: bool,
        count: usize,
        ty: &str,
        minmax: Option<(Value, Value)>,
    ) -> usize {
        let view = self.view(bytes, target);
        let mut a = json!({
            "bufferView": view,
            "componentType": component,
            "count": count,
            "type": ty,
        });
        if let Some(obj) = a.as_object_mut() {
            if normalized {
                obj.insert("normalized".into(), json!(true));
            }
            if let Some((min, max)) = minmax {
                obj.insert("min".into(), min);
                obj.insert("max".into(), max);
            }
        }
        self.accessors.push(a);
        self.accessors.len() - 1
    }

    fn f32s<const N: usize>(&mut self, data: &[[f32; N]], ty: &str, minmax: bool) -> usize {
        let bytes: Vec<u8> = data
            .iter()
            .flat_map(|v| v.iter().flat_map(|c| c.to_le_bytes()))
            .collect();
        let mm = minmax.then(|| {
            let mut lo = [f32::INFINITY; N];
            let mut hi = [f32::NEG_INFINITY; N];
            for v in data {
                for i in 0..N {
                    lo[i] = lo[i].min(v[i]);
                    hi[i] = hi[i].max(v[i]);
                }
            }
            (json!(lo.to_vec()), json!(hi.to_vec()))
        });
        self.accessor(&bytes, Some(ARRAY_BUFFER), FLOAT, false, data.len(), ty, mm)
    }
}

fn finite3(v: [f32; 3]) -> [f32; 3] {
    v.map(|c| if c.is_finite() { c } else { 0.0 })
}

fn unit3(v: [f32; 3]) -> [f32; 3] {
    let v = finite3(v);
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-6 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// Encode the top mip of an image as PNG.
pub fn encode_png(img: &Image) -> Result<Vec<u8>> {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 {
        return Err(ModelError::Export(format!("image is {w}x{h}")));
    }
    let need = (w as usize)
        .checked_mul(h as usize)
        .and_then(|x| x.checked_mul(4))
        .ok_or_else(|| ModelError::Export(format!("image {w}x{h} too large")))?;
    let data = img
        .mips
        .first()
        .ok_or_else(|| ModelError::Export("image has no mips".into()))?;
    if data.len() != need {
        return Err(ModelError::Export(format!(
            "image {w}x{h} mip 0 has {} bytes, expected {need}",
            data.len()
        )));
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if img.format == ImageFormat::Rgba8Srgb {
            enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        }
        let mut wr = enc
            .write_header()
            .map_err(|e| ModelError::Export(format!("png header: {e}")))?;
        wr.write_image_data(data)
            .map_err(|e| ModelError::Export(format!("png data: {e}")))?;
        wr.finish()
            .map_err(|e| ModelError::Export(format!("png finish: {e}")))?;
    }
    Ok(out)
}

/// Export one LOD of a model as glTF binary with default options.
///
/// `textures` pairs a material index with its base colour image; at most
/// one image per material is used (the first).
pub fn export_glb(model: &Model, lod: usize, textures: &[(usize, Image)]) -> Result<Vec<u8>> {
    export_glb_with(model, lod, textures, &GlbOptions::default())
}

/// Export one LOD of a model as glTF binary.
pub fn export_glb_with(
    model: &Model,
    lod: usize,
    textures: &[(usize, Image)],
    opts: &GlbOptions,
) -> Result<Vec<u8>> {
    let lod_data = model.lods.get(lod).ok_or_else(|| {
        ModelError::Export(format!(
            "LOD {lod} requested, model has {}",
            model.lods.len()
        ))
    })?;
    let mut b = Builder::default();
    let mut nodes: Vec<Value> = Vec::new();
    let mut scene_nodes: Vec<usize> = Vec::new();

    // Materials and textures.
    let mut tex_for: BTreeMap<usize, &Image> = BTreeMap::new();
    for (m, img) in textures {
        if *m >= model.materials.len() {
            return Err(ModelError::Export(format!(
                "texture for material {m}, model has {} materials",
                model.materials.len()
            )));
        }
        tex_for.entry(*m).or_insert(img);
    }
    let mut images = Vec::new();
    let mut gl_textures = Vec::new();
    let mut materials = Vec::new();
    for (i, mat) in model.materials.iter().enumerate() {
        let name = mat
            .shader_name
            .clone()
            .unwrap_or_else(|| format!("shader_{:08x}", mat.shader.0));
        let mut pbr = json!({
            "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
            "metallicFactor": 0.0,
            "roughnessFactor": 1.0,
        });
        if let Some(img) = tex_for.get(&i) {
            let png = encode_png(img)?;
            let view = b.view(&png, None);
            images.push(json!({"bufferView": view, "mimeType": "image/png"}));
            gl_textures.push(json!({"sampler": 0, "source": images.len() - 1}));
            if let Some(obj) = pbr.as_object_mut() {
                obj.insert(
                    "baseColorTexture".into(),
                    json!({"index": gl_textures.len() - 1, "texCoord": 0}),
                );
            }
        }
        materials.push(json!({
            "name": name,
            "pbrMetallicRoughness": pbr,
            "extras": {"shader_hash": format!("{:08x}", mat.shader.0)},
        }));
    }

    // Skeleton nodes.
    let bones: &[Bone] = model
        .skeleton
        .as_ref()
        .map(|s| s.bones.as_slice())
        .unwrap_or(&[]);
    let bone_node0 = nodes.len();
    let mut armature: Option<usize> = None;
    if !bones.is_empty() {
        let parents = sanitize_parents(bones);
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); bones.len()];
        let mut roots = Vec::new();
        for (i, p) in parents.iter().enumerate() {
            match p.and_then(|p| children.get_mut(p)) {
                Some(c) => c.push(bone_node0 + i),
                None => roots.push(bone_node0 + i),
            }
        }
        for (i, bone) in bones.iter().enumerate() {
            let t = rage_to_gltf(finite3(bone.translation));
            let q = bone.rotation;
            let qn = (q.iter().map(|c| c * c).sum::<f32>()).sqrt();
            let rot = if qn.is_finite() && qn > 1e-6 {
                [q[0] / qn, q[2] / qn, -q[1] / qn, q[3] / qn]
            } else {
                [0.0, 0.0, 0.0, 1.0]
            };
            let s = finite3(bone.scale);
            let s = if s.contains(&0.0) {
                [1.0, 1.0, 1.0]
            } else {
                [s[0], s[2], s[1]]
            };
            let name = bone
                .name
                .clone()
                .unwrap_or_else(|| format!("bone_{:08x}", bone.name_hash.0));
            let mut n = json!({
                "name": name,
                "translation": t,
                "rotation": rot,
                "scale": s,
                "extras": {"name_hash": format!("{:08x}", bone.name_hash.0), "index": i},
            });
            if let (Some(c), Some(obj)) = (children.get(i), n.as_object_mut())
                && !c.is_empty()
            {
                obj.insert("children".into(), json!(c));
            }
            nodes.push(n);
        }
        nodes.push(json!({"name": "armature", "children": roots}));
        armature = Some(nodes.len() - 1);
    }

    // Skin, if any mesh carries joints.
    let any_skinned = lod_data
        .meshes
        .iter()
        .any(|m| !m.joints.is_empty() && m.joints.len() == m.positions.len());
    let skin_index = match armature {
        Some(arm) if opts.skin && any_skinned => {
            let mut ibm: Vec<u8> = Vec::with_capacity((bones.len() + 1) * 64);
            for bone in bones {
                let m = if bone.inverse_bind.iter().flatten().all(|v| v.is_finite()) {
                    inverse_bind_to_gltf(&bone.inverse_bind)
                } else {
                    log::warn!("glb: non-finite inverse bind matrix replaced by identity");
                    IDENTITY16
                };
                ibm.extend(m.iter().flat_map(|v| v.to_le_bytes()));
            }
            ibm.extend(IDENTITY16.iter().flat_map(|v| v.to_le_bytes()));
            let acc = b.accessor(&ibm, None, FLOAT, false, bones.len() + 1, "MAT4", None);
            let mut joints: Vec<usize> = (bone_node0..bone_node0 + bones.len()).collect();
            joints.push(arm);
            Some(json!({"inverseBindMatrices": acc, "joints": joints, "skeleton": arm}))
        }
        _ => None,
    };

    // Meshes.
    let mut meshes = Vec::new();
    for (mi, mesh) in lod_data.meshes.iter().enumerate() {
        let Some(prim) = primitive(
            &mut b,
            mesh,
            mi,
            model,
            opts,
            skin_index.is_some(),
            bones.len(),
        ) else {
            continue;
        };
        meshes.push(json!({"name": format!("mesh{mi}"), "primitives": [prim]}));
        let mut node = json!({"name": format!("mesh{mi}"), "mesh": meshes.len() - 1});
        if skin_index.is_some()
            && mesh.joints.len() == mesh.positions.len()
            && let Some(obj) = node.as_object_mut()
        {
            obj.insert("skin".into(), json!(0));
        }
        nodes.push(node);
        scene_nodes.push(nodes.len() - 1);
    }
    if let Some(arm) = armature {
        scene_nodes.push(arm);
    }

    // Assemble JSON.
    let mut root = json!({
        "asset": {
            "version": "2.0",
            "generator": "u_rome_model",
            "extras": {"coordinate_system": "glTF Y-up metres; gltf(x, y, z) = rage(x, z, -y)"},
        },
        "scene": 0,
        "scenes": [{"name": model.name.clone().unwrap_or_else(|| "model".into()), "nodes": scene_nodes}],
        "nodes": nodes,
    });
    let obj = root
        .as_object_mut()
        .ok_or_else(|| ModelError::Export("internal: root is not an object".into()))?;
    let mut put = |k: &str, v: Vec<Value>| {
        if !v.is_empty() {
            obj.insert(k.into(), Value::Array(v));
        }
    };
    put("meshes", meshes);
    put("materials", materials);
    put("images", images);
    let has_textures = !gl_textures.is_empty();
    put("textures", gl_textures);
    if has_textures {
        put(
            "samplers",
            vec![json!({"magFilter": 9729, "minFilter": 9729, "wrapS": 10497, "wrapT": 10497})],
        );
    }
    if let Some(s) = skin_index {
        put("skins", vec![s]);
    }
    while !b.bin.len().is_multiple_of(4) {
        b.bin.push(0);
    }
    if !b.bin.is_empty() {
        put("buffers", vec![json!({"byteLength": b.bin.len()})]);
    }
    put("bufferViews", std::mem::take(&mut b.views));
    put("accessors", std::mem::take(&mut b.accessors));

    let mut json_bytes =
        serde_json::to_vec(&root).map_err(|e| ModelError::Export(format!("json: {e}")))?;
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + if b.bin.is_empty() { 0 } else { 8 + b.bin.len() };
    let total32 = u32::try_from(total)
        .map_err(|_| ModelError::Export(format!("glb would be {total} bytes, above 4 GiB")))?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&total32.to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    if !b.bin.is_empty() {
        out.extend_from_slice(&(b.bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&b.bin);
    }
    Ok(out)
}

/// Parent index per bone, with invalid parents and cycles turned into
/// roots.
fn sanitize_parents(bones: &[Bone]) -> Vec<Option<usize>> {
    let n = bones.len();
    let mut parents: Vec<Option<usize>> = bones
        .iter()
        .enumerate()
        .map(|(i, b)| b.parent.map(usize::from).filter(|&p| p < n && p != i))
        .collect();
    for i in 0..n {
        // Walk up; more than n steps means a cycle.
        let mut cur = parents.get(i).copied().flatten();
        let mut steps = 0;
        while let Some(p) = cur {
            steps += 1;
            if steps > n || p == i {
                log::warn!("glb: bone {i} parent chain has a cycle; made a root");
                if let Some(slot) = parents.get_mut(i) {
                    *slot = None;
                }
                break;
            }
            cur = parents.get(p).copied().flatten();
        }
    }
    parents
}

fn primitive(
    b: &mut Builder,
    mesh: &Mesh,
    mi: usize,
    model: &Model,
    opts: &GlbOptions,
    skinned: bool,
    bone_count: usize,
) -> Option<Value> {
    let n = mesh.positions.len();
    if n == 0 {
        log::debug!("glb: mesh {mi} has no vertices; skipped");
        return None;
    }
    let mut dropped = 0usize;
    let indices: Vec<u32> = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|t| {
            let ok = t.iter().all(|&i| (i as usize) < n);
            if !ok {
                dropped += 1;
            }
            ok
        })
        .flatten()
        .copied()
        .collect();
    if dropped > 0 {
        log::warn!("glb: mesh {mi}: dropped {dropped} triangles with out-of-range indices");
    }
    if indices.is_empty() {
        log::warn!("glb: mesh {mi} has no valid triangles; skipped");
        return None;
    }

    let mut attrs = serde_json::Map::new();
    let pos: Vec<[f32; 3]> = mesh
        .positions
        .iter()
        .map(|p| rage_to_gltf(finite3(*p)))
        .collect();
    attrs.insert("POSITION".into(), json!(b.f32s(&pos, "VEC3", true)));
    if mesh.normals.len() == n {
        let nrm: Vec<[f32; 3]> = mesh
            .normals
            .iter()
            .map(|v| rage_to_gltf(unit3(*v)))
            .collect();
        attrs.insert("NORMAL".into(), json!(b.f32s(&nrm, "VEC3", false)));
    }
    if mesh.uvs.len() == n {
        let uv: Vec<[f32; 2]> = mesh
            .uvs
            .iter()
            .map(|v| v.map(|c| if c.is_finite() { c } else { 0.0 }))
            .collect();
        attrs.insert("TEXCOORD_0".into(), json!(b.f32s(&uv, "VEC2", false)));
    }
    if mesh.colors.len() == n && opts.colors != ColorExport::None {
        let bytes: Vec<u8> = mesh.colors.iter().flatten().copied().collect();
        let acc = b.accessor(
            &bytes,
            Some(ARRAY_BUFFER),
            UNSIGNED_BYTE,
            true,
            n,
            "VEC4",
            None,
        );
        let key = if opts.colors == ColorExport::Color0 {
            "COLOR_0"
        } else {
            "_RAGE_COLOR"
        };
        attrs.insert(key.into(), json!(acc));
    }
    if skinned && mesh.joints.len() == n {
        let mut bad = 0usize;
        let mut joints = Vec::with_capacity(n);
        let mut weights = Vec::with_capacity(n);
        for i in 0..n {
            let mut j = mesh.joints.get(i).copied().unwrap_or_default();
            let mut w = mesh.weights.get(i).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
            for k in 0..4 {
                if !w[k].is_finite() || w[k] < 0.0 {
                    w[k] = 0.0;
                }
                if usize::from(j[k]) >= bone_count {
                    if w[k] > 0.0 {
                        bad += 1;
                    }
                    w[k] = 0.0;
                }
                if w[k] == 0.0 {
                    j[k] = 0;
                }
            }
            let sum: f32 = w.iter().sum();
            if sum > 0.0 {
                w = w.map(|x| x / sum);
            } else {
                // No usable influence: bind to the armature joint (last).
                j = [bone_count as u16, 0, 0, 0];
                w = [1.0, 0.0, 0.0, 0.0];
            }
            joints.push(j);
            weights.push(w);
        }
        if bad > 0 {
            log::warn!(
                "glb: mesh {mi}: {bad} influences name a joint >= {bone_count} bones; dropped"
            );
        }
        let max = joints.iter().flatten().copied().max().unwrap_or(0);
        let acc = if max <= 255 {
            let bytes: Vec<u8> = joints.iter().flatten().map(|&j| j as u8).collect();
            b.accessor(
                &bytes,
                Some(ARRAY_BUFFER),
                UNSIGNED_BYTE,
                false,
                n,
                "VEC4",
                None,
            )
        } else {
            let bytes: Vec<u8> = joints
                .iter()
                .flatten()
                .flat_map(|j| j.to_le_bytes())
                .collect();
            b.accessor(
                &bytes,
                Some(ARRAY_BUFFER),
                UNSIGNED_SHORT,
                false,
                n,
                "VEC4",
                None,
            )
        };
        attrs.insert("JOINTS_0".into(), json!(acc));
        attrs.insert("WEIGHTS_0".into(), json!(b.f32s(&weights, "VEC4", false)));
    }

    let max_index = indices.iter().copied().max().unwrap_or(0);
    let idx_acc = if max_index < u32::from(u16::MAX) {
        let bytes: Vec<u8> = indices
            .iter()
            .flat_map(|&i| (i as u16).to_le_bytes())
            .collect();
        b.accessor(
            &bytes,
            Some(ELEMENT_ARRAY_BUFFER),
            UNSIGNED_SHORT,
            false,
            indices.len(),
            "SCALAR",
            None,
        )
    } else {
        let bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        b.accessor(
            &bytes,
            Some(ELEMENT_ARRAY_BUFFER),
            UNSIGNED_INT,
            false,
            indices.len(),
            "SCALAR",
            None,
        )
    };
    let mut prim = json!({"attributes": attrs, "indices": idx_acc, "mode": 4});
    if let (Some(m), Some(obj)) = (mesh.material, prim.as_object_mut()) {
        if m < model.materials.len() {
            obj.insert("material".into(), json!(m));
        } else {
            log::warn!(
                "glb: mesh {mi} names material {m} of {}",
                model.materials.len()
            );
        }
    }
    Some(prim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_conversion() {
        assert_eq!(rage_to_gltf([1.0, 2.0, 3.0]), [1.0, 3.0, -2.0]);
    }

    #[test]
    fn inverse_bind_translation_converts() {
        // Row-vector matrix translating by (1, 2, 3) (translation in row 3).
        let m = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [1.0, 2.0, 3.0, 1.0],
        ];
        let g = inverse_bind_to_gltf(&m);
        // Column-major: translation in elements 12..15, converted axes.
        assert_eq!(&g[12..15], &[1.0, 3.0, -2.0]);
        assert_eq!(g[15], 1.0);
        assert_eq!(g[0], 1.0);
        assert_eq!(g[5], 1.0);
        assert_eq!(g[10], 1.0);
    }

    #[test]
    fn missing_lod_and_bad_texture_error() {
        let model = Model::default();
        assert!(export_glb(&model, 0, &[]).is_err());
        let mut model = Model::default();
        model.lods.push(Default::default());
        model.materials.push(Default::default());
        let empty = export_glb(&model, 0, &[]).unwrap();
        assert_eq!(&empty[0..4], b"glTF");
        let img = Image {
            width: 2,
            height: 2,
            format: ImageFormat::Rgba8,
            mips: vec![vec![0; 15]],
            provisional: false,
        };
        assert!(export_glb(&model, 0, &[(0, img.clone())]).is_err());
        assert!(export_glb(&model, 0, &[(3, img)]).is_err());
    }

    #[test]
    fn cyclic_parents_become_roots() {
        let bone = |p: Option<u16>| Bone {
            name: None,
            name_hash: Default::default(),
            parent: p,
            rotation: [0.0, 0.0, 0.0, 1.0],
            translation: [0.0; 3],
            scale: [1.0; 3],
            inverse_bind: [[0.0; 4]; 4],
        };
        let bones = vec![bone(Some(1)), bone(Some(0)), bone(Some(9)), bone(Some(0))];
        let p = sanitize_parents(&bones);
        assert_eq!(p[2], None);
        assert_eq!(p[3], Some(0));
        assert!(p[0].is_none() || p[1].is_none());
    }
}
