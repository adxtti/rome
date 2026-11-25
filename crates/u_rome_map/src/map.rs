//! `CMapData` decoding: entity placements, prop and grass instance batches,
//! and archetype definitions.
//!
//! Sources: GTA6-Asset-Info `formats/09-placements.md` and
//! `formats/10-interiors.md` (facts only). Struct and field names are the
//! case-sensitive JOAAT of the GTA V names; the schema carried by every
//! resource is the ground truth for field offsets and types, and the
//! documented offsets below are only used when the schema lacks a member
//! (which sets the `provisional` flag and records a warning).

use std::collections::BTreeSet;

use log::{debug, warn};
use u_rome_core::{Hash32, Reader, joaat};

use crate::error::{MapError, Result};
use crate::meta::{ArrayData, DataType, MemberDef, MetaResource, ObjRef, StructDef, Value};

/// Struct hashes (JOAAT of the GTA V type names, GTA6-Asset-Info §8).
pub mod structs {
    use u_rome_core::Hash32;

    pub const CMAP_DATA: Hash32 = Hash32(0xD359_3FA6);
    pub const CMAP_TYPES: Hash32 = Hash32(0xD98B_B561);
    pub const CENTITY_DEF: Hash32 = Hash32(0xCE50_1483);
    pub const CBASE_ARCHETYPE_DEF: Hash32 = Hash32(0x82D6_FC83);
    pub const CMLO_ARCHETYPE_DEF: Hash32 = Hash32(0x1050_6455);
    pub const CMLO_INSTANCE_DEF: Hash32 = Hash32(0x09CC_28BE);
    pub const INSTANCED_MAP_DATA: Hash32 = Hash32(0xF14A_15BE);
    pub const PROP_BATCH: Hash32 = Hash32(0xBA04_9770);
    pub const PROP_INSTANCE: Hash32 = Hash32(0xEA6B_3A96);
    pub const GRASS_BATCH: Hash32 = Hash32(0x7C47_5B5D);
    pub const GRASS_INSTANCE: Hash32 = Hash32(0xED86_F522);
}

/// Field-name hashes used by this decoder.
pub mod fields {
    use u_rome_core::{Hash32, joaat};

    pub const NAME: Hash32 = joaat("name");
    pub const PARENT: Hash32 = joaat("parent");
    pub const FLAGS: Hash32 = joaat("flags");
    pub const ENTITIES: Hash32 = joaat("entities");
    pub const INSTANCED_DATA: Hash32 = joaat("instancedData");
    pub const STREAMING_EXTENTS_MIN: Hash32 = joaat("streamingExtentsMin");
    pub const STREAMING_EXTENTS_MAX: Hash32 = joaat("streamingExtentsMax");
    pub const ENTITIES_EXTENTS_MIN: Hash32 = joaat("entitiesExtentsMin");
    pub const ENTITIES_EXTENTS_MAX: Hash32 = joaat("entitiesExtentsMax");
    /// `8bfb6095`, GTA6-Asset-Info §9 names recovered by hash.
    pub const NAME64: Hash32 = joaat("name64");
    /// `e5cdb664`.
    pub const PARENT64: Hash32 = joaat("parent64");
    /// `6c0f9c8c`, *single* match in GTA6-Asset-Info.
    pub const REGION_CODE: Hash32 = joaat("regionCode");

    pub const ARCHETYPE_NAME: Hash32 = joaat("archetypeName");
    pub const POSITION: Hash32 = joaat("position");
    pub const ROTATION: Hash32 = joaat("rotation");
    pub const SCALE_XY: Hash32 = joaat("scaleXY");
    pub const SCALE_Z: Hash32 = joaat("scaleZ");
    pub const GUID: Hash32 = joaat("guid");
    /// `6c8f1715`.
    pub const LOD_LEVEL: Hash32 = joaat("lodLevel");
    /// `d89231bd`.
    pub const PARENT_INDEX: Hash32 = joaat("parentIndex");
    /// `e74c3c23`.
    pub const PARENT_INSTANCE: Hash32 = joaat("parentInstance");
    /// `a687ac89`.
    pub const NUM_CHILDREN: Hash32 = joaat("numChildren");
    /// `bf74dfa7`.
    pub const LOD_DIST: Hash32 = joaat("lodDist");
    /// `ca974bcd`.
    pub const CHILD_LOD_DIST: Hash32 = joaat("childLodDist");
    /// `2691f019`, 0 on every entity (GTA6-Asset-Info §9.2 correction).
    pub const PRIORITY_LEVEL: Hash32 = joaat("priorityLevel");
    /// `84799847`, *single* match; meaning unknown (-1 where no LOD parent).
    pub const PARENT_DEP: Hash32 = joaat("parentDep");

    pub const BB_MIN: Hash32 = joaat("bbMin");
    pub const BB_MAX: Hash32 = joaat("bbMax");
    pub const BS_CENTRE: Hash32 = joaat("bsCentre");
    pub const BS_RADIUS: Hash32 = joaat("bsRadius");
    pub const HD_TEXTURE_DIST: Hash32 = joaat("hdTextureDist");
    pub const TEXTURE_DICTIONARY: Hash32 = joaat("textureDictionary");
    pub const DRAWABLE_DICTIONARY: Hash32 = joaat("drawableDictionary");
    pub const PHYSICS_DICTIONARY: Hash32 = joaat("physicsDictionary");
    pub const ASSET_NAME: Hash32 = joaat("assetName");
}

/// LOD level of a placement (GTA6-Asset-Info §9.2, confirmed values).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LodLevel {
    Hd,
    Lod,
    Slod1,
    Slod2,
    Slod3,
    OrphanHd,
    Slod4,
    /// A value outside 0..=6, or the field was missing.
    Unknown(u32),
}

impl LodLevel {
    pub fn from_raw(v: u32) -> Self {
        match v {
            0 => LodLevel::Hd,
            1 => LodLevel::Lod,
            2 => LodLevel::Slod1,
            3 => LodLevel::Slod2,
            4 => LodLevel::Slod3,
            5 => LodLevel::OrphanHd,
            6 => LodLevel::Slod4,
            other => LodLevel::Unknown(other),
        }
    }

    pub fn raw(self) -> u32 {
        match self {
            LodLevel::Hd => 0,
            LodLevel::Lod => 1,
            LodLevel::Slod1 => 2,
            LodLevel::Slod2 => 3,
            LodLevel::Slod3 => 4,
            LodLevel::OrphanHd => 5,
            LodLevel::Slod4 => 6,
            LodLevel::Unknown(v) => v,
        }
    }

    /// Detail rank: 0 = finest. ORPHAN_HD ranks with HD.
    pub fn rank(self) -> Option<u32> {
        Some(match self {
            LodLevel::Hd | LodLevel::OrphanHd => 0,
            LodLevel::Lod => 1,
            LodLevel::Slod1 => 2,
            LodLevel::Slod2 => 3,
            LodLevel::Slod3 => 4,
            LodLevel::Slod4 => 5,
            LodLevel::Unknown(_) => return None,
        })
    }

    /// The level a LOD parent of this level is expected to have ("every link
    /// goes exactly one level up", GTA6-Asset-Info §9.2). SLOD3 to SLOD4 is
    /// not documented and is assumed. ORPHAN_HD and SLOD4 have no parent.
    pub fn parent_level(self) -> Option<LodLevel> {
        Some(match self {
            LodLevel::Hd => LodLevel::Lod,
            LodLevel::Lod => LodLevel::Slod1,
            LodLevel::Slod1 => LodLevel::Slod2,
            LodLevel::Slod2 => LodLevel::Slod3,
            LodLevel::Slod3 => LodLevel::Slod4,
            LodLevel::OrphanHd | LodLevel::Slod4 | LodLevel::Unknown(_) => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            LodLevel::Hd => "HD",
            LodLevel::Lod => "LOD",
            LodLevel::Slod1 => "SLOD1",
            LodLevel::Slod2 => "SLOD2",
            LodLevel::Slod3 => "SLOD3",
            LodLevel::OrphanHd => "ORPHAN_HD",
            LodLevel::Slod4 => "SLOD4",
            LodLevel::Unknown(_) => "UNKNOWN",
        }
    }

    /// Index 0..=6 by raw value, 7 for unknown (for histograms).
    pub fn slot(self) -> usize {
        match self {
            LodLevel::Unknown(_) => 7,
            l => l.raw() as usize,
        }
    }
}

/// Which struct a placement was decoded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlacementKind {
    /// `CEntityDef`.
    Entity,
    /// `CMloInstanceDef`: an interior instance (rotation stored as is).
    MloInstance,
    /// Another struct with an entity prefix.
    Other(Hash32),
}

/// One placed entity in world space (RAGE Z-up metres).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// `joaat(model name)`.
    pub archetype: Hash32,
    pub position: [f32; 3],
    /// Entity rotation quaternion `[x, y, z, w]`, already un-inverted.
    pub rotation: [f32; 4],
    pub scale_xy: f32,
    pub scale_z: f32,
    pub flags: u32,
    pub lod_level: LodLevel,
    /// LOD slot in the parent map file, or -1.
    pub parent_index: i32,
    /// -1: parent is an entity of the parent map file; >= 0: instance of a prop batch.
    pub parent_instance: i32,
    pub num_children: u32,
    /// -1 means use the archetype's `lodDist`.
    pub lod_dist: f32,
    pub child_lod_dist: f32,
    /// u64 at +0x20, read as a GUID (inferred, GTA6-Asset-Info §9.1).
    pub guid: u64,
    /// `84799847` (`parentDep`?): -1..10, meaning unknown; -1 when absent.
    pub parent_dep: i32,
    pub kind: PlacementKind,
}

impl Placement {
    /// Column-major 4x4 transform: translate * rotate * scale.
    pub fn transform(&self) -> [[f32; 4]; 4] {
        let r = quat_to_mat3(self.rotation);
        let s = [self.scale_xy, self.scale_xy, self.scale_z];
        let mut m = [[0.0f32; 4]; 4];
        for (c, col) in m.iter_mut().take(3).enumerate() {
            for (row, v) in col.iter_mut().take(3).enumerate() {
                *v = r[row][c] * s[c];
            }
        }
        m[3] = [self.position[0], self.position[1], self.position[2], 1.0];
        m
    }
}

/// Quaternion inverse (conjugate divided by the squared norm). A zero or
/// non-finite quaternion becomes the identity.
pub fn invert_quat(q: [f32; 4]) -> [f32; 4] {
    let n = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
    if !n.is_finite() || n < 1.0e-12 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    [-q[0] / n, -q[1] / n, -q[2] / n, q[3] / n]
}

/// Row-major 3x3 rotation matrix of a (normalised) quaternion.
pub fn quat_to_mat3(q: [f32; 4]) -> [[f32; 3]; 3] {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    let [x, y, z, w] = if n.is_finite() && n > 1.0e-6 {
        [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    };
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

/// The four extents of a map file. Order follows GTA V `CMapData`
/// (streaming min/max, then entities min/max); provisional when the schema
/// did not name them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MapExtents {
    pub streaming_min: [f32; 3],
    pub streaming_max: [f32; 3],
    pub entities_min: [f32; 3],
    pub entities_max: [f32; 3],
}

/// One instance of a prop batch (struct `ea6b3a96`, 64 bytes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PropInstance {
    /// Three rows of a 3x4 matrix: rotation x scale in xyz, translation in w.
    pub rows: [[f32; 4]; 3],
    /// +0x34 tint RGBA.
    pub tint: [u8; 4],
    /// +0x30, +0x38: unknown f32 fields.
    pub unknown_30: f32,
    pub unknown_38: f32,
    /// +0x3C u16 and +0x3E u8 x 2, unknown.
    pub unknown_3c: u16,
    pub unknown_3e: [u8; 2],
}

impl PropInstance {
    pub fn translation(&self) -> [f32; 3] {
        [self.rows[0][3], self.rows[1][3], self.rows[2][3]]
    }

    /// Per-axis scale as the length of each basis column (row-vector
    /// interpretation of the rows is inferred).
    pub fn scale(&self) -> [f32; 3] {
        let col = |c: usize| {
            (self.rows[0][c].powi(2) + self.rows[1][c].powi(2) + self.rows[2][c].powi(2)).sqrt()
        };
        [col(0), col(1), col(2)]
    }

    /// Column-major 4x4 transform.
    pub fn transform(&self) -> [[f32; 4]; 4] {
        let mut m = [[0.0f32; 4]; 4];
        for (c, col) in m.iter_mut().enumerate() {
            for (r, v) in col.iter_mut().take(3).enumerate() {
                *v = self.rows[r][c];
            }
        }
        m[3][3] = 1.0;
        m
    }
}

/// A prop instance batch (struct `ba049770`, 368 bytes).
#[derive(Clone, Debug, PartialEq)]
pub struct PropBatch {
    pub archetype: Hash32,
    /// Batch centre from the entity prefix.
    pub position: [f32; 3],
    pub lod_dist: f32,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub instances: Vec<PropInstance>,
    /// True when a documented offset was used instead of the schema.
    pub provisional: bool,
}

/// One grass instance (struct `ed86f522`, 16 bytes), dequantised.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrassInstance {
    /// World position, dequantised inside the batch bbox.
    pub position: [f32; 3],
    /// Unit normal rebuilt from the stored x / y.
    pub normal: [f32; 3],
    /// Three colour bytes. Not a plain tint (low confidence, GTA6-Asset-Info §9.3).
    pub color: [u8; 3],
    /// Scale interpolated in the batch's scale range (inferred).
    pub scale: f32,
    /// Ambient occlusion byte.
    pub ao: u8,
    pub raw_position: [u16; 3],
    pub raw_normal: [u8; 2],
    pub raw_scale: u8,
    pub unknown_0d: u8,
    pub unknown_0e: u16,
}

/// A grass instance batch (struct `7c475b5d`, 416 bytes).
#[derive(Clone, Debug, PartialEq)]
pub struct GrassBatch {
    pub archetype: Hash32,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// +0xA0 vec3; read as (min, max, ?) as in GTA V `ScaleRange` (inferred).
    pub scale_range: [f32; 3],
    pub lod_dist: f32,
    pub instances: Vec<GrassInstance>,
    pub provisional: bool,
}

/// Dequantise a grass position: `min + q / 65535 * (max - min)`.
pub fn dequantise_grass_position(min: [f32; 3], max: [f32; 3], q: [u16; 3]) -> [f32; 3] {
    let f = |i: usize| min[i] + f32::from(q[i]) / 65535.0 * (max[i] - min[i]);
    [f(0), f(1), f(2)]
}

/// Rebuild a unit normal from u8 x / y (`v / 255 * 2 - 1`, z >= 0).
pub fn dequantise_grass_normal(nx: u8, ny: u8) -> [f32; 3] {
    let x = f32::from(nx) / 255.0 * 2.0 - 1.0;
    let y = f32::from(ny) / 255.0 * 2.0 - 1.0;
    let z = (1.0 - x * x - y * y).max(0.0).sqrt();
    [x, y, z]
}

/// One decoded map file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapData {
    /// Name hash (+0x08).
    pub name: Hash32,
    /// Parent map file name hash (+0x68), 0 when none.
    pub parent: Hash32,
    pub flags: u32,
    pub extents: MapExtents,
    /// 64-bit id `name64` / parent `parent64`, when the schema has them.
    pub name64: Option<u64>,
    pub parent64: Option<u64>,
    /// `regionCode` (`6c0f9c8c`), shared by a child file and its LOD files.
    pub region_code: Option<u32>,
    pub entities: Vec<Placement>,
    pub prop_batches: Vec<PropBatch>,
    pub grass_batches: Vec<GrassBatch>,
    /// True when any field relied on an uncertain rule or fallback offset.
    pub provisional: bool,
    /// Diagnostics: fallbacks used, skipped elements, decode failures.
    pub warnings: Vec<String>,
}

/// Field reader for one object: schema first, documented offset second.
struct Fields<'a> {
    meta: &'a MetaResource,
    obj: ObjRef,
    def: Option<&'a StructDef>,
    label: &'static str,
}

impl<'a> Fields<'a> {
    fn new(meta: &'a MetaResource, obj: ObjRef, label: &'static str) -> Self {
        Self {
            meta,
            obj,
            def: meta.schema().struct_def(obj.struct_hash),
            label,
        }
    }

    fn member(&self, name: Hash32) -> Option<&'a MemberDef> {
        self.def.and_then(|d| d.member(name))
    }

    /// Read `name` from the schema; if absent, read `fallback` and note it.
    fn get(
        &self,
        name: Hash32,
        field: &str,
        fallback: Option<(usize, DataType)>,
        notes: &mut Notes,
    ) -> Option<Value> {
        if let Some(m) = self.member(name) {
            match self.meta.read_value(&self.obj, m) {
                Ok(v) => return Some(v),
                Err(e) => {
                    notes.add(format!("{}.{field}: {e}", self.label));
                    return None;
                }
            }
        }
        let (off, ty) = fallback?;
        notes.fallback(format!(
            "{}.{field} not in schema; read at documented +0x{off:X}",
            self.label
        ));
        match self.meta.read_raw(&self.obj, off, ty) {
            Ok(v) => Some(v),
            Err(e) => {
                notes.add(format!("{}.{field}: {e}", self.label));
                None
            }
        }
    }

    /// Like [`Fields::get`] but records a missing field without a fallback.
    fn get_or_note(&self, name: Hash32, field: &str, notes: &mut Notes) -> Option<Value> {
        let v = self.get(name, field, None, notes);
        if v.is_none() && self.member(name).is_none() {
            notes.fallback(format!(
                "{}.{field} not in schema; default used",
                self.label
            ));
        }
        v
    }

    /// Resolve an array member: by name, then by element struct, then by the
    /// member at `doc_offset`, then a raw 0x52 array at `doc_offset`.
    fn array(
        &self,
        name: Option<Hash32>,
        element_struct: Option<Hash32>,
        doc_offset: usize,
        fallback_stride: usize,
        field: &str,
        notes: &mut Notes,
    ) -> Option<ArrayData> {
        let by_name = name
            .and_then(|n| self.member(n))
            .filter(|m| m.data_type().is_array());
        let by_elem = || {
            let def = self.def?;
            let want = element_struct?;
            def.members.iter().find(|m| {
                m.data_type().is_array()
                    && def
                        .element_of(m)
                        .is_some_and(|e| e.extra == want.0 && e.extra != 0)
            })
        };
        let by_off = || {
            self.def
                .and_then(|d| d.member_at(doc_offset as u32))
                .filter(|m| m.data_type().is_array())
        };
        let member = by_name.or_else(by_elem).or_else(by_off);
        let result = match member {
            Some(m) => {
                let element = self.def.and_then(|d| d.element_of(m)).copied();
                self.meta.read_array(
                    &self.obj,
                    m.offset as usize,
                    m.data_type(),
                    element,
                    fallback_stride,
                )
            }
            None => {
                notes.fallback(format!(
                    "{}.{field} not in schema; read as a 0x52 array at documented +0x{doc_offset:X}",
                    self.label
                ));
                self.meta.read_array(
                    &self.obj,
                    doc_offset,
                    DataType::ArrayRefFirst,
                    None,
                    fallback_stride,
                )
            }
        };
        match result {
            Ok(a) => Some(a),
            Err(e) => {
                notes.add(format!("{}.{field}: {e}", self.label));
                None
            }
        }
    }
}

/// Collects deduplicated warnings; `fallback` ones make the result provisional.
#[derive(Default)]
struct Notes {
    set: BTreeSet<String>,
    provisional: bool,
}

impl Notes {
    fn add(&mut self, s: String) {
        if self.set.len() < 256 && !self.set.contains(&s) {
            warn!("map: {s}");
            self.set.insert(s);
        }
        self.provisional = true;
    }

    fn fallback(&mut self, s: String) {
        if self.set.len() < 256 && !self.set.contains(&s) {
            debug!("map: {s}");
            self.set.insert(s);
        }
        self.provisional = true;
    }
}

fn f32_or(v: Option<Value>, d: f32) -> f32 {
    v.and_then(Value::as_f32).unwrap_or(d)
}

fn i32_or(v: Option<Value>, d: i32) -> i32 {
    v.and_then(Value::as_i64)
        .and_then(|x| i32::try_from(x).ok())
        .unwrap_or(d)
}

fn u32_or(v: Option<Value>, d: u32) -> u32 {
    v.and_then(Value::as_u64)
        .and_then(|x| u32::try_from(x).ok())
        .unwrap_or(d)
}

fn hash_or(v: Option<Value>) -> Hash32 {
    v.and_then(Value::as_hash).unwrap_or_default()
}

fn vec3_or(v: Option<Value>) -> [f32; 3] {
    v.and_then(Value::as_vec3).unwrap_or_default()
}

impl MapData {
    /// Decode the `CMapData` of a meta resource.
    ///
    /// Takes the root object when it is a `CMapData`, else the first
    /// `CMapData` instance. Entities that fail to decode are skipped and
    /// listed in [`MapData::warnings`]; only a missing `CMapData` or an
    /// unreadable map header is an error.
    pub fn from_meta(meta: &MetaResource) -> Result<MapData> {
        let obj = meta
            .root()
            .filter(|r| r.struct_hash == structs::CMAP_DATA)
            .or_else(|| meta.objects_of(structs::CMAP_DATA).into_iter().next())
            .ok_or(MapError::NoMapData)?;
        // Make sure the header itself is readable before decoding fields.
        meta.object_bytes(&obj, 0x460)?;
        let mut notes = Notes {
            provisional: meta.provisional,
            ..Notes::default()
        };
        for w in &meta.warnings {
            notes.set.insert(w.clone());
        }
        let f = Fields::new(meta, obj, "CMapData");
        let mut map = MapData {
            name: hash_or(f.get(
                fields::NAME,
                "name",
                Some((0x08, DataType::Hash)),
                &mut notes,
            )),
            parent: hash_or(f.get(
                fields::PARENT,
                "parent",
                Some((0x68, DataType::Hash)),
                &mut notes,
            )),
            flags: u32_or(
                f.get(
                    fields::FLAGS,
                    "flags",
                    Some((0x80, DataType::U32)),
                    &mut notes,
                ),
                0,
            ),
            ..MapData::default()
        };
        map.extents = MapExtents {
            streaming_min: vec3_or(f.get(
                fields::STREAMING_EXTENTS_MIN,
                "streamingExtentsMin",
                Some((0x90, DataType::Vec3Padded)),
                &mut notes,
            )),
            streaming_max: vec3_or(f.get(
                fields::STREAMING_EXTENTS_MAX,
                "streamingExtentsMax",
                Some((0xA0, DataType::Vec3Padded)),
                &mut notes,
            )),
            entities_min: vec3_or(f.get(
                fields::ENTITIES_EXTENTS_MIN,
                "entitiesExtentsMin",
                Some((0xB0, DataType::Vec3Padded)),
                &mut notes,
            )),
            entities_max: vec3_or(f.get(
                fields::ENTITIES_EXTENTS_MAX,
                "entitiesExtentsMax",
                Some((0xC0, DataType::Vec3Padded)),
                &mut notes,
            )),
        };
        // Optional 64-bit ids and region: no fallback, absence is normal.
        let opt = |name| f.member(name).and_then(|m| meta.read_value(&obj, m).ok());
        map.name64 = opt(fields::NAME64).and_then(Value::as_u64);
        map.parent64 = opt(fields::PARENT64).and_then(Value::as_u64);
        map.region_code = opt(fields::REGION_CODE)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());

        // Entities.
        if let Some(arr) = f.array(
            Some(fields::ENTITIES),
            None,
            0xD0,
            8,
            "entities",
            &mut notes,
        ) {
            match meta.array_objects(&arr, Some(structs::CENTITY_DEF)) {
                Ok((objs, skipped)) => {
                    if skipped > 0 {
                        notes.add(format!(
                            "CMapData.entities: {skipped} of {} elements did not resolve",
                            arr.count
                        ));
                    }
                    for o in objs {
                        match o.struct_hash {
                            structs::PROP_BATCH => {
                                if let Some(b) = decode_prop_batch(meta, o, &mut notes) {
                                    map.prop_batches.push(b);
                                }
                            }
                            structs::GRASS_BATCH => {
                                if let Some(b) = decode_grass_batch(meta, o, &mut notes) {
                                    map.grass_batches.push(b);
                                }
                            }
                            _ => match decode_placement(meta, o, &mut notes) {
                                Some(p) => map.entities.push(p),
                                None => notes.add(format!(
                                    "entity of struct {} at {} skipped",
                                    o.struct_hash, o.location
                                )),
                            },
                        }
                    }
                }
                Err(e) => notes.add(format!("CMapData.entities: {e}")),
            }
        }

        decode_instanced_data(meta, &f, &mut map, &mut notes);

        map.provisional = notes.provisional;
        map.warnings = notes.set.into_iter().collect();
        debug!(
            "map {}: {} entities, {} prop batches, {} grass batches, provisional {}",
            map.name,
            map.entities.len(),
            map.prop_batches.len(),
            map.grass_batches.len(),
            map.provisional
        );
        Ok(map)
    }

    /// Number of LOD slots: entities followed by prop batches (GTA6-Asset-Info §9.2).
    pub fn lod_slot_count(&self) -> usize {
        self.entities.len() + self.prop_batches.len()
    }
}

/// Decode the batches listed in `instancedData` (`f14a15be`, CMapData +0x140,
/// arrays at +0x08 / +0x18). When that path yields nothing, any batch blocks
/// in the resource are used instead.
fn decode_instanced_data(
    meta: &MetaResource,
    f: &Fields<'_>,
    map: &mut MapData,
    notes: &mut Notes,
) {
    let mut found_any = !map.prop_batches.is_empty() || !map.grass_batches.is_empty();
    let inst_off = f
        .member(fields::INSTANCED_DATA)
        .filter(|m| m.data_type() == DataType::Struct)
        .or_else(|| {
            f.def.and_then(|d| {
                d.members.iter().find(|m| {
                    m.data_type() == DataType::Struct && m.extra == structs::INSTANCED_MAP_DATA.0
                })
            })
        })
        .map(|m| m.offset as usize);
    let have_schema = meta
        .schema()
        .struct_def(structs::INSTANCED_MAP_DATA)
        .is_some();
    if inst_off.is_some() || have_schema {
        let off = inst_off.unwrap_or(0x140);
        if let Some(loc) = f.obj.location.offset_by(off) {
            let inst = ObjRef {
                location: loc,
                struct_hash: structs::INSTANCED_MAP_DATA,
            };
            let fi = Fields::new(meta, inst, "rage__fwInstancedMapData");
            let mut arrays: Vec<ArrayData> = Vec::new();
            if let Some(def) = fi.def {
                for m in def.members.iter().filter(|m| m.data_type().is_array()) {
                    let element = def.element_of(m).copied();
                    match meta.read_array(&inst, m.offset as usize, m.data_type(), element, 8) {
                        Ok(a) => arrays.push(a),
                        Err(e) => notes.add(format!("rage__fwInstancedMapData array: {e}")),
                    }
                }
            }
            for a in arrays {
                // Embedded-struct elements carry their struct; pointers resolve by block.
                match meta.array_objects(&a, None) {
                    Ok((objs, _)) => {
                        for o in objs {
                            match o.struct_hash {
                                structs::PROP_BATCH => {
                                    if let Some(b) = decode_prop_batch(meta, o, notes) {
                                        map.prop_batches.push(b);
                                        found_any = true;
                                    }
                                }
                                structs::GRASS_BATCH => {
                                    if let Some(b) = decode_grass_batch(meta, o, notes) {
                                        map.grass_batches.push(b);
                                        found_any = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => debug!("map: instanced array skipped: {e}"),
                }
            }
        }
    }
    if found_any {
        return;
    }
    let props = meta.objects_of(structs::PROP_BATCH);
    let grass = meta.objects_of(structs::GRASS_BATCH);
    if props.is_empty() && grass.is_empty() {
        return;
    }
    notes.fallback(format!(
        "batches not reachable from instancedData; using {} prop and {} grass batch blocks directly",
        props.len(),
        grass.len()
    ));
    for o in props {
        if let Some(b) = decode_prop_batch(meta, o, notes) {
            map.prop_batches.push(b);
        }
    }
    for o in grass {
        if let Some(b) = decode_grass_batch(meta, o, notes) {
            map.grass_batches.push(b);
        }
    }
}

/// Decode a `CEntityDef` (or a struct with an entity prefix).
fn decode_placement(meta: &MetaResource, obj: ObjRef, notes: &mut Notes) -> Option<Placement> {
    let kind = match obj.struct_hash {
        structs::CENTITY_DEF => PlacementKind::Entity,
        structs::CMLO_INSTANCE_DEF => PlacementKind::MloInstance,
        other => PlacementKind::Other(other),
    };
    let label = match kind {
        PlacementKind::Entity => "CEntityDef",
        PlacementKind::MloInstance => "CMloInstanceDef",
        PlacementKind::Other(_) => "entity",
    };
    if let PlacementKind::Other(h) = kind {
        let def = meta.schema().struct_def(h);
        if def.is_none_or(|d| {
            d.member(fields::ARCHETYPE_NAME).is_none() && d.member(fields::POSITION).is_none()
        }) {
            return None;
        }
    }
    if let Err(e) = meta.object_bytes(&obj, 0x90) {
        notes.add(format!("{label} at {}: {e}", obj.location));
        return None;
    }
    let f = Fields::new(meta, obj, label);
    let stored = f
        .get(
            fields::ROTATION,
            "rotation",
            Some((0x10, DataType::Vec4)),
            notes,
        )
        .and_then(Value::as_vec4)
        .unwrap_or([0.0, 0.0, 0.0, 1.0]);
    // Entities store the inverse rotation; interior instances store it as is
    // (GTA6-Asset-Info §9.1, §10, both confirmed).
    let rotation = match kind {
        PlacementKind::MloInstance => stored,
        _ => invert_quat(stored),
    };
    Some(Placement {
        archetype: hash_or(f.get(
            fields::ARCHETYPE_NAME,
            "archetypeName",
            Some((0x44, DataType::Hash)),
            notes,
        )),
        position: vec3_or(f.get(
            fields::POSITION,
            "position",
            Some((0x38, DataType::Vec3)),
            notes,
        )),
        rotation,
        scale_xy: f32_or(
            f.get(
                fields::SCALE_XY,
                "scaleXY",
                Some((0x54, DataType::F32)),
                notes,
            ),
            1.0,
        ),
        scale_z: f32_or(
            f.get(
                fields::SCALE_Z,
                "scaleZ",
                Some((0x58, DataType::F32)),
                notes,
            ),
            1.0,
        ),
        flags: u32_or(
            f.get(fields::FLAGS, "flags", Some((0x48, DataType::U32)), notes),
            0,
        ),
        guid: f
            .get(fields::GUID, "guid", Some((0x20, DataType::U64)), notes)
            .and_then(Value::as_u64)
            .unwrap_or(0),
        lod_level: f
            .get_or_note(fields::LOD_LEVEL, "lodLevel", notes)
            .and_then(Value::as_u64)
            .map(|v| LodLevel::from_raw(u32::try_from(v).unwrap_or(u32::MAX)))
            .unwrap_or(LodLevel::Unknown(u32::MAX)),
        parent_index: i32_or(
            f.get_or_note(fields::PARENT_INDEX, "parentIndex", notes),
            -1,
        ),
        parent_instance: i32_or(
            f.get_or_note(fields::PARENT_INSTANCE, "parentInstance", notes),
            -1,
        ),
        num_children: u32_or(f.get_or_note(fields::NUM_CHILDREN, "numChildren", notes), 0),
        // +0x60 is the LOD distance in the prop batch's entity prefix
        // (GTA6-Asset-Info §9.3); used for entities only as a fallback.
        lod_dist: f32_or(
            f.get(
                fields::LOD_DIST,
                "lodDist",
                Some((0x60, DataType::F32)),
                notes,
            ),
            -1.0,
        ),
        child_lod_dist: f32_or(
            f.get_or_note(fields::CHILD_LOD_DIST, "childLodDist", notes),
            0.0,
        ),
        parent_dep: f
            .member(fields::PARENT_DEP)
            .and_then(|m| meta.read_value(&obj, m).ok())
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(-1),
        kind,
    })
}

/// Decode a prop batch (`ba049770`). Offsets from GTA6-Asset-Info §9.3: +0x60
/// LOD distance, +0x90 instances (`ea6b3a96`, 64 B), +0xA0 bbox. The bbox is
/// read as min at +0xA0 and max at +0xB0 (layout inferred).
fn decode_prop_batch(meta: &MetaResource, obj: ObjRef, notes: &mut Notes) -> Option<PropBatch> {
    let mut local = Notes::default();
    if let Err(e) = meta.object_bytes(&obj, 368) {
        notes.add(format!("prop batch at {}: {e}", obj.location));
        return None;
    }
    let f = Fields::new(meta, obj, "rage__fwPropInstanceListDef");
    let archetype = hash_or(f.get(
        fields::ARCHETYPE_NAME,
        "archetypeName",
        Some((0x44, DataType::Hash)),
        &mut local,
    ));
    let position = vec3_or(f.get(
        fields::POSITION,
        "position",
        Some((0x38, DataType::Vec3)),
        &mut local,
    ));
    let lod_dist = f32_or(
        f.get(
            fields::LOD_DIST,
            "lodDist",
            Some((0x60, DataType::F32)),
            &mut local,
        ),
        -1.0,
    );
    let bbox_min = vec3_or(meta.read_raw(&obj, 0xA0, DataType::Vec3Padded).ok());
    let bbox_max = vec3_or(meta.read_raw(&obj, 0xB0, DataType::Vec3Padded).ok());
    local.provisional = true; // bbox layout inferred
    let mut instances = Vec::new();
    if let Some(arr) = f.array(
        None,
        Some(structs::PROP_INSTANCE),
        0x90,
        64,
        "instances",
        &mut local,
    ) && let Some(loc) = arr.location
    {
        let stride = arr.stride.max(64);
        match meta.bytes_at(loc, arr.count.saturating_mul(stride), "prop instances") {
            Ok(bytes) => {
                instances.reserve(arr.count);
                for chunk in bytes.chunks_exact(stride) {
                    if let Some(i) = read_prop_instance(chunk) {
                        instances.push(i);
                    }
                }
            }
            Err(e) => local.add(format!("prop batch instances: {e}")),
        }
    }
    let provisional = local.provisional;
    notes.set.extend(local.set);
    notes.provisional |= provisional;
    Some(PropBatch {
        archetype,
        position,
        lod_dist,
        bbox_min,
        bbox_max,
        instances,
        provisional,
    })
}

fn read_prop_instance(b: &[u8]) -> Option<PropInstance> {
    let r = Reader::new(b);
    let f = |o: usize| r.f32_at(o).ok();
    let row = |o: usize| Some([f(o)?, f(o + 4)?, f(o + 8)?, f(o + 12)?]);
    let tint = r.bytes_at(0x34, 4).ok()?;
    let tail = r.bytes_at(0x3E, 2).ok()?;
    Some(PropInstance {
        rows: [row(0)?, row(16)?, row(32)?],
        unknown_30: f(0x30)?,
        tint: [*tint.first()?, *tint.get(1)?, *tint.get(2)?, *tint.get(3)?],
        unknown_38: f(0x38)?,
        unknown_3c: r.u16_at(0x3C).ok()?,
        unknown_3e: [*tail.first()?, *tail.get(1)?],
    })
}

/// Decode a grass batch (`7c475b5d`). Offsets from GTA6-Asset-Info §9.3:
/// +0x80 bbox (min +0x80, max +0x90 inferred), +0xA0 vec3 scale range, +0xB8
/// LOD distance, +0x178 instances (`ed86f522`, 16 B).
fn decode_grass_batch(meta: &MetaResource, obj: ObjRef, notes: &mut Notes) -> Option<GrassBatch> {
    let mut local = Notes::default();
    if let Err(e) = meta.object_bytes(&obj, 416) {
        notes.add(format!("grass batch at {}: {e}", obj.location));
        return None;
    }
    let f = Fields::new(meta, obj, "rage__fwGrassInstanceListDef");
    let archetype = hash_or(f.get(
        fields::ARCHETYPE_NAME,
        "archetypeName",
        Some((0x44, DataType::Hash)),
        &mut local,
    ));
    let bbox_min = vec3_or(meta.read_raw(&obj, 0x80, DataType::Vec3Padded).ok());
    let bbox_max = vec3_or(meta.read_raw(&obj, 0x90, DataType::Vec3Padded).ok());
    let scale_range = vec3_or(meta.read_raw(&obj, 0xA0, DataType::Vec3).ok());
    let lod_dist = f32_or(meta.read_raw(&obj, 0xB8, DataType::F32).ok(), -1.0);
    local.provisional = true; // bbox and scale-range layout inferred
    let mut instances = Vec::new();
    if let Some(arr) = f.array(
        None,
        Some(structs::GRASS_INSTANCE),
        0x178,
        16,
        "instances",
        &mut local,
    ) && let Some(loc) = arr.location
    {
        let stride = arr.stride.max(16);
        match meta.bytes_at(loc, arr.count.saturating_mul(stride), "grass instances") {
            Ok(bytes) => {
                instances.reserve(arr.count);
                for chunk in bytes.chunks_exact(stride) {
                    if let Some(i) = read_grass_instance(chunk, bbox_min, bbox_max, scale_range) {
                        instances.push(i);
                    }
                }
            }
            Err(e) => local.add(format!("grass batch instances: {e}")),
        }
    }
    let provisional = local.provisional;
    notes.set.extend(local.set);
    notes.provisional |= provisional;
    Some(GrassBatch {
        archetype,
        bbox_min,
        bbox_max,
        scale_range,
        lod_dist,
        instances,
        provisional,
    })
}

/// Decode one 16-byte grass instance: u16 x 3 position, u8 normal x / y,
/// u8 x 3 colour, u8 scale, u8 AO, u8, u16.
pub fn read_grass_instance(
    b: &[u8],
    bbox_min: [f32; 3],
    bbox_max: [f32; 3],
    scale_range: [f32; 3],
) -> Option<GrassInstance> {
    let r = Reader::new(b);
    let q = [r.u16_at(0).ok()?, r.u16_at(2).ok()?, r.u16_at(4).ok()?];
    let byte = |o: usize| r.u8_at(o).ok();
    let nx = byte(6)?;
    let ny = byte(7)?;
    let raw_scale = byte(11)?;
    let t = f32::from(raw_scale) / 255.0;
    Some(GrassInstance {
        position: dequantise_grass_position(bbox_min, bbox_max, q),
        normal: dequantise_grass_normal(nx, ny),
        color: [byte(8)?, byte(9)?, byte(10)?],
        scale: scale_range[0] + t * (scale_range[1] - scale_range[0]),
        ao: byte(12)?,
        raw_position: q,
        raw_normal: [nx, ny],
        raw_scale,
        unknown_0d: byte(13)?,
        unknown_0e: r.u16_at(14).ok()?,
    })
}

/// Which archetype struct a definition came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArchetypeKind {
    Base,
    Mlo,
}

/// An archetype definition (`CBaseArchetypeDef`, 184 B, or the same prefix
/// of `CMloArchetypeDef`). GTA6-Asset-Info §9.4.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchetypeDef {
    pub name: Hash32,
    pub kind: ArchetypeKind,
    pub lod_dist: f32,
    pub bb_min: [f32; 3],
    pub bb_max: [f32; 3],
    pub bs_centre: [f32; 3],
    pub bs_radius: f32,
    pub hd_texture_dist: f32,
    pub texture_dictionary: Hash32,
    /// +0x7C, inferred (0 where checked).
    pub drawable_dictionary: Hash32,
    pub physics_dictionary: Hash32,
    pub asset_name: Hash32,
    pub provisional: bool,
}

impl ArchetypeDef {
    /// Every archetype definition in a meta resource (usually a `CMapTypes` file).
    pub fn all_from_meta(meta: &MetaResource) -> Vec<ArchetypeDef> {
        let mut out = Vec::new();
        for (hash, kind) in [
            (structs::CBASE_ARCHETYPE_DEF, ArchetypeKind::Base),
            (structs::CMLO_ARCHETYPE_DEF, ArchetypeKind::Mlo),
        ] {
            for obj in meta.objects_of(hash) {
                let mut notes = Notes::default();
                if meta.object_bytes(&obj, 184).is_err() {
                    continue;
                }
                let f = Fields::new(meta, obj, "CBaseArchetypeDef");
                let h = |n, s, o, notes: &mut Notes| {
                    hash_or(f.get(n, s, Some((o, DataType::Hash)), notes))
                };
                let def = ArchetypeDef {
                    name: h(fields::NAME, "name", 0x74, &mut notes),
                    kind,
                    lod_dist: f32_or(
                        f.get(
                            fields::LOD_DIST,
                            "lodDist",
                            Some((0x38, DataType::F32)),
                            &mut notes,
                        ),
                        -1.0,
                    ),
                    bb_min: vec3_or(f.get(
                        fields::BB_MIN,
                        "bbMin",
                        Some((0x40, DataType::Vec3)),
                        &mut notes,
                    )),
                    bb_max: vec3_or(f.get(
                        fields::BB_MAX,
                        "bbMax",
                        Some((0x4C, DataType::Vec3)),
                        &mut notes,
                    )),
                    bs_centre: vec3_or(f.get(
                        fields::BS_CENTRE,
                        "bsCentre",
                        Some((0x58, DataType::Vec3)),
                        &mut notes,
                    )),
                    bs_radius: f32_or(
                        f.get(
                            fields::BS_RADIUS,
                            "bsRadius",
                            Some((0x64, DataType::F32)),
                            &mut notes,
                        ),
                        0.0,
                    ),
                    hd_texture_dist: f32_or(
                        f.get(
                            fields::HD_TEXTURE_DIST,
                            "hdTextureDist",
                            Some((0x70, DataType::F32)),
                            &mut notes,
                        ),
                        0.0,
                    ),
                    texture_dictionary: h(
                        fields::TEXTURE_DICTIONARY,
                        "textureDictionary",
                        0x78,
                        &mut notes,
                    ),
                    drawable_dictionary: h(
                        fields::DRAWABLE_DICTIONARY,
                        "drawableDictionary",
                        0x7C,
                        &mut notes,
                    ),
                    physics_dictionary: h(
                        fields::PHYSICS_DICTIONARY,
                        "physicsDictionary",
                        0x80,
                        &mut notes,
                    ),
                    asset_name: h(fields::ASSET_NAME, "assetName", 0x88, &mut notes),
                    provisional: notes.provisional,
                };
                out.push(def);
            }
        }
        out
    }
}

/// JOAAT of a model name, as stored in [`Placement::archetype`].
pub fn archetype_hash(model_name: &str) -> Hash32 {
    joaat(model_name)
}

#[cfg(test)]
pub(crate) mod tests;
