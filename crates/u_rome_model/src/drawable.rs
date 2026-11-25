//! Drawable / fragment resource walking (best effort).
//!
//! Sources (facts only): GTA6-Asset-Info `formats/02-resources.md` §2.1 and
//! `formats/07-models.md` §7.1, §7.4-§7.6. Where a field is taken from the
//! GTA V layout by analogy rather than from a GTA VI source, the code says
//! so and the result is marked provisional.
//!
//! # Pointers
//!
//! Pointers are u64. `0x5xxxxxxx` points into the system page and
//! `0x6xxxxxxx` into the graphics page; the low 28 bits are the offset
//! (§2.1). The main drawable is the resource root (system offset 0) of a
//! `.#dr`, or the struct that a `.#ft` root points to at +0x18.
//!
//! # Structures read
//!
//! | Structure | Field |
//! |---|---|
//! | drawable | +0x10 shader group, +0x18 skeleton (GTA V analogy), +0x50/+0x58/+0x60/+0x68 LOD model-array headers (high..vlow), +0xA8 name |
//! | model-array header | ptr to model pointers, u16 count, u16 capacity |
//! | model (pointer + 8, stride 0x40) | +0x00 geometry pointers, +0x08 u16 count, +0x0C u16 palette count, +0x18 u16 shader index per geometry, +0x20 bone palette (u32 hashes) |
//! | geometry (0xA0) | +0x08 vertex buffer descriptor, +0x40 holder whose first u64 is the index buffer descriptor |
//! | buffer descriptor | +0x08 u32 size in dwords, +0x10 flags (0x1601 vertices, 0x1380 meshlets), +0x18 graphics pointer |
//! | shader group | +0x10 shader pointer array, +0x18 u16 count (GTA V analogy) |
//! | shader | +0x00 joaat(lowercase name), +0x04 "atem", +0x10 slot pointers, +0x20 parameter table |
//! | skeleton header | +0x00 bones (0x50 each), +0x08 inverse binds, +0x18 parent indices |
//!
//! LODs come only from the LOD group, never from file order (§7.4).

use std::collections::HashMap;

use u_rome_core::{
    Bone, Hash32, Material, Mesh, Model, ModelLod, Reader, Skeleton, joaat, joaat_lower,
};

use crate::error::{ModelError, Result};
use crate::index::decode_index_buffer_detailed;
use crate::vertex::{VertexData, decode_vertex_buffer, is_permutation, permute};

/// Sanity caps on counts read from headers.
pub const MAX_MODELS_PER_LOD: usize = 4096;
pub const MAX_GEOMETRIES_PER_MODEL: usize = 4096;
pub const MAX_SHADERS: usize = 4096;
pub const MAX_BONES: usize = 16384;
pub const MAX_PALETTE: usize = 4096;
/// Upper bound on a single buffer (vertex or index) in bytes.
pub const MAX_BUFFER_BYTES: usize = 256 << 20;

const LOD_OFFSETS: [usize; 4] = [0x50, 0x58, 0x60, 0x68];
const BONE_STRIDE: usize = 0x50;
const FLAGS_VERTEX: u16 = 0x1601;
const FLAGS_INDEX: u16 = 0x1380;

/// Result of [`decode_drawable`]: the model plus everything that was
/// guessed, skipped or repaired along the way.
#[derive(Clone, Debug, Default)]
pub struct DecodedModel {
    pub model: Model,
    /// Human-readable notes with offsets, also sent to `log::warn!`.
    pub warnings: Vec<String>,
    /// True when any part relied on an inferred layout (shader group count,
    /// skeleton location, material parameter table, page split, ...).
    pub provisional: bool,
}

/// The two pages of a decompressed resource.
#[derive(Clone, Copy, Debug)]
pub struct ResourcePages<'a> {
    pub system: &'a [u8],
    pub graphics: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    System,
    Graphics,
}

/// Resolve a 64-bit resource pointer to a page and offset.
pub fn resolve_pointer(ptr: u64) -> Option<(bool, usize)> {
    resolve(ptr).map(|(p, o)| (p == Page::System, o))
}

fn resolve(ptr: u64) -> Option<(Page, usize)> {
    if ptr >> 32 != 0 {
        return None;
    }
    let off = (ptr & 0x0FFF_FFFF) as usize;
    match (ptr >> 28) & 0xF {
        5 => Some((Page::System, off)),
        6 => Some((Page::Graphics, off)),
        _ => None,
    }
}

struct Ctx<'a> {
    sys: Reader<'a>,
    warnings: Vec<String>,
    provisional: bool,
}

impl<'a> Ctx<'a> {
    fn warn(&mut self, msg: String) {
        log::warn!("drawable: {msg}");
        self.warnings.push(msg);
    }

    fn u16(&self, off: usize) -> Option<u16> {
        self.sys.u16_at(off).ok()
    }

    fn u32(&self, off: usize) -> Option<u32> {
        self.sys.u32_at(off).ok()
    }

    fn u64(&self, off: usize) -> Option<u64> {
        self.sys.u64_at(off).ok()
    }

    fn f32s<const N: usize>(&self, off: usize) -> Option<[f32; N]> {
        let mut v = [0f32; N];
        for (i, x) in v.iter_mut().enumerate() {
            *x = self.sys.f32_at(off.checked_add(i * 4)?).ok()?;
        }
        Some(v)
    }

    /// Follow a system pointer stored at `off`; `None` if null or invalid.
    fn sys_ptr(&self, off: usize) -> Option<usize> {
        match resolve(self.u64(off)?)? {
            (Page::System, o) if o < self.sys.len() => Some(o),
            _ => None,
        }
    }

    fn gfx_ptr(&self, off: usize) -> Option<usize> {
        match resolve(self.u64(off)?)? {
            (Page::Graphics, o) => Some(o),
            _ => None,
        }
    }

    fn cstr(&self, off: usize) -> Option<String> {
        let data = self.sys.data().get(off..)?;
        let end = data.iter().take(256).position(|&b| b == 0)?;
        let s = data.get(..end)?;
        if s.is_empty() || !s.iter().all(|b| (0x20..0x7F).contains(b)) {
            return None;
        }
        Some(String::from_utf8_lossy(s).into_owned())
    }

    /// Model-array header at `off`: (pointer array offset, count).
    fn model_array(&self, off: usize) -> Option<(usize, usize)> {
        let arr = self.sys_ptr(off)?;
        let count = usize::from(self.u16(off + 8)?);
        (count > 0 && count <= MAX_MODELS_PER_LOD).then_some((arr, count))
    }

    fn has_lod_group(&self, d: usize) -> bool {
        LOD_OFFSETS.iter().any(|&o| {
            self.sys_ptr(d + o)
                .and_then(|h| self.model_array(h))
                .is_some()
        })
    }
}

/// A buffer referenced by a geometry: graphics offset and byte length.
#[derive(Clone, Copy, Debug)]
struct BufRef {
    offset: usize,
    len: usize,
    flags: u16,
}

#[derive(Clone, Debug)]
struct GeomRef {
    lod: usize,
    model: usize,
    geom: usize,
    vb: Option<BufRef>,
    ib: Option<BufRef>,
    shader: Option<u16>,
    palette: Vec<u32>,
}

/// Decode a drawable or fragment from its system and graphics pages.
pub fn decode_drawable_pages(pages: ResourcePages<'_>) -> Result<DecodedModel> {
    let mut ctx = Ctx {
        sys: Reader::new(pages.system),
        warnings: Vec::new(),
        provisional: false,
    };
    let d = find_main_drawable(&mut ctx)?;
    let geoms = collect_geometries(&mut ctx, d)?;
    finish(ctx, d, &geoms, pages.graphics)
}

/// Decode a drawable or fragment from the decompressed resource bytes
/// (system page followed by the graphics page).
///
/// The split between the pages is not stored in the bytes this function
/// receives, so it is inferred: the graphics start is chosen so that the
/// most vertex buffer pointers land on a vertex blob magic. The result is
/// marked provisional; prefer [`decode_drawable_pages`] when the archive
/// layer knows the split.
pub fn decode_drawable(resource_bytes: &[u8]) -> Result<DecodedModel> {
    let mut ctx = Ctx {
        sys: Reader::new(resource_bytes),
        warnings: Vec::new(),
        provisional: false,
    };
    let d = find_main_drawable(&mut ctx)?;
    let geoms = collect_geometries(&mut ctx, d)?;
    let vb_offsets: Vec<usize> = geoms
        .iter()
        .filter_map(|g| g.vb.map(|b| b.offset))
        .collect();
    let split = infer_graphics_start(resource_bytes, &vb_offsets).ok_or_else(|| {
        ModelError::Resource(format!(
            "could not locate the graphics page: no start offset puts any of {} vertex buffer pointers on a 0x4042/0x3063 magic",
            vb_offsets.len()
        ))
    })?;
    let gfx = resource_bytes.get(split.0..).unwrap_or(&[]);
    ctx.warn(format!(
        "graphics page start inferred at 0x{:X} ({} of {} vertex buffers on a magic)",
        split.0,
        split.1,
        vb_offsets.len()
    ));
    ctx.provisional = true;
    finish(ctx, d, &geoms, gfx)
}

/// Find the graphics page start `s` maximising how many `bytes[s + off]`
/// hold a vertex blob magic. Returns (start, matches).
fn infer_graphics_start(bytes: &[u8], vb_offsets: &[usize]) -> Option<(usize, usize)> {
    let first = *vb_offsets.first()?;
    let is_magic = |p: usize| {
        p.checked_add(4)
            .and_then(|e| bytes.get(p..e))
            .and_then(|b| b.first_chunk::<4>())
            .map(|b| u32::from_le_bytes(*b))
            .is_some_and(|m| {
                m == crate::vertex::VERTEX_MAGIC || m == crate::vertex::VERTEX_MAGIC_FLOAT
            })
    };
    let mut best: Option<(usize, usize)> = None;
    let mut candidates = 0usize;
    for p in first..bytes.len().saturating_sub(3) {
        if !is_magic(p) {
            continue;
        }
        let s = p - first;
        // The graphics page cannot start inside the resource header.
        if s < 0x10 {
            continue;
        }
        candidates += 1;
        if candidates > 100_000 {
            break;
        }
        let score = vb_offsets
            .iter()
            .filter(|&&o| s.checked_add(o).is_some_and(is_magic))
            .count();
        let better = match best {
            None => true,
            Some((bs, bscore)) => {
                score > bscore || (score == bscore && s % 16 == 0 && bs % 16 != 0)
            }
        };
        if better {
            best = Some((s, score));
        }
    }
    best
}

fn find_main_drawable(ctx: &mut Ctx<'_>) -> Result<usize> {
    if ctx.sys.len() < 0x70 {
        return Err(ModelError::Resource(format!(
            "system page is {} bytes, too small for a drawable",
            ctx.sys.len()
        )));
    }
    if ctx.has_lod_group(0) {
        return Ok(0);
    }
    // Fragment: root +0x18 points to the main drawable (§7.4).
    if let Some(d) = ctx.sys_ptr(0x18)
        && ctx.has_lod_group(d)
    {
        log::debug!("drawable: fragment root, main drawable at 0x{d:X}");
        return Ok(d);
    }
    Err(ModelError::Resource(
        "no LOD group at root +0x50..+0x68 and no fragment drawable at root +0x18 \
         (drawable dictionaries are not supported yet)"
            .into(),
    ))
}

fn read_buf_desc(ctx: &mut Ctx<'_>, desc: usize, what: &str) -> Option<BufRef> {
    let dwords = ctx.u32(desc + 8)? as usize;
    let flags = ctx.u16(desc + 0x10)?;
    let Some(offset) = ctx.gfx_ptr(desc + 0x18) else {
        ctx.warn(format!(
            "{what} descriptor at 0x{desc:X}: +0x18 is not a graphics pointer"
        ));
        return None;
    };
    let len = dwords.saturating_mul(4);
    if len > MAX_BUFFER_BYTES {
        ctx.warn(format!(
            "{what} descriptor at 0x{desc:X}: {len} bytes exceeds cap {MAX_BUFFER_BYTES}"
        ));
        return None;
    }
    Some(BufRef { offset, len, flags })
}

fn collect_geometries(ctx: &mut Ctx<'_>, d: usize) -> Result<Vec<GeomRef>> {
    let mut out = Vec::new();
    for (lod, &lo) in LOD_OFFSETS.iter().enumerate() {
        let Some(h) = ctx.sys_ptr(d + lo) else {
            continue;
        };
        let Some((arr, count)) = ctx.model_array(h) else {
            ctx.warn(format!(
                "LOD {lod}: model-array header at 0x{h:X} is invalid"
            ));
            continue;
        };
        for mi in 0..count {
            let Some(mp) = ctx.sys_ptr(arr + mi * 8) else {
                ctx.warn(format!(
                    "LOD {lod} model {mi}: pointer at 0x{:X} invalid",
                    arr + mi * 8
                ));
                continue;
            };
            // Model pointers point 8 bytes before the struct (§7.4).
            let s = mp + 8;
            let Some(garr) = ctx.sys_ptr(s) else {
                ctx.warn(format!(
                    "LOD {lod} model {mi} at 0x{s:X}: no geometry array"
                ));
                continue;
            };
            let gcount = usize::from(ctx.u16(s + 8).unwrap_or(0));
            if gcount > MAX_GEOMETRIES_PER_MODEL {
                ctx.warn(format!(
                    "LOD {lod} model {mi} at 0x{s:X}: {gcount} geometries exceeds cap"
                ));
                continue;
            }
            let shader_arr = ctx.sys_ptr(s + 0x18);
            let palette = read_palette(ctx, s);
            for gi in 0..gcount {
                let Some(g) = ctx.sys_ptr(garr + gi * 8) else {
                    ctx.warn(format!(
                        "LOD {lod} model {mi} geometry {gi}: pointer at 0x{:X} invalid",
                        garr + gi * 8
                    ));
                    continue;
                };
                let what = format!("LOD {lod} model {mi} geometry {gi} (record 0x{g:X})");
                let vb = ctx
                    .sys_ptr(g + 0x08)
                    .and_then(|desc| read_buf_desc(ctx, desc, &format!("{what} vertex buffer")));
                let ib = ctx
                    .sys_ptr(g + 0x40)
                    .and_then(|holder| ctx.sys_ptr(holder))
                    .and_then(|desc| read_buf_desc(ctx, desc, &format!("{what} index buffer")));
                if let Some(b) = vb
                    && b.flags != FLAGS_VERTEX
                {
                    ctx.warn(format!(
                        "{what}: vertex buffer flags 0x{:04X}, expected 0x{FLAGS_VERTEX:04X}",
                        b.flags
                    ));
                }
                if let Some(b) = ib
                    && b.flags != FLAGS_INDEX
                {
                    ctx.warn(format!(
                        "{what}: index buffer flags 0x{:04X}, expected 0x{FLAGS_INDEX:04X}",
                        b.flags
                    ));
                }
                out.push(GeomRef {
                    lod,
                    model: mi,
                    geom: gi,
                    vb,
                    ib,
                    shader: shader_arr.and_then(|a| ctx.u16(a + gi * 2)),
                    palette: palette.clone(),
                });
            }
        }
    }
    if out.is_empty() {
        return Err(ModelError::Resource(format!(
            "drawable at 0x{d:X} has a LOD group but no readable geometry"
        )));
    }
    Ok(out)
}

/// Ped bone palette: model +0x0C u16 count, +0x20 pointer to u32 joaat
/// hashes, sorted ascending (§7.5). On vehicles these fields hold
/// something else (not ascending), so unsorted data is ignored.
fn read_palette(ctx: &mut Ctx<'_>, s: usize) -> Vec<u32> {
    let count = usize::from(ctx.u16(s + 0x0C).unwrap_or(0));
    if count == 0 || count > MAX_PALETTE {
        return Vec::new();
    }
    let Some(p) = ctx.sys_ptr(s + 0x20) else {
        return Vec::new();
    };
    let Some(bytes) = ctx.sys.bytes_at(p, count * 4).ok() else {
        return Vec::new();
    };
    let pal: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    if pal.windows(2).all(|w| w[0] < w[1]) {
        pal
    } else {
        log::debug!("drawable: model 0x{s:X} +0x0C/+0x20 is not an ascending bone palette");
        Vec::new()
    }
}

fn finish(mut ctx: Ctx<'_>, d: usize, geoms: &[GeomRef], gfx: &[u8]) -> Result<DecodedModel> {
    let mut model = Model {
        name: read_name(&ctx, d),
        ..Default::default()
    };
    model.materials = read_materials(&mut ctx, d);

    let mut lods: Vec<ModelLod> = vec![ModelLod::default(); 4];
    let mut palettes: Vec<(usize, usize, Vec<u32>)> = Vec::new();
    for g in geoms {
        let what = format!("LOD {} model {} geometry {}", g.lod, g.model, g.geom);
        let Some(mesh) = decode_geometry(&mut ctx, g, gfx, &what) else {
            continue;
        };
        let mut mesh = mesh;
        if let Some(si) = g.shader {
            if usize::from(si) < model.materials.len() {
                mesh.material = Some(usize::from(si));
            } else if !model.materials.is_empty() {
                ctx.warn(format!(
                    "{what}: shader index {si} >= {} shaders",
                    model.materials.len()
                ));
            }
        }
        if let Some(l) = lods.get_mut(g.lod) {
            if !g.palette.is_empty() {
                palettes.push((g.lod, l.meshes.len(), g.palette.clone()));
            }
            l.meshes.push(mesh);
        }
    }
    // Drop empty trailing LODs but keep indices of populated ones stable.
    while lods.last().is_some_and(|l| l.meshes.is_empty()) {
        lods.pop();
    }
    model.lods = lods;

    let wants_skeleton = model
        .lods
        .iter()
        .flat_map(|l| &l.meshes)
        .any(|m| !m.joints.is_empty())
        || !palettes.is_empty()
        || ctx.sys_ptr(d + 0x18).is_some();
    if wants_skeleton {
        model.skeleton = find_skeleton(&mut ctx, d);
    }
    if !palettes.is_empty() {
        apply_palettes(&mut ctx, &mut model, &palettes);
    }
    if let Some(skel) = &model.skeleton {
        let n = skel.bones.len();
        for (li, l) in model.lods.iter().enumerate() {
            for (mi, m) in l.meshes.iter().enumerate() {
                let bad = m
                    .joints
                    .iter()
                    .zip(&m.weights)
                    .flat_map(|(j, w)| j.iter().zip(w))
                    .filter(|(j, w)| **w > 0.0 && usize::from(**j) >= n)
                    .count();
                if bad > 0 {
                    ctx.warn(format!(
                        "LOD {li} mesh {mi}: {bad} influences name a joint >= {n} bones"
                    ));
                }
            }
        }
    }
    if model.lods.iter().all(|l| l.meshes.is_empty()) {
        return Err(ModelError::Resource(format!(
            "no geometry decoded; first problems: {}",
            ctx.warnings
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    Ok(DecodedModel {
        model,
        warnings: ctx.warnings,
        provisional: ctx.provisional,
    })
}

fn decode_geometry(ctx: &mut Ctx<'_>, g: &GeomRef, gfx: &[u8], what: &str) -> Option<Mesh> {
    let slice = |b: BufRef| -> Option<&[u8]> {
        let end = b.offset.checked_add(b.len)?.min(gfx.len());
        gfx.get(b.offset..end)
    };
    let Some(vbuf) = g.vb.and_then(slice) else {
        ctx.warn(format!(
            "{what}: vertex buffer missing or outside the graphics page"
        ));
        return None;
    };
    let Some(ibuf) = g.ib.and_then(slice) else {
        ctx.warn(format!(
            "{what}: index buffer missing or outside the graphics page"
        ));
        return None;
    };
    let mut vd = match decode_vertex_buffer(vbuf) {
        Ok(v) => v,
        Err(e) => {
            ctx.warn(format!(
                "{what}: vertex buffer at graphics 0x{:X}: {e}",
                g.vb.map_or(0, |b| b.offset)
            ));
            return None;
        }
    };
    let ib = match decode_index_buffer_detailed(ibuf) {
        Ok(i) => i,
        Err(e) => {
            ctx.warn(format!(
                "{what}: index buffer at graphics 0x{:X}: {e}",
                g.ib.map_or(0, |b| b.offset)
            ));
            return None;
        }
    };
    for w in &vd.warnings {
        ctx.warnings.push(format!("{what}: {w}"));
    }
    if vd.provisional {
        ctx.provisional = true;
    }
    let v = vd.vertex_count;
    let mut indices: Vec<u32> = Vec::with_capacity(ib.indices.len());
    let mut out_of_range = 0usize;
    for t in ib.indices.as_chunks::<3>().0 {
        if t.iter().all(|&i| (i as usize) < v) {
            indices.extend_from_slice(t);
        } else {
            out_of_range += 1;
        }
    }
    if out_of_range > 0 {
        ctx.warn(format!(
            "{what}: {out_of_range} triangles index past {v} vertices; dropped"
        ));
    }
    if vd.axis_order.is_none() {
        let order = best_axis_order(&vd.positions, &vd.normals, &indices);
        ctx.warn(format!(
            "{what}: axis order chosen by scoring against normals: {order:?}"
        ));
        vd.apply_axis_order(order);
    }
    let flipped = orient_triangles(&vd.positions, &vd.normals, &mut indices);
    if flipped > 0 {
        log::debug!(
            "drawable: {what}: flipped {flipped} of {} triangles to match normals",
            indices.len() / 3
        );
    }
    Some(vertex_data_to_mesh(vd, indices))
}

/// Build a core [`Mesh`] from decoded vertex data and a triangle list.
/// UV0 becomes `uvs`; other UV sets are not carried by the core type.
pub fn vertex_data_to_mesh(vd: VertexData, indices: Vec<u32>) -> Mesh {
    let (joints, weights) = vd.skin();
    Mesh {
        positions: vd.positions,
        normals: vd.normals,
        tangents: vd.tangents,
        uvs: vd.uv_sets.into_iter().next().unwrap_or_default(),
        colors: vd.colors,
        joints,
        weights,
        indices,
        material: None,
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn tri_verts<T: Copy>(data: &[T], t: &[u32; 3]) -> Option<[T; 3]> {
    Some([
        *data.get(t[0] as usize)?,
        *data.get(t[1] as usize)?,
        *data.get(t[2] as usize)?,
    ])
}

/// Flip triangles whose counter-clockwise face normal points against the
/// summed vertex normals (GTA6-Asset-Info §7.3: about 97 % agree, flip the
/// rest). Returns the number flipped. No-op without normals.
pub fn orient_triangles(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    indices: &mut [u32],
) -> usize {
    if normals.len() != positions.len() {
        return 0;
    }
    let mut flipped = 0;
    for t in indices.as_chunks_mut::<3>().0 {
        let (Some(p), Some(n)) = (tri_verts(positions, t), tri_verts(normals, t)) else {
            continue;
        };
        let face = cross(sub(p[1], p[0]), sub(p[2], p[0]));
        let ns = [
            n[0][0] + n[1][0] + n[2][0],
            n[0][1] + n[1][1] + n[2][1],
            n[0][2] + n[1][2] + n[2][2],
        ];
        if dot(face, ns) < 0.0 {
            t.swap(1, 2);
            flipped += 1;
        }
    }
    flipped
}

/// Score all six slot permutations by mean |cos| between face normals and
/// stored normals and return the best. Only used when the +0x6C axis mode
/// is not 0..=2 (§7.2). `positions` must be in slot order.
pub fn best_axis_order(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    indices: &[u32],
) -> [usize; 3] {
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    if normals.len() != positions.len() || indices.len() < 3 {
        return [0, 1, 2];
    }
    let tris = indices.as_chunks::<3>().0;
    // Sample at most 4096 triangles evenly.
    let stride = (tris.len() / 4096).max(1);
    let mut best = ([0, 1, 2], f32::NEG_INFINITY);
    for order in ORDERS {
        debug_assert!(is_permutation(order));
        let mut sum = 0f32;
        let mut n = 0usize;
        for t in tris.iter().step_by(stride) {
            let (Some(p), Some(nr)) = (tri_verts(positions, t), tri_verts(normals, t)) else {
                continue;
            };
            let p = p.map(|v| permute(v, order));
            let face = cross(sub(p[1], p[0]), sub(p[2], p[0]));
            let fl = dot(face, face).sqrt();
            if fl < 1e-12 {
                continue;
            }
            let ns = [
                nr[0][0] + nr[1][0] + nr[2][0],
                nr[0][1] + nr[1][1] + nr[2][1],
                nr[0][2] + nr[1][2] + nr[2][2],
            ];
            let nl = dot(ns, ns).sqrt();
            if nl < 1e-12 {
                continue;
            }
            sum += (dot(face, ns) / (fl * nl)).abs();
            n += 1;
        }
        let score = if n > 0 { sum / n as f32 } else { 0.0 };
        if score > best.1 + 1e-6 {
            best = (order, score);
        }
    }
    best.0
}

fn read_name(ctx: &Ctx<'_>, d: usize) -> Option<String> {
    let strip = |s: String| -> String {
        match s.rfind(".#") {
            Some(i) => s.get(..i).map(str::to_owned).unwrap_or(s),
            None => s,
        }
    };
    if let Some(p) = ctx.sys_ptr(d + 0xA8)
        && let Some(s) = ctx.cstr(p)
    {
        return Some(strip(s));
    }
    // Fallback (§2.3): the name survives as `name.#xx` in the system page;
    // scan backwards over [a-z0-9_-].
    let data = ctx.sys.data();
    for suffix in [b".#ft".as_slice(), b".#dr".as_slice()] {
        if let Some(pos) = data.windows(4).position(|w| w == suffix) {
            let start = data
                .get(..pos)?
                .iter()
                .rposition(|b| {
                    !(b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
                })
                .map_or(0, |i| i + 1);
            if start < pos {
                return data
                    .get(start..pos)
                    .map(|s| String::from_utf8_lossy(s).into_owned());
            }
        }
    }
    None
}

fn read_materials(ctx: &mut Ctx<'_>, d: usize) -> Vec<Material> {
    let Some(group) = ctx.sys_ptr(d + 0x10) else {
        return Vec::new();
    };
    let Some(arr) = ctx.sys_ptr(group + 0x10) else {
        ctx.warn(format!(
            "shader group at 0x{group:X}: no shader array at +0x10"
        ));
        return Vec::new();
    };
    // Count at +0x18 by analogy with GTA V's shader group; validated by the
    // "atem" tag on each shader.
    let count = usize::from(ctx.u16(group + 0x18).unwrap_or(0)).min(MAX_SHADERS);
    ctx.provisional = true;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let Some(s) = ctx.sys_ptr(arr + i * 8) else {
            ctx.warn(format!(
                "shader {i}: pointer at 0x{:X} invalid; stopping at {i} shaders",
                arr + i * 8
            ));
            break;
        };
        let tag = ctx.sys.bytes_at(s + 4, 4).ok();
        if tag != Some(b"atem".as_slice()) {
            ctx.warn(format!(
                "shader {i} at 0x{s:X}: no \"atem\" tag at +0x04; stopping at {i} shaders"
            ));
            break;
        }
        let shader = Hash32(ctx.u32(s).unwrap_or(0));
        let textures = read_shader_textures(ctx, s, i);
        out.push(Material {
            shader,
            shader_name: None,
            textures,
        });
    }
    out
}

/// Texture parameters of one shader (§7.6): parameter table at +0x20 (byte
/// 1 = texture count, then {u32 param hash, u32 descriptor} entries where a
/// texture's descriptor is 4 x slot), slot pointers at +0x10, each slot ->
/// 0x50-byte struct -> +0x30 texture reference -> +0x08 u64 texture hash.
/// The entry start inside the table is not published; +0x08 and +0x10 are
/// tried and validated.
fn read_shader_textures(ctx: &mut Ctx<'_>, s: usize, si: usize) -> Vec<(Hash32, u64)> {
    let (Some(table), Some(slots)) = (ctx.sys_ptr(s + 0x20), ctx.sys_ptr(s + 0x10)) else {
        return Vec::new();
    };
    let tex_count = ctx.sys.u8_at(table + 1).map_or(0, usize::from);
    if tex_count == 0 {
        return Vec::new();
    }
    let entries = [8usize, 0x10].into_iter().find(|&base| {
        (0..tex_count).all(|i| {
            ctx.u32(table + base + i * 8 + 4)
                .is_some_and(|desc| desc % 4 == 0 && (desc / 4) < tex_count as u32)
        })
    });
    if entries.is_none() {
        ctx.warn(format!(
            "shader {si} at 0x{s:X}: parameter table at 0x{table:X} has no recognisable texture entries"
        ));
    }
    let mut out = Vec::new();
    for i in 0..tex_count {
        let (param, slot) = match entries {
            Some(base) => {
                let e = table + base + i * 8;
                (
                    Hash32(ctx.u32(e).unwrap_or(0)),
                    ctx.u32(e + 4).map_or(i, |d| (d / 4) as usize),
                )
            }
            None => (Hash32(0), i),
        };
        let hash = ctx
            .sys_ptr(slots + slot * 8)
            .and_then(|st| ctx.sys_ptr(st + 0x30))
            .and_then(|r| ctx.u64(r + 8));
        match hash {
            Some(h) if h != 0 => out.push((param, h)),
            _ => log::debug!("drawable: shader {si} texture {i}: no texture reference"),
        }
    }
    out
}

/// Locate and read the skeleton. Tries the drawable's +0x18 pointer with the
/// header at +0x20 (GTA V layout analogy) and at +0x00, then scans the
/// system page for a header whose bone array and parent array agree.
fn find_skeleton(ctx: &mut Ctx<'_>, d: usize) -> Option<Skeleton> {
    let mut candidates = Vec::new();
    if let Some(p) = ctx.sys_ptr(d + 0x18) {
        candidates.push(p + 0x20);
        candidates.push(p);
    }
    for h in candidates {
        if let Some((skel, _)) = read_skeleton_header(ctx, h) {
            ctx.warn(format!(
                "skeleton header at 0x{h:X} ({} bones) located through drawable +0x18 (GTA V layout analogy)",
                skel.bones.len()
            ));
            ctx.provisional = true;
            return Some(skel);
        }
    }
    let len = ctx.sys.len();
    let mut h = 0;
    while h + 0x20 <= len {
        if let Some((skel, true)) = read_skeleton_header(ctx, h) {
            ctx.warn(format!(
                "skeleton header found by scanning at 0x{h:X} ({} bones)",
                skel.bones.len()
            ));
            ctx.provisional = true;
            return Some(skel);
        }
        h += 8;
    }
    ctx.warn("no skeleton found; joints stay unresolved".into());
    ctx.provisional = true;
    None
}

/// Read a skeleton header; returns the skeleton and whether the parent
/// index array at +0x18 agrees with the bone records.
fn read_skeleton_header(ctx: &Ctx<'_>, h: usize) -> Option<(Skeleton, bool)> {
    let bones_at = ctx.sys_ptr(h)?;
    let ibm_at = ctx.sys_ptr(h + 8)?;
    // Bone 0 must be index 0 with no parent.
    if ctx.u16(bones_at + 0x48)? != 0 || ctx.u16(bones_at + 0x46)? != 0xFFFF {
        return None;
    }
    let mut bones = Vec::new();
    for i in 0..MAX_BONES {
        let r = bones_at + i * BONE_STRIDE;
        if r + BONE_STRIDE > ctx.sys.len() || ctx.u16(r + 0x48) != Some(i as u16) {
            break;
        }
        let parent = ctx.u16(r + 0x46)?;
        if parent != 0xFFFF && usize::from(parent) >= MAX_BONES {
            break;
        }
        let name_ptr = ctx.u64(r + 0x30)?;
        let name = if name_ptr == 0 {
            None
        } else {
            match resolve(name_ptr) {
                Some((Page::System, o)) => ctx.cstr(o),
                _ => break,
            }
        };
        let stored_hash = ctx.u32(r + 0x1C)?;
        let name_hash = if stored_hash != 0 {
            Hash32(stored_hash)
        } else if let Some(n) = &name {
            joaat_lower(n)
        } else {
            Hash32(u32::from(ctx.u16(r + 0x4A)?))
        };
        let ibm = ibm_at + i * 64;
        let inverse_bind = match ctx.f32s::<16>(ibm) {
            Some(m) => [
                [m[0], m[1], m[2], m[3]],
                [m[4], m[5], m[6], m[7]],
                [m[8], m[9], m[10], m[11]],
                [m[12], m[13], m[14], m[15]],
            ],
            None => break,
        };
        bones.push(Bone {
            name,
            name_hash,
            parent: (parent != 0xFFFF).then_some(parent),
            rotation: ctx.f32s::<4>(r)?,
            translation: ctx.f32s::<3>(r + 0x10)?,
            scale: ctx.f32s::<3>(r + 0x20)?,
            inverse_bind,
        });
    }
    if bones.is_empty() {
        return None;
    }
    let parents_match = ctx.sys_ptr(h + 0x18).is_some_and(|pa| {
        bones.iter().take(16).enumerate().all(|(i, b)| {
            ctx.u16(pa + i * 2)
                .is_some_and(|p| p == b.parent.unwrap_or(0xFFFF))
        })
    });
    Some((Skeleton { bones }, parents_match))
}

/// Remap palette-relative joint indices to skeleton bone indices by name
/// hash. A one-entry palette on a mesh without joints binds every vertex to
/// that bone (§7.5 "One-bone palettes").
fn apply_palettes(ctx: &mut Ctx<'_>, model: &mut Model, palettes: &[(usize, usize, Vec<u32>)]) {
    let Some(skel) = &model.skeleton else {
        ctx.warn("bone palette present but no skeleton; joints left palette-relative".into());
        return;
    };
    let mut by_hash: HashMap<u32, u16> = HashMap::new();
    for (i, b) in skel.bones.iter().enumerate() {
        let i = i as u16;
        by_hash.entry(b.name_hash.0).or_insert(i);
        if let Some(n) = &b.name {
            by_hash.entry(joaat(n).0).or_insert(i);
            by_hash.entry(joaat_lower(n).0).or_insert(i);
        }
    }
    ctx.provisional = true;
    for (lod, mi, pal) in palettes {
        let Some(mesh) = model.lods.get_mut(*lod).and_then(|l| l.meshes.get_mut(*mi)) else {
            continue;
        };
        let map: Vec<Option<u16>> = pal.iter().map(|h| by_hash.get(h).copied()).collect();
        let unresolved = map.iter().filter(|m| m.is_none()).count();
        if unresolved > 0 {
            ctx.warn(format!(
                "LOD {lod} mesh {mi}: {unresolved} of {} palette bones not in this skeleton (shared ped skeleton?)",
                pal.len()
            ));
        }
        if mesh.joints.is_empty() && pal.len() == 1 {
            if let Some(Some(b)) = map.first() {
                let n = mesh.positions.len();
                mesh.joints = vec![[*b, 0, 0, 0]; n];
                mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; n];
            }
            continue;
        }
        for (j, w) in mesh.joints.iter_mut().zip(mesh.weights.iter_mut()) {
            for k in 0..4 {
                match map.get(usize::from(j[k])).copied().flatten() {
                    Some(b) => j[k] = b,
                    None => {
                        j[k] = 0;
                        w[k] = 0.0;
                    }
                }
            }
            let sum: f32 = w.iter().sum();
            if sum > 0.0 {
                *w = w.map(|x| x / sum);
            } else {
                *w = [1.0, 0.0, 0.0, 0.0];
            }
        }
    }
}

#[cfg(test)]
mod tests;
