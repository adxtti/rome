//! Minimal generic reader for self-describing meta resources (`0DRP`, PRD0
//! type 0x7a).
//!
//! # Layout
//!
//! Sources: GTA6-Asset-Info `formats/02-resources.md` and `formats/08-meta.md`
//! (facts only), rage9-re `docs/SCRIPT_FORMAT.md` (MIT OR Apache-2.0).
//!
//! A meta resource is a paged RAGE resource. The system page holds the header
//! and the schema tables; instance blocks live in the system or the graphics
//! page. Pointers are 64-bit: `0x5xxxxxxx` points into the system page and
//! `0x6xxxxxxx` into the graphics page, the low 28 bits being the offset.
//!
//! | Off  | Meaning |
//! |------|---------|
//! | 0x08 | u64 page-map pointer |
//! | 0x10 | `0DRP` magic, u8 resource type 0x7a at 0x14 |
//! | 0x20 | u32 root count (GTA6-Asset-Info) or 1-based root block index (rage9-re `LRSC` +0x10) |
//! | 0x28 | ptr to the struct table (32 B per struct) |
//! | 0x30 | ptr to the enum table (24 B per enum) |
//! | 0x38 | ptr to the instance-block table (16 B per block) |
//! | 0x48 | u16 struct count, u16 enum count (+0x4A), u16 block count (+0x4C) |
//!
//! References between objects are either plain pointers or *pool references*:
//! a u64 with the 1-based block index in the low bits and the byte offset
//! inside the block in bits 20 and up. Pool references take priority over
//! pointers when both readings are possible (GTA6-Asset-Info §8, confirmed on
//! 14,471 ambiguous values).

use std::collections::HashMap;

use log::{debug, warn};
use u_rome_core::{ByteError, Hash32, Reader};

use crate::error::{MapError, Result};

/// Offset of the `0DRP` magic inside the resource.
pub const MAGIC_OFFSET: usize = 0x10;
/// The PRD0 resource type of meta resources.
pub const META_RESOURCE_TYPE: u8 = 0x7a;
/// Field name hash of an anonymous array element member.
pub const ANONYMOUS_MEMBER: Hash32 = Hash32(0x100);

const HEADER_LEN: usize = 0x50;
const STRUCT_RECORD: usize = 32;
const MEMBER_RECORD: usize = 16;
const ENUM_RECORD: usize = 24;
const ENUM_VALUE_RECORD: usize = 16;
const BLOCK_RECORD: usize = 16;

const MAX_MEMBERS_PER_STRUCT: usize = 4096;
const MAX_ENUM_VALUES: usize = 65536;
/// Hard cap on the number of elements one array may expand to.
pub const MAX_ARRAY_ELEMENTS: usize = 1 << 22;
/// Hard cap on the number of objects iterated in one block.
pub const MAX_BLOCK_OBJECTS: usize = 1 << 22;

/// Mask for the block index of a pool reference.
///
/// GTA6-Asset-Info §8 gives bits 0-12, a later note and rage9-re give bits
/// 0-11. Bits 12-19 are otherwise unused, so the 13-bit mask reads every
/// 12-bit reference identically; a reference that needs bit 12 is flagged
/// provisional.
const POOL_INDEX_MASK: u64 = 0x1FFF;
const POOL_INDEX_BIT12: u64 = 0x1000;
const POOL_RESERVED_MASK: u64 = 0xF_E000;
const POOL_OFFSET_SHIFT: u32 = 20;

/// The two pages of a RAGE resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Page {
    /// CPU structures (`0x5xxxxxxx` pointers).
    System,
    /// GPU data (`0x6xxxxxxx` pointers).
    Graphics,
}

/// A byte position inside one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Location {
    pub page: Page,
    pub offset: usize,
}

impl Location {
    pub fn offset_by(self, n: usize) -> Option<Location> {
        Some(Location {
            page: self.page,
            offset: self.offset.checked_add(n)?,
        })
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let p = match self.page {
            Page::System => "sys",
            Page::Graphics => "gfx",
        };
        write!(f, "{p}+0x{:X}", self.offset)
    }
}

/// Interpretation of a member type code (GTA6-Asset-Info §8 "Type codes",
/// rage9-re §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    /// 0x01 (rage9-re).
    Bool,
    /// 0x10.
    S8,
    /// 0x11.
    U8,
    /// 0x12.
    S16,
    /// 0x13.
    U16,
    /// 0x14: s32 per rage9-re ("s32/f32-bits"); not listed by GTA6-Asset-Info.
    S32,
    /// 0x15.
    U32,
    /// 0x08 (GTA6-Asset-Info) and 0x16 (rage9-re); 0x4f with sub-type 5.
    U64,
    /// 0x4f with sub-type 0x0f (inferred).
    U128,
    /// 0x20, IEEE half.
    F16,
    /// 0x21.
    F32,
    /// 0x32, 12 bytes.
    Vec3,
    /// 0x33, 12 bytes of data padded to 16.
    Vec3Padded,
    /// 0x34, vec4 or quaternion.
    Vec4,
    /// 0x4a, u32 JOAAT hash.
    Hash,
    /// 0x60, u8 enum.
    Enum8,
    /// 0x65, bitset (reference = bit count).
    Bitset,
    /// 0x52: `{u64 ref, u16 count, u16 capacity}`.
    ArrayRefFirst,
    /// 0x5c: `{u32 count, u32 capacity, u64 ref}`.
    ArrayCountFirst,
    /// 0x44, inline chars or u64 reference into a char pool.
    String,
    /// 0x07, u64 pointer (polymorphic when `extra == 0`).
    Pointer,
    /// 0x05, embedded struct (`extra` = struct hash).
    Struct,
    /// 0x71, map (24 bytes).
    Map,
    /// Any other code.
    Unknown(u8),
}

impl DataType {
    pub fn from_code(code: u8, sub_type: u8) -> Self {
        match code {
            0x01 => DataType::Bool,
            0x10 => DataType::S8,
            0x11 => DataType::U8,
            0x12 => DataType::S16,
            0x13 => DataType::U16,
            0x14 => DataType::S32,
            0x15 => DataType::U32,
            0x08 | 0x16 => DataType::U64,
            0x4f if sub_type == 0x0f => DataType::U128,
            0x4f => DataType::U64,
            0x20 => DataType::F16,
            0x21 => DataType::F32,
            0x32 => DataType::Vec3,
            0x33 => DataType::Vec3Padded,
            0x34 => DataType::Vec4,
            0x4a => DataType::Hash,
            0x60 => DataType::Enum8,
            0x65 => DataType::Bitset,
            0x52 => DataType::ArrayRefFirst,
            0x5c => DataType::ArrayCountFirst,
            0x44 => DataType::String,
            0x07 => DataType::Pointer,
            0x05 => DataType::Struct,
            0x71 => DataType::Map,
            other => DataType::Unknown(other),
        }
    }

    /// Fixed byte size, when it does not depend on the schema.
    pub fn fixed_size(self) -> Option<usize> {
        Some(match self {
            DataType::Bool | DataType::S8 | DataType::U8 | DataType::Enum8 => 1,
            DataType::S16 | DataType::U16 | DataType::F16 => 2,
            DataType::S32 | DataType::U32 | DataType::F32 | DataType::Hash => 4,
            DataType::U64 | DataType::Pointer => 8,
            DataType::Vec3 => 12,
            DataType::Vec3Padded | DataType::Vec4 | DataType::U128 => 16,
            DataType::ArrayRefFirst | DataType::ArrayCountFirst => 16,
            DataType::Map => 24,
            DataType::Bitset | DataType::String | DataType::Struct | DataType::Unknown(_) => {
                return None;
            }
        })
    }

    pub fn is_array(self) -> bool {
        matches!(self, DataType::ArrayRefFirst | DataType::ArrayCountFirst)
    }
}

/// One member record of a struct schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberDef {
    /// Case-sensitive JOAAT of the field name; [`ANONYMOUS_MEMBER`] for array elements.
    pub name: Hash32,
    pub offset: u32,
    pub type_code: u8,
    pub sub_type: u8,
    /// Arrays: index of the element member. Bitsets: bit count.
    pub reference: u16,
    /// Struct hash for embedded structs and typed pointers, enum hash for enums.
    pub extra: u32,
}

impl MemberDef {
    pub fn data_type(&self) -> DataType {
        DataType::from_code(self.type_code, self.sub_type)
    }
}

/// One struct schema record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDef {
    pub name: Hash32,
    pub signature: u32,
    pub flags: u32,
    pub size: u32,
    pub members: Vec<MemberDef>,
}

impl StructDef {
    /// The first named member with this field hash.
    pub fn member(&self, name: Hash32) -> Option<&MemberDef> {
        self.members
            .iter()
            .find(|m| m.name == name && m.name != ANONYMOUS_MEMBER)
    }

    /// The first named member at this offset.
    pub fn member_at(&self, offset: u32) -> Option<&MemberDef> {
        self.members
            .iter()
            .find(|m| m.offset == offset && m.name != ANONYMOUS_MEMBER)
    }

    /// The element member of an array member.
    ///
    /// The array's `reference` is the element's index in this struct's member
    /// list (rage9-re §2.2). If it does not point at an anonymous member, the
    /// anonymous member right before the array is used.
    pub fn element_of(&self, array: &MemberDef) -> Option<&MemberDef> {
        if let Some(m) = self.members.get(usize::from(array.reference))
            && m.name == ANONYMOUS_MEMBER
        {
            return Some(m);
        }
        let pos = self.members.iter().position(|m| m == array)?;
        let prev = self.members.get(pos.checked_sub(1)?)?;
        (prev.name == ANONYMOUS_MEMBER).then_some(prev)
    }
}

/// One enum schema record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDef {
    pub name: Hash32,
    pub signature: u32,
    /// `(value, name hash)` pairs.
    pub values: Vec<(u64, Hash32)>,
}

/// The struct and enum tables of one resource.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub structs: Vec<StructDef>,
    pub enums: Vec<EnumDef>,
    by_name: HashMap<Hash32, usize>,
}

impl Schema {
    pub fn new(structs: Vec<StructDef>, enums: Vec<EnumDef>) -> Self {
        let mut by_name = HashMap::with_capacity(structs.len());
        for (i, s) in structs.iter().enumerate() {
            by_name.entry(s.name).or_insert(i);
        }
        Self {
            structs,
            enums,
            by_name,
        }
    }

    pub fn struct_def(&self, name: Hash32) -> Option<&StructDef> {
        self.structs.get(*self.by_name.get(&name)?)
    }

    pub fn enum_def(&self, name: Hash32) -> Option<&EnumDef> {
        self.enums.iter().find(|e| e.name == name)
    }

    /// Name hash of an enum value, if the enum lists it.
    pub fn enum_value_name(&self, enum_name: Hash32, value: u64) -> Option<Hash32> {
        self.enum_def(enum_name)?
            .values
            .iter()
            .find(|(v, _)| *v == value)
            .map(|(_, n)| *n)
    }
}

/// One entry of the instance-block table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// 1-based index, as used by pool references.
    pub index: u16,
    /// Struct hash of the objects, or a primitive type code (a pool).
    pub type_hash: Hash32,
    /// Byte size of the block.
    pub size: u32,
    /// The raw pointer from the table.
    pub raw_ptr: u64,
    /// Where the block's bytes are, when the pointer resolves inside a page.
    pub location: Option<Location>,
}

/// Decoded header fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetaHeader {
    pub page_map: u64,
    pub resource_type: u8,
    /// +0x20: root count (GTA6-Asset-Info) or 1-based root block (rage9-re). Provisional.
    pub root_field: u32,
    pub struct_table: u64,
    pub enum_table: u64,
    pub block_table: u64,
    pub struct_count: u16,
    pub enum_count: u16,
    pub block_count: u16,
}

/// A typed object reference: where the object is and which struct it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjRef {
    pub location: Location,
    pub struct_hash: Hash32,
}

/// A decoded scalar or vector value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    UInt(u64),
    Float(f32),
    Hash(Hash32),
    Vec3([f32; 3]),
    Vec4([f32; 4]),
}

impl Value {
    pub fn as_i64(self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(v),
            Value::UInt(v) => i64::try_from(v).ok(),
            Value::Hash(h) => Some(i64::from(h.0)),
            Value::Float(f) if f.is_finite() => Some(f as i64),
            _ => None,
        }
    }

    pub fn as_u64(self) -> Option<u64> {
        match self {
            Value::Int(v) => u64::try_from(v).ok(),
            Value::UInt(v) => Some(v),
            Value::Hash(h) => Some(u64::from(h.0)),
            _ => None,
        }
    }

    pub fn as_f32(self) -> Option<f32> {
        match self {
            Value::Float(f) => Some(f),
            Value::Int(v) => Some(v as f32),
            Value::UInt(v) => Some(v as f32),
            _ => None,
        }
    }

    pub fn as_hash(self) -> Option<Hash32> {
        match self {
            Value::Hash(h) => Some(h),
            Value::UInt(v) => u32::try_from(v).ok().map(Hash32),
            Value::Int(v) => u32::try_from(v).ok().map(Hash32),
            _ => None,
        }
    }

    pub fn as_vec3(self) -> Option<[f32; 3]> {
        match self {
            Value::Vec3(v) => Some(v),
            Value::Vec4([x, y, z, _]) => Some([x, y, z]),
            _ => None,
        }
    }

    pub fn as_vec4(self) -> Option<[f32; 4]> {
        match self {
            Value::Vec4(v) => Some(v),
            _ => None,
        }
    }
}

/// An array member resolved to its element storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayData {
    /// Start of the elements; `None` for an empty array.
    pub location: Option<Location>,
    pub count: usize,
    pub stride: usize,
    /// The element member, when the schema has one.
    pub element: Option<MemberDef>,
    /// The raw reference value.
    pub raw_ref: u64,
}

/// A parsed meta resource: pages, schema and instance blocks.
#[derive(Clone, Debug)]
pub struct MetaResource {
    system: Vec<u8>,
    graphics: Vec<u8>,
    header: MetaHeader,
    schema: Schema,
    blocks: Vec<Block>,
    /// Block indices sorted by (page, offset) for containment lookups.
    sorted_blocks: Vec<usize>,
    /// True when part of the parse relied on an uncertain rule.
    pub provisional: bool,
    /// Human-readable notes on uncertain decisions, for diagnostics.
    pub warnings: Vec<String>,
}

fn rd<T>(r: std::result::Result<T, ByteError>, ctx: impl FnOnce() -> String) -> Result<T> {
    r.map_err(|e| MapError::bytes(ctx(), e))
}

fn align_up(v: usize, a: usize) -> Option<usize> {
    let r = v % a;
    if r == 0 {
        Some(v)
    } else {
        v.checked_add(a - r)
    }
}

/// Decode a plain pointer (no pool reading) into a page location.
pub fn decode_pointer(value: u64) -> Option<Location> {
    if value >> 32 != 0 {
        return None;
    }
    let offset = usize::try_from(value & 0x0FFF_FFFF).ok()?;
    match value >> 28 {
        5 => Some(Location {
            page: Page::System,
            offset,
        }),
        6 => Some(Location {
            page: Page::Graphics,
            offset,
        }),
        _ => None,
    }
}

/// Split a pool reference into (1-based block index, byte offset), without
/// checking that the block exists.
pub fn split_pool_ref(value: u64) -> Option<(u16, usize)> {
    if value & POOL_RESERVED_MASK != 0 {
        return None;
    }
    let index = u16::try_from(value & POOL_INDEX_MASK).ok()?;
    if index == 0 {
        return None;
    }
    let offset = usize::try_from(value >> POOL_OFFSET_SHIFT).ok()?;
    Some((index, offset))
}

/// Build a pool reference (1-based block index, byte offset).
pub fn make_pool_ref(block_index: u16, offset: usize) -> u64 {
    u64::from(block_index) | ((offset as u64) << POOL_OFFSET_SHIFT)
}

/// IEEE 754 half to f32.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1F);
    let frac = u32::from(h & 0x3FF);
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, f) => {
            // Subnormal: normalise.
            let mut e: i32 = -14;
            let mut f = f;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            let f = f & 0x3FF;
            sign | (((e + 127) as u32) << 23) | (f << 13)
        }
        (0x1F, 0) => sign | 0x7F80_0000,
        (0x1F, f) => sign | 0x7FC0_0000 | (f << 13),
        (e, f) => sign | ((e + 112) << 23) | (f << 13),
    };
    f32::from_bits(bits)
}

impl MetaResource {
    /// Parse a meta resource from one buffer holding the system page followed
    /// by the graphics page.
    ///
    /// The page boundary is not stored in the bytes this crate sees, so it is
    /// inferred when any block lives in the graphics page (see
    /// [`MetaResource::parse_pages`] to pass the pages separately). An inferred
    /// boundary sets [`MetaResource::provisional`] and adds a warning.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let header = Self::read_header(bytes)?;
        let probe = Self::build(bytes.to_vec(), Vec::new(), header, false)?;
        let gfx_extent = probe.graphics_extent();
        if gfx_extent == 0 {
            return Ok(probe);
        }
        let sys_extent = probe.system_extent();
        let base = infer_graphics_base(&probe, bytes.len(), sys_extent, gfx_extent);
        let Some(base) = base else {
            warn!(
                "meta: no graphics page fits: buffer {} bytes, system data ends at 0x{sys_extent:X}, graphics extent 0x{gfx_extent:X}",
                bytes.len()
            );
            let mut probe = probe;
            probe.provisional = true;
            probe.warnings.push(format!(
                "graphics page not found: buffer {} bytes, system extent 0x{sys_extent:X}, graphics extent 0x{gfx_extent:X}",
                bytes.len()
            ));
            return Ok(probe);
        };
        debug!(
            "meta: graphics page inferred at buffer offset 0x{base:X} (system extent 0x{sys_extent:X}, graphics extent 0x{gfx_extent:X}, buffer {} bytes)",
            bytes.len()
        );
        let (sys, gfx) = bytes.split_at(base.min(bytes.len()));
        let mut meta = Self::build(sys.to_vec(), gfx.to_vec(), header, true)?;
        meta.provisional = true;
        meta.warnings.push(format!(
            "graphics page boundary inferred at 0x{base:X}; pass the pages separately to MetaResource::parse_pages for an exact split"
        ));
        Ok(meta)
    }

    /// Parse a meta resource whose system and graphics pages are known.
    pub fn parse_pages(system: &[u8], graphics: &[u8]) -> Result<Self> {
        let header = Self::read_header(system)?;
        Self::build(system.to_vec(), graphics.to_vec(), header, true)
    }

    fn read_header(bytes: &[u8]) -> Result<MetaHeader> {
        let r = Reader::new(bytes);
        let magic = r.bytes_at(MAGIC_OFFSET, 4).unwrap_or(&[]);
        if magic != b"0DRP" {
            return Err(MapError::BadMagic {
                found: magic.to_vec(),
            });
        }
        let ctx = |f: &str| format!("meta header field {f}");
        let resource_type = rd(r.u8_at(0x14), || ctx("type@0x14"))?;
        if resource_type != META_RESOURCE_TYPE {
            return Err(MapError::BadResourceType {
                found: resource_type,
            });
        }
        if bytes.len() < HEADER_LEN {
            return Err(MapError::bytes(
                "meta header",
                ByteError::OutOfBounds {
                    offset: 0,
                    len: HEADER_LEN,
                    size: bytes.len(),
                },
            ));
        }
        Ok(MetaHeader {
            page_map: rd(r.u64_at(0x08), || ctx("page map@0x08"))?,
            resource_type,
            root_field: rd(r.u32_at(0x20), || ctx("root@0x20"))?,
            struct_table: rd(r.u64_at(0x28), || ctx("struct table@0x28"))?,
            enum_table: rd(r.u64_at(0x30), || ctx("enum table@0x30"))?,
            block_table: rd(r.u64_at(0x38), || ctx("block table@0x38"))?,
            struct_count: rd(r.u16_at(0x48), || ctx("struct count@0x48"))?,
            enum_count: rd(r.u16_at(0x4A), || ctx("enum count@0x4A"))?,
            block_count: rd(r.u16_at(0x4C), || ctx("block count@0x4C"))?,
        })
    }

    fn build(
        system: Vec<u8>,
        graphics: Vec<u8>,
        header: MetaHeader,
        warn_blocks: bool,
    ) -> Result<Self> {
        let mut warnings = Vec::new();
        let structs = Self::read_structs(&system, &graphics, &header)?;
        let enums = Self::read_enums(&system, &graphics, &header)?;
        let schema = Schema::new(structs, enums);
        let blocks = Self::read_blocks(&system, &graphics, &header)?;
        for b in &blocks {
            let fits = b.location.is_some_and(|loc| {
                let len = match loc.page {
                    Page::System => system.len(),
                    Page::Graphics => graphics.len(),
                };
                loc.offset
                    .checked_add(b.size as usize)
                    .is_some_and(|end| end <= len)
            });
            if !fits && warn_blocks {
                warn!(
                    "meta: block {} (type {}, {} bytes, ptr 0x{:X}) lies outside its page",
                    b.index, b.type_hash, b.size, b.raw_ptr
                );
                warnings.push(format!(
                    "block {} (type {}) ptr 0x{:X} size {} outside its page",
                    b.index, b.type_hash, b.raw_ptr, b.size
                ));
            }
        }
        let mut sorted_blocks: Vec<usize> = (0..blocks.len())
            .filter(|&i| blocks.get(i).is_some_and(|b| b.location.is_some()))
            .collect();
        sorted_blocks.sort_by_key(|&i| blocks.get(i).and_then(|b| b.location));
        Ok(Self {
            system,
            graphics,
            header,
            schema,
            blocks,
            sorted_blocks,
            provisional: false,
            warnings,
        })
    }

    fn page_of<'a>(system: &'a [u8], graphics: &'a [u8], page: Page) -> &'a [u8] {
        match page {
            Page::System => system,
            Page::Graphics => graphics,
        }
    }

    fn table_reader<'a>(
        system: &'a [u8],
        graphics: &'a [u8],
        ptr: u64,
        count: usize,
        record: usize,
        what: &'static str,
    ) -> Result<Option<Reader<'a>>> {
        if count == 0 {
            return Ok(None);
        }
        let loc = decode_pointer(ptr).ok_or_else(|| MapError::BadReference {
            context: format!("{what} pointer ({count} records)"),
            value: ptr,
        })?;
        let page = Self::page_of(system, graphics, loc.page);
        let len = count.saturating_mul(record);
        if loc.offset.checked_add(len).is_none_or(|e| e > page.len()) {
            return Err(MapError::OutOfPage {
                context: format!("{what} ({count} records of {record} bytes)"),
                page: loc.page,
                offset: loc.offset,
                len,
                page_len: page.len(),
            });
        }
        let mut r = Reader::new(page);
        r.seek(loc.offset);
        Ok(Some(r))
    }

    fn read_structs(system: &[u8], graphics: &[u8], header: &MetaHeader) -> Result<Vec<StructDef>> {
        let count = usize::from(header.struct_count);
        let Some(mut r) = Self::table_reader(
            system,
            graphics,
            header.struct_table,
            count,
            STRUCT_RECORD,
            "struct table",
        )?
        else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let at = r.pos();
            let ctx = || format!("struct record {i} at sys+0x{at:X}");
            let name = Hash32(rd(r.u32(), ctx)?);
            let signature = rd(r.u32(), ctx)?;
            let flags = rd(r.u32(), ctx)?;
            let _zero = rd(r.u32(), ctx)?;
            let members_ptr = rd(r.u64(), ctx)?;
            let size = rd(r.u32(), ctx)?;
            let _pad = rd(r.u16(), ctx)?;
            let member_count = usize::from(rd(r.u16(), ctx)?);
            if member_count > MAX_MEMBERS_PER_STRUCT {
                return Err(MapError::TooLarge {
                    what: "struct member count",
                    count: member_count,
                    limit: MAX_MEMBERS_PER_STRUCT,
                });
            }
            let mut members = Vec::with_capacity(member_count);
            if let Some(mut m) = Self::table_reader(
                system,
                graphics,
                members_ptr,
                member_count,
                MEMBER_RECORD,
                "member table",
            )? {
                for j in 0..member_count {
                    let mat = m.pos();
                    let mctx = || format!("member {j} of struct {name} at 0x{mat:X}");
                    members.push(MemberDef {
                        name: Hash32(rd(m.u32(), mctx)?),
                        offset: rd(m.u32(), mctx)?,
                        type_code: rd(m.u8(), mctx)?,
                        sub_type: rd(m.u8(), mctx)?,
                        reference: rd(m.u16(), mctx)?,
                        extra: rd(m.u32(), mctx)?,
                    });
                }
            }
            out.push(StructDef {
                name,
                signature,
                flags,
                size,
                members,
            });
        }
        Ok(out)
    }

    fn read_enums(system: &[u8], graphics: &[u8], header: &MetaHeader) -> Result<Vec<EnumDef>> {
        let count = usize::from(header.enum_count);
        let Some(mut r) = Self::table_reader(
            system,
            graphics,
            header.enum_table,
            count,
            ENUM_RECORD,
            "enum table",
        )?
        else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let at = r.pos();
            let ctx = || format!("enum record {i} at 0x{at:X}");
            let name = Hash32(rd(r.u32(), ctx)?);
            let signature = rd(r.u32(), ctx)?;
            let values_ptr = rd(r.u64(), ctx)?;
            let value_count = rd(r.u32(), ctx)? as usize;
            let _zero = rd(r.u32(), ctx)?;
            if value_count > MAX_ENUM_VALUES {
                return Err(MapError::TooLarge {
                    what: "enum value count",
                    count: value_count,
                    limit: MAX_ENUM_VALUES,
                });
            }
            let mut values = Vec::with_capacity(value_count);
            if let Some(mut v) = Self::table_reader(
                system,
                graphics,
                values_ptr,
                value_count,
                ENUM_VALUE_RECORD,
                "enum values",
            )? {
                for j in 0..value_count {
                    let vctx = || format!("value {j} of enum {name}");
                    let value = rd(v.u64(), vctx)?;
                    let vname = rd(v.u64(), vctx)?;
                    values.push((value, Hash32(vname as u32)));
                }
            }
            out.push(EnumDef {
                name,
                signature,
                values,
            });
        }
        Ok(out)
    }

    fn read_blocks(system: &[u8], graphics: &[u8], header: &MetaHeader) -> Result<Vec<Block>> {
        let count = usize::from(header.block_count);
        let Some(mut r) = Self::table_reader(
            system,
            graphics,
            header.block_table,
            count,
            BLOCK_RECORD,
            "instance block table",
        )?
        else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let at = r.pos();
            let ctx = || format!("block record {} at 0x{at:X}", i + 1);
            let type_hash = Hash32(rd(r.u32(), ctx)?);
            let size = rd(r.u32(), ctx)?;
            let raw_ptr = rd(r.u64(), ctx)?;
            out.push(Block {
                index: u16::try_from(i + 1).unwrap_or(u16::MAX),
                type_hash,
                size,
                raw_ptr,
                location: decode_pointer(raw_ptr),
            });
        }
        Ok(out)
    }

    /// End of everything the header tables and system blocks occupy.
    fn system_extent(&self) -> usize {
        let mut end = HEADER_LEN;
        let mut extend = |ptr: u64, len: usize| {
            if let Some(loc) = decode_pointer(ptr)
                && loc.page == Page::System
            {
                end = end.max(loc.offset.saturating_add(len));
            }
        };
        let h = &self.header;
        extend(h.struct_table, usize::from(h.struct_count) * STRUCT_RECORD);
        extend(h.enum_table, usize::from(h.enum_count) * ENUM_RECORD);
        extend(h.block_table, usize::from(h.block_count) * BLOCK_RECORD);
        for b in &self.blocks {
            extend(b.raw_ptr, b.size as usize);
        }
        end
    }

    /// End of the furthest graphics block (0 when there is none).
    fn graphics_extent(&self) -> usize {
        self.blocks
            .iter()
            .filter_map(|b| {
                let loc = b.location?;
                (loc.page == Page::Graphics).then(|| loc.offset.saturating_add(b.size as usize))
            })
            .max()
            .unwrap_or(0)
    }

    pub fn header(&self) -> &MetaHeader {
        &self.header
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Block by 1-based index.
    pub fn block(&self, index: u16) -> Option<&Block> {
        self.blocks.get(usize::from(index).checked_sub(1)?)
    }

    pub fn page(&self, page: Page) -> &[u8] {
        Self::page_of(&self.system, &self.graphics, page)
    }

    /// Bytes at a location, bounds-checked.
    pub fn bytes_at(&self, loc: Location, len: usize, context: &str) -> Result<&[u8]> {
        let page = self.page(loc.page);
        match loc.offset.checked_add(len) {
            Some(end) if end <= page.len() => {
                page.get(loc.offset..end)
                    .ok_or_else(|| MapError::OutOfPage {
                        context: context.to_string(),
                        page: loc.page,
                        offset: loc.offset,
                        len,
                        page_len: page.len(),
                    })
            }
            _ => Err(MapError::OutOfPage {
                context: context.to_string(),
                page: loc.page,
                offset: loc.offset,
                len,
                page_len: page.len(),
            }),
        }
    }

    /// The block whose bytes contain `loc`.
    pub fn block_containing(&self, loc: Location) -> Option<&Block> {
        // Binary search for the last block starting at or before loc.
        let idx = self
            .sorted_blocks
            .partition_point(|&i| self.blocks.get(i).and_then(|b| b.location) <= Some(loc));
        // Blocks may nest or overlap; scan back a few for containment.
        for &i in self.sorted_blocks.get(..idx)?.iter().rev().take(8) {
            let b = self.blocks.get(i)?;
            let start = b.location?;
            if start.page == loc.page
                && loc.offset >= start.offset
                && loc.offset < start.offset.saturating_add(b.size as usize)
            {
                return Some(b);
            }
        }
        None
    }

    /// Resolve a u64 reference: pool reference first, then plain pointer.
    ///
    /// Returns the location and, when known, the block it lies in. Returns
    /// `None` for 0 and for values that resolve neither way.
    pub fn resolve_ref(&self, value: u64) -> Option<(Location, Option<&Block>)> {
        if value == 0 {
            return None;
        }
        if let Some((index, offset)) = split_pool_ref(value)
            && let Some(block) = self.block(index)
            && let Some(start) = block.location
            && offset < (block.size as usize).max(1)
        {
            if value & POOL_INDEX_BIT12 != 0 {
                debug!("meta: pool reference 0x{value:X} uses bit 12 of the block index");
            }
            return Some((start.offset_by(offset)?, Some(block)));
        }
        let loc = decode_pointer(value)?;
        Some((loc, self.block_containing(loc)))
    }

    /// Whether resolving this value relied on the disputed bit 12 of the
    /// pool index (GTA6-Asset-Info: bits 0-12; rage9-re: bits 0-11).
    pub fn ref_uses_disputed_bit(&self, value: u64) -> bool {
        split_pool_ref(value).is_some_and(|(i, _)| self.block(i).is_some())
            && value & POOL_INDEX_BIT12 != 0
    }

    /// Every object of the given struct, in block order.
    pub fn objects_of(&self, struct_hash: Hash32) -> Vec<ObjRef> {
        let Some(def) = self.schema.struct_def(struct_hash) else {
            return Vec::new();
        };
        let stride = def.size as usize;
        if stride == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for b in self.blocks.iter().filter(|b| b.type_hash == struct_hash) {
            let Some(start) = b.location else { continue };
            let n = (b.size as usize / stride).min(MAX_BLOCK_OBJECTS);
            for i in 0..n {
                if let Some(location) = start.offset_by(i * stride) {
                    out.push(ObjRef {
                        location,
                        struct_hash,
                    });
                }
            }
        }
        out
    }

    /// The root object, if the header's +0x20 field reads as a valid 1-based
    /// block index (rage9-re). Provisional: GTA6-Asset-Info reads +0x20 as a
    /// root count and identifies the root as the struct nobody references.
    pub fn root(&self) -> Option<ObjRef> {
        let index = u16::try_from(self.header.root_field).ok()?;
        let block = self.block(index)?;
        self.schema.struct_def(block.type_hash)?;
        Some(ObjRef {
            location: block.location?,
            struct_hash: block.type_hash,
        })
    }

    /// The bytes of an object (struct size from the schema, or `fallback_size`).
    pub fn object_bytes(&self, obj: &ObjRef, fallback_size: usize) -> Result<&[u8]> {
        let size = self
            .schema
            .struct_def(obj.struct_hash)
            .map(|d| d.size as usize)
            .unwrap_or(fallback_size);
        self.bytes_at(
            obj.location,
            size,
            &format!("object of struct {} at {}", obj.struct_hash, obj.location),
        )
    }

    /// Read a member of an object as a scalar or vector [`Value`].
    pub fn read_value(&self, obj: &ObjRef, member: &MemberDef) -> Result<Value> {
        let ty = member.data_type();
        let ctx = || {
            format!(
                "member 0x{:08X} (+0x{:X}) of {} at {}",
                member.name.0, member.offset, obj.struct_hash, obj.location
            )
        };
        let len = match ty {
            DataType::Bitset => 4,
            DataType::Unknown(_) | DataType::Struct | DataType::String | DataType::Map => {
                return Err(MapError::UnsupportedType {
                    context: ctx(),
                    member: member.name.0,
                    type_code: member.type_code,
                });
            }
            t => t.fixed_size().unwrap_or(4),
        };
        let at = obj
            .location
            .offset_by(member.offset as usize)
            .ok_or_else(|| MapError::BadReference {
                context: ctx(),
                value: member.offset as u64,
            })?;
        let bytes = self.bytes_at(at, len, &ctx())?;
        read_typed(bytes, ty).ok_or_else(|| MapError::UnsupportedType {
            context: ctx(),
            member: member.name.0,
            type_code: member.type_code,
        })
    }

    /// Read a value of a given type at a byte offset inside an object, with
    /// no schema involved. Used for documented fallback offsets.
    pub fn read_raw(&self, obj: &ObjRef, offset: usize, ty: DataType) -> Result<Value> {
        let len = ty.fixed_size().unwrap_or(4);
        let ctx = || {
            format!(
                "raw +0x{offset:X} {ty:?} of {} at {}",
                obj.struct_hash, obj.location
            )
        };
        let at = obj
            .location
            .offset_by(offset)
            .ok_or_else(|| MapError::BadReference {
                context: ctx(),
                value: offset as u64,
            })?;
        let bytes = self.bytes_at(at, len, &ctx())?;
        read_typed(bytes, ty).ok_or(MapError::UnsupportedType {
            context: ctx(),
            member: 0,
            type_code: 0,
        })
    }

    /// Resolve an array member of an object.
    ///
    /// `element` overrides the schema's element member (used when the schema
    /// lacks the array). Bounds of the element storage are checked against the
    /// page before any allocation.
    pub fn read_array(
        &self,
        obj: &ObjRef,
        array_offset: usize,
        array_type: DataType,
        element: Option<MemberDef>,
        fallback_stride: usize,
    ) -> Result<ArrayData> {
        let ctx = || {
            format!(
                "array +0x{array_offset:X} of {} at {}",
                obj.struct_hash, obj.location
            )
        };
        let at = obj
            .location
            .offset_by(array_offset)
            .ok_or_else(|| MapError::BadReference {
                context: ctx(),
                value: array_offset as u64,
            })?;
        let head = self.bytes_at(at, 16, &ctx())?;
        let r = Reader::new(head);
        let (raw_ref, count) = match array_type {
            DataType::ArrayCountFirst => (rd(r.u64_at(8), ctx)?, rd(r.u32_at(0), ctx)? as usize),
            _ => (rd(r.u64_at(0), ctx)?, usize::from(rd(r.u16_at(8), ctx)?)),
        };
        let stride = element
            .as_ref()
            .and_then(|e| self.member_size(e))
            .unwrap_or(fallback_stride);
        if count == 0 {
            return Ok(ArrayData {
                location: None,
                count: 0,
                stride,
                element,
                raw_ref,
            });
        }
        if count > MAX_ARRAY_ELEMENTS {
            return Err(MapError::TooLarge {
                what: "array element count",
                count,
                limit: MAX_ARRAY_ELEMENTS,
            });
        }
        let (loc, _) = self
            .resolve_ref(raw_ref)
            .ok_or_else(|| MapError::BadReference {
                context: format!("{} ({count} elements)", ctx()),
                value: raw_ref,
            })?;
        let total = count.saturating_mul(stride.max(1));
        self.bytes_at(loc, total, &format!("{} elements", ctx()))?;
        Ok(ArrayData {
            location: Some(loc),
            count,
            stride,
            element,
            raw_ref,
        })
    }

    /// Byte size of a member, using the schema for embedded structs.
    pub fn member_size(&self, m: &MemberDef) -> Option<usize> {
        match m.data_type() {
            DataType::Struct => self
                .schema
                .struct_def(Hash32(m.extra))
                .map(|d| d.size as usize)
                .filter(|&s| s > 0),
            t => t.fixed_size(),
        }
    }

    /// Expand an array into object references.
    ///
    /// Pointer elements are resolved one by one (polymorphic pointers take the
    /// struct of the block they point into); embedded-struct elements are laid
    /// out at `stride`. Elements that do not resolve are skipped with a
    /// warning and reported in the second return value.
    pub fn array_objects(
        &self,
        array: &ArrayData,
        default_struct: Option<Hash32>,
    ) -> Result<(Vec<ObjRef>, usize)> {
        let Some(start) = array.location else {
            return Ok((Vec::new(), 0));
        };
        let elem_type = array.element.map(|e| e.data_type());
        let mut out = Vec::with_capacity(array.count.min(MAX_ARRAY_ELEMENTS));
        let mut skipped = 0usize;
        match elem_type {
            Some(DataType::Pointer) | None if array.stride == 8 => {
                let extra = array.element.map(|e| e.extra).unwrap_or(0);
                let bytes = self.bytes_at(start, array.count * 8, "pointer array")?;
                let r = Reader::new(bytes);
                for i in 0..array.count {
                    let v = rd(r.u64_at(i * 8), || {
                        format!("pointer element {i} at {start}")
                    })?;
                    match self.resolve_ref(v) {
                        Some((location, block)) => {
                            let struct_hash = if extra != 0 {
                                Hash32(extra)
                            } else if let Some(b) = block {
                                b.type_hash
                            } else if let Some(d) = default_struct {
                                d
                            } else {
                                skipped += 1;
                                continue;
                            };
                            out.push(ObjRef {
                                location,
                                struct_hash,
                            });
                        }
                        None => {
                            warn!(
                                "meta: array element {i} at {start}: reference 0x{v:X} does not resolve"
                            );
                            skipped += 1;
                        }
                    }
                }
            }
            _ => {
                let struct_hash = array
                    .element
                    .filter(|e| e.data_type() == DataType::Struct)
                    .map(|e| Hash32(e.extra))
                    .or(default_struct);
                let Some(struct_hash) = struct_hash else {
                    return Err(MapError::UnsupportedType {
                        context: format!("array at {start}: elements are not structs"),
                        member: 0,
                        type_code: array.element.map(|e| e.type_code).unwrap_or(0),
                    });
                };
                for i in 0..array.count {
                    if let Some(location) = start.offset_by(i * array.stride) {
                        out.push(ObjRef {
                            location,
                            struct_hash,
                        });
                    }
                }
            }
        }
        Ok((out, skipped))
    }
}

/// Decode `bytes` (exactly the type's size) as a [`Value`].
fn read_typed(bytes: &[u8], ty: DataType) -> Option<Value> {
    let r = Reader::new(bytes);
    let f = |o: usize| r.f32_at(o).ok();
    Some(match ty {
        DataType::Bool | DataType::U8 | DataType::Enum8 => Value::UInt(u64::from(r.u8_at(0).ok()?)),
        DataType::S8 => Value::Int(i64::from(r.u8_at(0).ok()? as i8)),
        DataType::S16 => Value::Int(i64::from(r.i16_at(0).ok()?)),
        DataType::U16 => Value::UInt(u64::from(r.u16_at(0).ok()?)),
        DataType::S32 => Value::Int(i64::from(r.i32_at(0).ok()?)),
        DataType::U32 | DataType::Bitset => Value::UInt(u64::from(r.u32_at(0).ok()?)),
        DataType::U64 | DataType::Pointer | DataType::U128 => Value::UInt(r.u64_at(0).ok()?),
        DataType::Hash => Value::Hash(Hash32(r.u32_at(0).ok()?)),
        DataType::F16 => Value::Float(f16_to_f32(r.u16_at(0).ok()?)),
        DataType::F32 => Value::Float(f(0)?),
        DataType::Vec3 | DataType::Vec3Padded => Value::Vec3([f(0)?, f(4)?, f(8)?]),
        DataType::Vec4 => Value::Vec4([f(0)?, f(4)?, f(8)?, f(12)?]),
        _ => return None,
    })
}

/// Infer where the graphics page starts in a concatenated buffer.
///
/// GTA6-Asset-Info §2.1: the graphics page follows the system page; it is
/// either exactly the graphics block extent or that rounded up to 64 KiB, and
/// the system page is made of whole zlib streams (64 KiB when full). The
/// candidates are scored by how many f32 members of graphics-page objects
/// read as finite, plausible numbers; ties keep the order below.
fn infer_graphics_base(
    probe: &MetaResource,
    len: usize,
    sys_extent: usize,
    gfx_extent: usize,
) -> Option<usize> {
    const CHUNK: usize = 0x1_0000;
    let mut candidates = Vec::new();
    let mut push = |c: Option<usize>| {
        if let Some(c) = c
            && c >= sys_extent
            && c.checked_add(gfx_extent).is_some_and(|e| e <= len)
            && !candidates.contains(&c)
        {
            candidates.push(c);
        }
    };
    push(len.checked_sub(gfx_extent));
    push(align_up(gfx_extent, CHUNK).and_then(|g| len.checked_sub(g)));
    push(align_up(sys_extent, 16));
    push(align_up(sys_extent, CHUNK));
    match candidates.len() {
        0 => None,
        1 => candidates.first().copied(),
        _ => {
            let mut best: Option<(usize, usize)> = None;
            for &c in &candidates {
                let score = plausibility(probe, c, len);
                debug!("meta: graphics base candidate 0x{c:X} scores {score}");
                if best.is_none_or(|(_, s)| score > s) {
                    best = Some((c, score));
                }
            }
            best.map(|(c, _)| c)
        }
    }
}

/// Count plausible f32 fields in graphics blocks if the page started at `base`.
fn plausibility(probe: &MetaResource, base: usize, len: usize) -> usize {
    let buf = probe.page(Page::System);
    let mut score = 0usize;
    let mut budget = 200_000usize;
    for b in probe.blocks() {
        let Some(loc) = b.location else { continue };
        if loc.page != Page::Graphics {
            continue;
        }
        let Some(def) = probe.schema().struct_def(b.type_hash) else {
            continue;
        };
        let stride = def.size as usize;
        if stride == 0 {
            continue;
        }
        let n = (b.size as usize / stride).min(256);
        for i in 0..n {
            for m in &def.members {
                let floats = match m.data_type() {
                    DataType::F32 => 1,
                    DataType::Vec3 | DataType::Vec3Padded => 3,
                    DataType::Vec4 => 4,
                    _ => 0,
                };
                for k in 0..floats {
                    if budget == 0 {
                        return score;
                    }
                    budget -= 1;
                    let off = base
                        .checked_add(loc.offset)
                        .and_then(|o| o.checked_add(i * stride))
                        .and_then(|o| o.checked_add(m.offset as usize + k * 4));
                    let Some(off) = off.filter(|&o| o.saturating_add(4) <= len) else {
                        continue;
                    };
                    if let Ok(v) = Reader::new(buf).f32_at(off)
                        && v.is_finite()
                        && v.abs() < 1.0e6
                        && (v == 0.0 || v.abs() > 1.0e-6)
                    {
                        score += 1;
                    }
                }
            }
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_refs_and_pointers() {
        assert_eq!(split_pool_ref(make_pool_ref(3, 0x500)), Some((3, 0x500)));
        assert_eq!(make_pool_ref(3, 0x500), 0x5000_0003);
        assert_eq!(split_pool_ref(0), None);
        assert_eq!(split_pool_ref(0x0010_0000), None); // index 0
        assert_eq!(split_pool_ref(0x0000_2001), None); // reserved bit 13
        assert_eq!(
            decode_pointer(0x5000_1230),
            Some(Location {
                page: Page::System,
                offset: 0x1230
            })
        );
        assert_eq!(
            decode_pointer(0x6000_0010),
            Some(Location {
                page: Page::Graphics,
                offset: 0x10
            })
        );
        assert_eq!(decode_pointer(0x7000_0000), None);
        assert_eq!(decode_pointer(0x1_5000_0000), None);
    }

    #[test]
    fn halves() {
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x7C00), f32::INFINITY);
        assert!(f16_to_f32(0x7E00).is_nan());
        assert!((f16_to_f32(0x0001) - 5.960_464_5e-8).abs() < 1e-12);
    }

    #[test]
    fn type_codes() {
        assert_eq!(DataType::from_code(0x4f, 5), DataType::U64);
        assert_eq!(DataType::from_code(0x4f, 0x0f), DataType::U128);
        assert_eq!(DataType::from_code(0x33, 0).fixed_size(), Some(16));
        assert_eq!(DataType::from_code(0xEE, 0), DataType::Unknown(0xEE));
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(matches!(
            MetaResource::parse(b"short"),
            Err(MapError::BadMagic { .. })
        ));
        let mut b = vec![0u8; 0x50];
        b[0x10..0x14].copy_from_slice(b"0DRP");
        b[0x14] = 0x10;
        assert!(matches!(
            MetaResource::parse(&b),
            Err(MapError::BadResourceType { found: 0x10 })
        ));
        b[0x14] = 0x7a;
        let m = MetaResource::parse(&b).unwrap();
        assert!(m.blocks().is_empty());
        assert!(m.schema().structs.is_empty());
    }
}
