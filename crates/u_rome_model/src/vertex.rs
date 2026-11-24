//! Vertex buffer ("mesh blob") decoding.
//!
//! Layout (GTA6-Asset-Info `formats/07-models.md` §7.2, facts only):
//!
//! * u32 slot table at `+4k`, `k = 0..23`. Each slot is
//!   `offset_in_2_byte_units << 8 | format`; 0 means the slot is unused.
//!   Slot 0 is also the magic: `0x4042` (quantised positions at 0x80) or
//!   `0x3063` (3 x f32 positions at 0x60, cables and water).
//! * Quantised header: f32 x 3 base at 0x60, u32 axis mode at 0x6C, f32 x 3
//!   step at 0x70.
//! * Streams are tightly packed arrays of `V` elements. `V` is the position
//!   section length divided by the position element size (for the quantised
//!   variant: `(smallest stream offset > 0x80 - 0x80) / 8`).
//!
//! Positions come out in model space (the +0x6C axis order applied), RAGE
//! Z-up metres. Normals and tangents are stored in model space already.

use u_rome_core::Reader;

use crate::error::{Context, ModelError, Result};

/// Slot 0 value of a quantised-position blob (also the blob magic).
pub const VERTEX_MAGIC: u32 = 0x4042;
/// Slot 0 value of a float-position blob (cables, water).
pub const VERTEX_MAGIC_FLOAT: u32 = 0x3063;
/// Number of stream slots in the header.
pub const STREAM_SLOTS: usize = 23;
/// Sanity cap on vertices per blob (the largest known mesh has ~65 K).
pub const MAX_VERTICES: usize = 1 << 24;

/// End of the slot table; no stream may start before this.
const SLOT_TABLE_END: usize = STREAM_SLOTS * 4;
const QUANT_DATA_START: usize = 0x80;
const FLOAT_DATA_START: usize = 0x60;

/// Stream format codes (low byte of a slot).
pub mod format {
    /// Slot 8: one joint per vertex, u8 or u16 (width from the section size).
    pub const JOINT_SINGLE: u8 = 0x00;
    /// Octahedral normal, 10:10 in a u32.
    pub const OCT_NORMAL: u8 = 0x21;
    /// 4 x u8 joints.
    pub const U8X4: u8 = 0x25;
    /// RGBA8 (colours, skin weights).
    pub const RGBA8: u8 = 0x26;
    /// R10G10B10A2 tangent.
    pub const RGB10A2: u8 = 0x27;
    /// 2 x s16 / 8192 UV.
    pub const S16X2: u8 = 0x28;
    /// 8 bytes: quantised u64 position in slot 0, 2 x f32 UV elsewhere.
    pub const EIGHT_BYTE: u8 = 0x42;
    /// 4 x u16 joints.
    pub const U16X4: u8 = 0x4A;
    /// 3 x f32 (float positions, water normals, cable directions).
    pub const F32X3: u8 = 0x63;
    /// Ped head skin, 16 bytes per vertex (inferred, provisional).
    pub const HEAD_SKIN: u8 = 0x8C;
}

/// Byte size of one element of a stream format, if fixed.
pub fn element_size(fmt: u8) -> Option<usize> {
    match fmt {
        format::OCT_NORMAL | format::U8X4 | format::RGBA8 | format::RGB10A2 | format::S16X2 => {
            Some(4)
        }
        format::EIGHT_BYTE | format::U16X4 => Some(8),
        format::F32X3 => Some(12),
        format::HEAD_SKIN => Some(16),
        _ => None,
    }
}

/// One non-empty slot of the stream table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub slot: u8,
    pub format: u8,
    /// Byte offset of the stream inside the blob.
    pub offset: usize,
    /// Bytes from `offset` to the next stream (or the end of the blob).
    pub section_len: usize,
    /// Whether this crate understood and decoded the stream.
    pub decoded: bool,
}

/// How positions are stored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PositionEncoding {
    /// u64 per vertex, dequantised as `slot * step + base` per slot.
    Quantized {
        base: [f32; 3],
        step: [f32; 3],
        /// Raw u32 at +0x6C.
        axis_mode: u32,
    },
    /// 3 x f32 per vertex.
    Float,
}

/// Width and layout of the joint stream (slot 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JointFormat {
    /// Format 0x00, one u8 joint, weight 1.
    SingleU8,
    /// Format 0x00, one u16 joint, weight 1 (skeletons above 255 bones).
    SingleU16,
    /// Format 0x25, 4 x u8.
    U8x4,
    /// Format 0x4A, 4 x u16.
    U16x4,
    /// Format 0x8C, 16 bytes per vertex, inferred reading (provisional).
    HeadSkin,
}

/// Everything decoded from one vertex buffer blob.
///
/// Attribute vectors are either empty (stream absent) or `vertex_count` long.
#[derive(Clone, Debug, Default)]
pub struct VertexData {
    pub vertex_count: usize,
    /// `None` only for a default-constructed value.
    pub position_encoding: Option<PositionEncoding>,
    /// Model-space positions (RAGE Z-up metres). When the axis mode is not
    /// 0..=2 they are left in slot order and `axis_order` is `None`.
    pub positions: Vec<[f32; 3]>,
    /// Slot permutation applied to positions: `model[order[i]] = slot[i]`.
    pub axis_order: Option<[usize; 3]>,
    /// Slot 2, unit length.
    pub normals: Vec<[f32; 3]>,
    /// Slot 11: xyz unit tangent, w = bitangent sign. The frame belongs to
    /// the second UV set (GTA6-Asset-Info §7.2, "high").
    pub tangents: Vec<[f32; 4]>,
    /// UV0..UV3 from slots 3, 4, 14, 15. D3D convention (v down), which is
    /// also glTF's.
    pub uv_sets: [Vec<[f32; 2]>; 4],
    /// Slot 1 RGBA8. b = tint palette index, a = layer-2 weight; r and g
    /// meaning unknown. Not necessarily a display colour.
    pub colors: Vec<[u8; 4]>,
    /// Slot 12 RGBA8 (vehicle sticker zone in a; terrain layer weights).
    pub colors_slot12: Vec<[u8; 4]>,
    /// Slot 13 RGBA8, meaning unknown.
    pub colors_slot13: Vec<[u8; 4]>,
    /// Slot 8 joints (indices into the skeleton or the model's bone palette).
    pub joints: Vec<[u16; 4]>,
    pub joint_format: Option<JointFormat>,
    /// Slot 6 raw weights (sum 255 over all influences).
    pub weights: Vec<[u8; 4]>,
    /// Slot 9: second joint set on ped meshes.
    pub joints2: Vec<[u16; 4]>,
    /// Slot 7: second weight set on ped meshes.
    pub weights2: Vec<[u8; 4]>,
    pub streams: Vec<StreamInfo>,
    /// Human-readable notes about anything skipped or guessed.
    pub warnings: Vec<String>,
    /// True when any part of the decode relied on an inferred fact.
    pub provisional: bool,
}

impl VertexData {
    fn warn(&mut self, msg: String) {
        log::warn!("vertex buffer: {msg}");
        self.warnings.push(msg);
    }

    /// Per-vertex skinning reduced to four influences, as used by
    /// [`u_rome_core::Mesh`]. Returns empty vectors when the blob has no
    /// joint stream.
    ///
    /// Ped meshes carry eight influences (slots 8/6 and 9/7); the four
    /// heaviest are kept and renormalised. Weights always sum to 1.
    pub fn skin(&self) -> (Vec<[u16; 4]>, Vec<[f32; 4]>) {
        if self.joints.len() != self.vertex_count || self.vertex_count == 0 {
            return (Vec::new(), Vec::new());
        }
        let single = matches!(
            self.joint_format,
            Some(JointFormat::SingleU8 | JointFormat::SingleU16)
        );
        let has_w = self.weights.len() == self.vertex_count;
        let has_second =
            self.joints2.len() == self.vertex_count && self.weights2.len() == self.vertex_count;
        let mut joints = Vec::with_capacity(self.vertex_count);
        let mut weights = Vec::with_capacity(self.vertex_count);
        for i in 0..self.vertex_count {
            let j1 = self.joints.get(i).copied().unwrap_or_default();
            if single || !has_w {
                joints.push([j1[0], 0, 0, 0]);
                weights.push([1.0, 0.0, 0.0, 0.0]);
                continue;
            }
            let w1 = self.weights.get(i).copied().unwrap_or_default();
            let mut inf: Vec<(u16, u8)> = j1.iter().copied().zip(w1).collect();
            if has_second {
                let j2 = self.joints2.get(i).copied().unwrap_or_default();
                let w2 = self.weights2.get(i).copied().unwrap_or_default();
                inf.extend(j2.iter().copied().zip(w2));
                // Stable sort keeps slot order among equal weights.
                inf.sort_by_key(|&(_, w)| std::cmp::Reverse(w));
            }
            let mut j = [0u16; 4];
            let mut w = [0f32; 4];
            for (k, (jj, ww)) in inf.iter().take(4).enumerate() {
                j[k] = *jj;
                w[k] = f32::from(*ww);
            }
            let sum: f32 = w.iter().sum();
            if sum > 0.0 {
                for x in &mut w {
                    *x /= sum;
                }
            } else {
                w = [1.0, 0.0, 0.0, 0.0];
            }
            // glTF wants unused influences to carry joint 0 and weight 0.
            for k in 0..4 {
                if w[k] == 0.0 {
                    j[k] = 0;
                }
            }
            joints.push(j);
            weights.push(w);
        }
        (joints, weights)
    }

    /// Re-order positions from slot order with `order` (`model[order[i]] =
    /// slot[i]`). Only valid while `axis_order` is `None`.
    pub fn apply_axis_order(&mut self, order: [usize; 3]) {
        if self.axis_order.is_some() || !is_permutation(order) {
            return;
        }
        for p in &mut self.positions {
            *p = permute(*p, order);
        }
        self.axis_order = Some(order);
    }
}

pub(crate) fn is_permutation(o: [usize; 3]) -> bool {
    let mut seen = [false; 3];
    for i in o {
        match seen.get_mut(i) {
            Some(s) if !*s => *s = true,
            _ => return false,
        }
    }
    true
}

pub(crate) fn permute(slot: [f32; 3], order: [usize; 3]) -> [f32; 3] {
    let mut m = slot;
    for (i, &axis) in order.iter().enumerate() {
        if let (Some(dst), Some(src)) = (m.get_mut(axis), slot.get(i)) {
            *dst = *src;
        }
    }
    m
}

/// Slot order for an axis mode at +0x6C (GTA6-Asset-Info §7.2, corrected
/// 2026-11-20): 2 = identity, 1 = slots hold (z, x, y), 0 = slots hold
/// (z, y, x). Other values are unknown.
pub fn axis_order_for_mode(mode: u32) -> Option<[usize; 3]> {
    match mode {
        2 => Some([0, 1, 2]),
        1 => Some([2, 0, 1]),
        0 => Some([2, 1, 0]),
        _ => None,
    }
}

/// Dequantise one u64 position word into slot space (before the axis
/// permutation). x and y have 24 bits (high 16 bits in the word's middle,
/// low 8 bits in bits 0..16), z has 16 bits.
pub fn dequantize_position(q: u64, base: [f32; 3], step: [f32; 3]) -> [f32; 3] {
    let qx = (((q >> 16) & 0xFFFF) << 8) | (q & 0xFF);
    let qy = (((q >> 32) & 0xFFFF) << 8) | ((q >> 8) & 0xFF);
    let qz = q >> 48;
    [
        qx as f32 * step[0] + base[0],
        qy as f32 * step[1] + base[1],
        qz as f32 * step[2] + base[2],
    ]
}

/// Encode slot-space integer coordinates (x, y < 2^24, z < 2^16) into the
/// u64 position word. Inverse of the bit layout in [`dequantize_position`];
/// used by tests and tools that build synthetic blobs.
pub fn quantize_position_word(qx: u32, qy: u32, qz: u16) -> u64 {
    let qx = u64::from(qx & 0xFF_FFFF);
    let qy = u64::from(qy & 0xFF_FFFF);
    (u64::from(qz) << 48) | ((qy >> 8) << 32) | ((qx >> 8) << 16) | ((qy & 0xFF) << 8) | (qx & 0xFF)
}

/// Octahedral 10:10 normal (top 12 bits unknown and ignored).
pub fn decode_oct_normal(w: u32) -> [f32; 3] {
    let x = (w & 1023) as f32 / 1023.0 * 2.0 - 1.0;
    let y = ((w >> 10) & 1023) as f32 / 1023.0 * 2.0 - 1.0;
    let z = 1.0 - x.abs() - y.abs();
    let (x, y) = if z < 0.0 {
        (
            (1.0 - y.abs()) * sign_nonzero(x),
            (1.0 - x.abs()) * sign_nonzero(y),
        )
    } else {
        (x, y)
    };
    normalize([x, y, z])
}

/// R10G10B10A2 tangent: xyz = v / 1023 * 2 - 1, A2 = 3 means bitangent sign
/// -1 (GTA6-Asset-Info §7.2). Other A2 values are read as +1.
pub fn decode_tangent(w: u32) -> [f32; 4] {
    let c = |s: u32| ((w >> s) & 1023) as f32 / 1023.0 * 2.0 - 1.0;
    let t = normalize([c(0), c(10), c(20)]);
    let sign = if (w >> 30) == 3 { -1.0 } else { 1.0 };
    [t[0], t[1], t[2], sign]
}

fn sign_nonzero(v: f32) -> f32 {
    if v >= 0.0 { 1.0 } else { -1.0 }
}

pub(crate) fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-12 && len.is_finite() {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 0.0, 1.0]
    }
}

fn f32le(b: [u8; 4]) -> f32 {
    f32::from_le_bytes(b)
}

/// Decode a vertex buffer blob into model-space attributes.
///
/// Fails on a bad magic, a truncated header, or an unusable position
/// stream. Problems with any other stream are recorded in
/// [`VertexData::warnings`] and the stream is skipped.
pub fn decode_vertex_buffer(bytes: &[u8]) -> Result<VertexData> {
    let r = Reader::new(bytes);
    let magic = r
        .u32_at(0)
        .ctx(|| "vertex buffer: reading magic at offset 0".into())?;
    if magic != VERTEX_MAGIC && magic != VERTEX_MAGIC_FLOAT {
        return Err(ModelError::BadVertexMagic { found: magic });
    }
    let float_positions = magic == VERTEX_MAGIC_FLOAT;
    let header_len = if float_positions {
        SLOT_TABLE_END
    } else {
        QUANT_DATA_START
    };
    if bytes.len() < header_len {
        return Err(ModelError::VertexBuffer(format!(
            "blob is {} bytes, header needs 0x{header_len:X}",
            bytes.len()
        )));
    }

    let mut vd = VertexData::default();

    // Read the slot table.
    let mut streams: Vec<StreamInfo> = Vec::new();
    for slot in 0..STREAM_SLOTS {
        let v = r
            .u32_at(slot * 4)
            .ctx(|| format!("vertex buffer: slot {slot} at 0x{:X}", slot * 4))?;
        if v == 0 {
            continue;
        }
        let offset = ((v >> 8) as usize) * 2;
        let fmt = (v & 0xFF) as u8;
        if offset < SLOT_TABLE_END || offset >= bytes.len() {
            if slot == 0 {
                return Err(ModelError::VertexBuffer(format!(
                    "position slot 0 = 0x{v:08X} points to 0x{offset:X}, outside 0x{SLOT_TABLE_END:X}..0x{:X}",
                    bytes.len()
                )));
            }
            vd.warn(format!(
                "slot {slot} = 0x{v:08X}: stream offset 0x{offset:X} outside 0x{SLOT_TABLE_END:X}..0x{:X}; ignored",
                bytes.len()
            ));
            vd.provisional = true;
            continue;
        }
        streams.push(StreamInfo {
            slot: slot as u8,
            format: fmt,
            offset,
            section_len: 0,
            decoded: false,
        });
    }

    // Section lengths: up to the next higher stream offset or blob end.
    let mut offsets: Vec<usize> = streams.iter().map(|s| s.offset).collect();
    offsets.sort_unstable();
    offsets.dedup();
    if offsets.len() != streams.len() {
        vd.warn("two slots share a stream offset".into());
        vd.provisional = true;
    }
    for s in &mut streams {
        let next = offsets
            .iter()
            .copied()
            .find(|&o| o > s.offset)
            .unwrap_or(bytes.len());
        s.section_len = next - s.offset;
    }

    // Position stream and vertex count.
    let Some(pos) = streams.iter().find(|s| s.slot == 0).cloned() else {
        return Err(ModelError::VertexBuffer(
            "no position stream in slot 0".into(),
        ));
    };
    let (pos_size, expected_off) = if float_positions {
        (12usize, FLOAT_DATA_START)
    } else {
        (8usize, QUANT_DATA_START)
    };
    if pos.offset != expected_off {
        vd.warn(format!(
            "position stream at 0x{:X}, expected 0x{expected_off:X}",
            pos.offset
        ));
        vd.provisional = true;
    }
    let vcount = pos.section_len / pos_size;
    if pos.section_len % pos_size != 0 {
        log::debug!(
            "vertex buffer: position section 0x{:X} bytes is not a multiple of {pos_size}; {} bytes padding",
            pos.section_len,
            pos.section_len % pos_size
        );
    }
    if streams.len() == 1 {
        vd.warn(format!(
            "only a position stream; vertex count {vcount} taken from blob length (padding would add vertices)"
        ));
        vd.provisional = true;
    }
    if vcount > MAX_VERTICES {
        return Err(ModelError::TooLarge {
            what: "vertex buffer vertex count",
            requested: vcount as u64,
            limit: MAX_VERTICES as u64,
        });
    }
    vd.vertex_count = vcount;

    // Positions.
    let pos_bytes = r
        .bytes_at(pos.offset, vcount * pos_size)
        .ctx(|| format!("vertex buffer: position stream at 0x{:X}", pos.offset))?;
    if float_positions {
        vd.position_encoding = Some(PositionEncoding::Float);
        let (chunks, _) = pos_bytes.as_chunks::<12>();
        vd.positions = chunks
            .iter()
            .map(|c| {
                let [a0, a1, a2, a3, b0, b1, b2, b3, c0, c1, c2, c3] = *c;
                [
                    f32le([a0, a1, a2, a3]),
                    f32le([b0, b1, b2, b3]),
                    f32le([c0, c1, c2, c3]),
                ]
            })
            .collect();
        vd.axis_order = Some([0, 1, 2]);
    } else {
        let rd3 = |off: usize| -> Result<[f32; 3]> {
            let mut v = [0f32; 3];
            for (i, x) in v.iter_mut().enumerate() {
                *x = r
                    .f32_at(off + i * 4)
                    .ctx(|| format!("vertex buffer: header f32 at 0x{:X}", off + i * 4))?;
            }
            Ok(v)
        };
        let base = rd3(0x60)?;
        let step = rd3(0x70)?;
        let axis_mode = r
            .u32_at(0x6C)
            .ctx(|| "vertex buffer: axis mode at 0x6C".into())?;
        if base.iter().chain(step.iter()).any(|v| !v.is_finite()) {
            return Err(ModelError::VertexBuffer(format!(
                "non-finite position base {base:?} or step {step:?} at 0x60/0x70"
            )));
        }
        vd.position_encoding = Some(PositionEncoding::Quantized {
            base,
            step,
            axis_mode,
        });
        let (chunks, _) = pos_bytes.as_chunks::<8>();
        // Base and step are applied per slot, before the axis permutation
        // (the published formula names them by slot: `x = slot_x * step.x +
        // base.x`).
        vd.positions = chunks
            .iter()
            .map(|c| dequantize_position(u64::from_le_bytes(*c), base, step))
            .collect();
        match axis_order_for_mode(axis_mode) {
            Some(order) => vd.apply_axis_order(order),
            None => {
                vd.warn(format!(
                    "unknown axis mode {axis_mode} at 0x6C; positions left in slot order"
                ));
                vd.provisional = true;
            }
        }
    }
    if let Some(s) = streams.iter_mut().find(|s| s.slot == 0) {
        s.decoded = true;
    }

    // Other streams.
    for i in 0..streams.len() {
        let Some(s) = streams.get(i).cloned() else {
            continue;
        };
        if s.slot == 0 {
            continue;
        }
        let ok = decode_stream(&mut vd, bytes, &s);
        if let Some(st) = streams.get_mut(i) {
            st.decoded = ok;
        }
    }
    vd.streams = streams;
    Ok(vd)
}

/// Decode one non-position stream into `vd`. Returns whether it was used.
fn decode_stream(vd: &mut VertexData, bytes: &[u8], s: &StreamInfo) -> bool {
    let v = vd.vertex_count;
    let what = format!(
        "slot {} fmt 0x{:02X} at 0x{:X} ({} bytes)",
        s.slot, s.format, s.offset, s.section_len
    );

    // Element width: fixed by format, or (format 0x00) from the section.
    let width = match (s.slot, s.format) {
        (8, format::JOINT_SINGLE) => {
            if v == 0 {
                1
            } else if s.section_len >= 2 * v {
                // u8 with up to 16 bytes of padding would also fit when V
                // is tiny; flag that case.
                if s.section_len - v < 16 {
                    vd.warn(format!(
                        "{what}: joint width ambiguous for {v} vertices; read as u16"
                    ));
                    vd.provisional = true;
                }
                2
            } else {
                1
            }
        }
        (_, f) => match element_size(f) {
            Some(n) => n,
            None => {
                vd.warn(format!("{what}: unknown format; stream skipped"));
                vd.provisional = true;
                return false;
            }
        },
    };
    let need = v.saturating_mul(width);
    if s.section_len < need {
        vd.warn(format!(
            "{what}: section too short for {v} vertices x {width} bytes; stream skipped"
        ));
        vd.provisional = true;
        return false;
    }
    let Some(data) = bytes.get(s.offset..s.offset + need) else {
        vd.warn(format!("{what}: out of bounds; stream skipped"));
        return false;
    };
    let u32s = || {
        data.as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
    };
    let rgba = || data.as_chunks::<4>().0.to_vec();

    match (s.slot, s.format) {
        (1, format::RGBA8) => vd.colors = rgba(),
        (12, format::RGBA8) => vd.colors_slot12 = rgba(),
        (13, format::RGBA8) => vd.colors_slot13 = rgba(),
        (2, format::OCT_NORMAL) => vd.normals = u32s().map(decode_oct_normal).collect(),
        (2, format::F32X3) => {
            vd.normals = data
                .as_chunks::<12>()
                .0
                .iter()
                .map(|c| {
                    let [a0, a1, a2, a3, b0, b1, b2, b3, c0, c1, c2, c3] = *c;
                    normalize([
                        f32le([a0, a1, a2, a3]),
                        f32le([b0, b1, b2, b3]),
                        f32le([c0, c1, c2, c3]),
                    ])
                })
                .collect();
        }
        (3 | 4 | 14 | 15, format::S16X2 | format::EIGHT_BYTE) => {
            let set = match s.slot {
                3 => 0,
                4 => 1,
                14 => 2,
                _ => 3,
            };
            let uvs: Vec<[f32; 2]> = if s.format == format::S16X2 {
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| {
                        let [a0, a1, b0, b1] = *c;
                        [
                            f32::from(i16::from_le_bytes([a0, a1])) / 8192.0,
                            f32::from(i16::from_le_bytes([b0, b1])) / 8192.0,
                        ]
                    })
                    .collect()
            } else {
                data.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| {
                        let [a0, a1, a2, a3, b0, b1, b2, b3] = *c;
                        [f32le([a0, a1, a2, a3]), f32le([b0, b1, b2, b3])]
                    })
                    .collect()
            };
            if let Some(dst) = vd.uv_sets.get_mut(set) {
                *dst = uvs;
            }
        }
        (6, format::RGBA8) => vd.weights = rgba(),
        (7, format::RGBA8) => vd.weights2 = rgba(),
        (8, format::JOINT_SINGLE) => {
            if width == 2 {
                vd.joints = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| [u16::from_le_bytes(*c), 0, 0, 0])
                    .collect();
                vd.joint_format = Some(JointFormat::SingleU16);
            } else {
                vd.joints = data.iter().map(|&b| [u16::from(b), 0, 0, 0]).collect();
                vd.joint_format = Some(JointFormat::SingleU8);
            }
        }
        (8 | 9, format::U8X4) => {
            let j: Vec<[u16; 4]> = data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| c.map(u16::from))
                .collect();
            if s.slot == 8 {
                vd.joints = j;
                vd.joint_format = Some(JointFormat::U8x4);
            } else {
                vd.joints2 = j;
            }
        }
        (8, format::U16X4) => {
            vd.joints = data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| {
                    let [a0, a1, b0, b1, c0, c1, d0, d1] = *c;
                    [
                        u16::from_le_bytes([a0, a1]),
                        u16::from_le_bytes([b0, b1]),
                        u16::from_le_bytes([c0, c1]),
                        u16::from_le_bytes([d0, d1]),
                    ]
                })
                .collect();
            vd.joint_format = Some(JointFormat::U16x4);
        }
        (8 | 9, format::HEAD_SKIN) => {
            // GTA6-Asset-Info §7.5 "Ped skinning": slot 9 reads as 4 x u32
            // joints, slot 8 as 4 x (u16, u16) with plausible joints in the
            // low halves. Marked there as inferred, not decoded.
            let j: Vec<[u16; 4]> = data
                .as_chunks::<16>()
                .0
                .iter()
                .map(|c| {
                    let (words, _) = c.as_chunks::<4>();
                    let mut out = [0u16; 4];
                    for (o, w) in out.iter_mut().zip(words) {
                        let v = u32::from_le_bytes(*w);
                        *o = if s.slot == 8 {
                            (v & 0xFFFF) as u16
                        } else {
                            u16::try_from(v).unwrap_or(u16::MAX)
                        };
                    }
                    out
                })
                .collect();
            vd.warn(format!(
                "{what}: head skin format read with an inferred layout"
            ));
            vd.provisional = true;
            if s.slot == 8 {
                vd.joints = j;
                vd.joint_format = Some(JointFormat::HeadSkin);
            } else {
                vd.joints2 = j;
            }
        }
        (11, format::RGB10A2) => vd.tangents = u32s().map(decode_tangent).collect(),
        _ => {
            log::debug!("vertex buffer: {what}: not decoded");
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests;
