//! Struct and enum schemas (`LSCH`, `LENM`) and member type codes.
//!
//! Layouts follow rage9-re `docs/SCRIPT_FORMAT.md` sections 2.2 and 2.3
//! (MIT OR Apache-2.0); type codes beyond the ones it lists come from the
//! GTA6-Asset-Info notes `formats/08-meta.md` and `formats/17-other.md`
//! (facts only). Codes whose layout is inferred by a single source, or where
//! sources disagree, report `provisional: true` from [`type_info`].

use std::collections::HashMap;

use u_rome_core::{Hash32, Reader};

use crate::error::{PsoError, Result};

/// Member type codes (`LSCH` member descriptor byte `+0x08`).
pub mod type_code {
    pub const BOOL: u8 = 0x01;
    pub const STRUCT: u8 = 0x05;
    pub const POINTER: u8 = 0x07;
    pub const U64_ALT: u8 = 0x08;
    pub const S8: u8 = 0x10;
    pub const U8: u8 = 0x11;
    pub const S16: u8 = 0x12;
    pub const U16: u8 = 0x13;
    pub const S32: u8 = 0x14;
    pub const U32: u8 = 0x15;
    pub const U64: u8 = 0x16;
    pub const F16: u8 = 0x20;
    pub const F32: u8 = 0x21;
    pub const VEC4_ALT: u8 = 0x31;
    pub const VEC3: u8 = 0x32;
    pub const VEC3_PADDED: u8 = 0x33;
    pub const VEC4: u8 = 0x34;
    pub const STRING: u8 = 0x44;
    pub const HASH: u8 = 0x4a;
    pub const HASH64: u8 = 0x4d;
    pub const WIDE: u8 = 0x4f;
    pub const INLINE_ARRAY: u8 = 0x50;
    pub const ARRAY_REF16: u8 = 0x52;
    pub const ARRAY: u8 = 0x5c;
    pub const ENUM8: u8 = 0x60;
    pub const TAGGED: u8 = 0x62;
    pub const BITSET: u8 = 0x65;
    pub const MAP: u8 = 0x71;
    pub const HASH_MAP: u8 = 0x75;
}

/// Member name hash used by the anonymous element descriptor of an array
/// (rage9-re 2.2, GTA6-Asset-Info 08-meta). Such members are not fields.
pub const ANONYMOUS_MEMBER: Hash32 = Hash32(0x100);

/// Static facts about a member type code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeInfo {
    pub code: u8,
    /// Short human-readable name, e.g. `"u32"`, `"atArray"`.
    pub name: &'static str,
    /// Byte size when it does not depend on the schema (`None` for embedded
    /// structs, inline arrays and unknown codes).
    pub size: Option<u32>,
    /// True when the layout comes from a single inferred source or sources
    /// disagree. Decoded values of such members may be wrong.
    pub provisional: bool,
}

/// Look up what is known about a member type code.
pub fn type_info(code: u8, subtype: u8) -> TypeInfo {
    use type_code::*;
    let (name, size, provisional) = match code {
        BOOL => ("bool", Some(1), false),
        STRUCT => ("struct", None, false),
        // 08-meta: pointer, always a pool reference in PSO (17-other).
        POINTER => ("pointer", Some(8), false),
        // 08-meta lists 0x08 as u64 in meta resources; rage9-re uses 0x16.
        U64_ALT => ("u64", Some(8), false),
        S8 => ("s8", Some(1), false),
        U8 => ("u8", Some(1), false),
        S16 => ("s16", Some(2), false),
        U16 => ("u16", Some(2), false),
        // rage9-re 2.2: "s32/f32-bits". The script DOM vector struct stores
        // f32 bit patterns in 0x14 members, so the integer reading may be
        // wrong for some fields.
        S32 => ("s32", Some(4), true),
        U32 => ("u32", Some(4), false),
        U64 => ("u64", Some(8), false),
        F16 => ("f16", Some(2), false),
        F32 => ("f32", Some(4), false),
        // 17-other: "4 floats", inferred.
        VEC4_ALT => ("vec4f", Some(16), true),
        VEC3 => ("vec3", Some(12), false),
        VEC3_PADDED => ("vec3 (16 B)", Some(16), false),
        VEC4 => ("vec4", Some(16), false),
        // 17-other: "0x44 takes 16 bytes, probably a string reference into
        // LSNS (not decoded)"; 08-meta: "inline chars or u64 ref".
        STRING => ("string", Some(16), true),
        HASH => ("hash", Some(4), false),
        HASH64 => ("hash64", Some(8), false),
        // 08-meta: u64 for sub-type 5, 128-bit for sub-type 0x0f (inferred).
        WIDE => {
            if subtype == 0x0f {
                ("u128", Some(16), true)
            } else {
                ("u64", Some(8), true)
            }
        }
        INLINE_ARRAY => ("inline array", None, false),
        // 08-meta: {u64 ref, u16 count, u16 capacity} = 12 bytes; padded size
        // to 8-byte alignment is assumed.
        ARRAY_REF16 => ("array (ref first)", Some(16), true),
        ARRAY => ("atArray", Some(16), false),
        ENUM8 => ("enum8", Some(1), false),
        // 17-other: "tagged value (16 B: u32 tag, then the payload)", inferred.
        TAGGED => ("tagged value", Some(16), true),
        // 17-other: bitsets take 16 bytes in PSO; only the first u64 is used.
        BITSET => ("bitset", Some(16), true),
        MAP => ("map", Some(24), true),
        HASH_MAP => ("hash map", Some(16), true),
        _ => ("unknown", None, true),
    };
    TypeInfo {
        code,
        name,
        size,
        provisional,
    }
}

/// One member descriptor (16 bytes in `LSCH`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDef {
    /// JOAAT of the field name (`0x100` = anonymous array element).
    pub name_hash: Hash32,
    /// Byte offset inside the owning struct.
    pub offset: u32,
    /// Type code, see [`type_code`].
    pub type_code: u8,
    pub subtype: u8,
    /// For arrays: index of the element descriptor within the owning struct's
    /// members. For bitsets: bit count.
    pub reference: u16,
    /// Struct hash for `0x05`/`0x07`, enum hash for `0x60`, element count for
    /// `0x50`.
    pub extra: u32,
    /// Byte size when known from the type code and schema.
    pub size: Option<u32>,
}

impl MemberDef {
    pub fn is_anonymous(&self) -> bool {
        self.name_hash == ANONYMOUS_MEMBER
    }

    pub fn type_info(&self) -> TypeInfo {
        type_info(self.type_code, self.subtype)
    }
}

/// One struct descriptor (32 bytes in `LSCH`) with its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDef {
    /// Low 32 bits of the struct name: the value used by block type hashes
    /// and by `0x05` member `extra` fields.
    pub name_hash: Hash32,
    /// `+0x04`. rage9-re calls it a signature; GTA6-Asset-Info 17-other reads
    /// `+0x00..0x08` as one 64-bit name hash. Both readings are kept via
    /// [`StructDef::name64`].
    pub signature: u32,
    /// `+0x08`: 0x200 / 0x300 / 0x400 (alignment 4 / 8 / 16 per 17-other).
    pub flags: u32,
    /// Instance size in bytes.
    pub size: u32,
    pub members: Vec<MemberDef>,
}

impl StructDef {
    /// The 64-bit name (signature in the high half).
    pub fn name64(&self) -> u64 {
        (u64::from(self.signature) << 32) | u64::from(self.name_hash.0)
    }

    /// Named members (anonymous array element descriptors skipped).
    pub fn fields(&self) -> impl Iterator<Item = &MemberDef> {
        self.members.iter().filter(|m| !m.is_anonymous())
    }

    /// First named member with this hash.
    pub fn member(&self, name_hash: Hash32) -> Option<&MemberDef> {
        self.fields().find(|m| m.name_hash == name_hash)
    }

    /// Bytes from `members[index]` to the next member (or the struct end).
    /// Used for opaque fallbacks when a type's size is unknown.
    pub fn member_span(&self, index: usize) -> u32 {
        let Some(m) = self.members.get(index) else {
            return 0;
        };
        let next = self
            .fields()
            .map(|o| o.offset)
            .filter(|&o| o > m.offset)
            .min()
            .unwrap_or(self.size);
        next.saturating_sub(m.offset)
    }

    /// Element descriptor of an array member. Uses `reference` as an index into
    /// this struct's members (rage9-re 2.2); if that is out of range or not
    /// anonymous, falls back to the closest preceding anonymous member.
    pub fn element_of(&self, array_index: usize) -> Option<&MemberDef> {
        self.members.get(self.element_index(array_index)?)
    }

    /// Index form of [`StructDef::element_of`].
    pub fn element_index(&self, array_index: usize) -> Option<usize> {
        let array = self.members.get(array_index)?;
        let r = usize::from(array.reference);
        if r != array_index && self.members.get(r).is_some_and(MemberDef::is_anonymous) {
            return Some(r);
        }
        (0..array_index)
            .rev()
            .find(|&i| self.members.get(i).is_some_and(MemberDef::is_anonymous))
    }
}

/// One enum value entry (16 bytes in `LENM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnumValue {
    pub value: i64,
    /// Low 32 bits of the stored 64-bit name hash.
    pub name_hash: Hash32,
    /// The stored 64-bit name hash.
    pub name_hash_raw: u64,
}

/// One enum descriptor (24 bytes in `LENM`) with its values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDef {
    pub name_hash: Hash32,
    /// `+0x04`, see [`StructDef::signature`].
    pub signature: u32,
    pub values: Vec<EnumValue>,
}

impl EnumDef {
    pub fn value_name(&self, value: i64) -> Option<Hash32> {
        self.values
            .iter()
            .find(|v| v.value == value)
            .map(|v| v.name_hash)
    }
}

/// All struct and enum schemas of a document.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub structs: Vec<StructDef>,
    pub enums: Vec<EnumDef>,
    struct_index: HashMap<Hash32, usize>,
    enum_index: HashMap<Hash32, usize>,
}

impl Schema {
    pub fn new(structs: Vec<StructDef>, enums: Vec<EnumDef>) -> Self {
        let mut s = Schema {
            structs,
            enums,
            struct_index: HashMap::new(),
            enum_index: HashMap::new(),
        };
        for (i, d) in s.structs.iter().enumerate() {
            s.struct_index.entry(d.name_hash).or_insert(i);
        }
        for (i, d) in s.enums.iter().enumerate() {
            s.enum_index.entry(d.name_hash).or_insert(i);
        }
        s.fill_member_sizes();
        s
    }

    pub fn find_struct(&self, hash: Hash32) -> Option<&StructDef> {
        self.struct_index
            .get(&hash)
            .and_then(|&i| self.structs.get(i))
    }

    pub fn find_enum(&self, hash: Hash32) -> Option<&EnumDef> {
        self.enum_index.get(&hash).and_then(|&i| self.enums.get(i))
    }

    /// Name hash of `value` in enum `enum_hash`, if both are known.
    pub fn enum_value_name(&self, enum_hash: Hash32, value: i64) -> Option<Hash32> {
        self.find_enum(enum_hash)?.value_name(value)
    }

    fn fill_member_sizes(&mut self) {
        let struct_sizes: HashMap<Hash32, u32> =
            self.structs.iter().map(|s| (s.name_hash, s.size)).collect();
        let base_size = |m: &MemberDef| -> Option<u32> {
            match m.type_code {
                type_code::STRUCT => struct_sizes.get(&Hash32(m.extra)).copied(),
                _ => type_info(m.type_code, m.subtype).size,
            }
        };
        for s in &mut self.structs {
            let sizes: Vec<Option<u32>> = s
                .members
                .iter()
                .map(|m| match m.type_code {
                    type_code::INLINE_ARRAY => s
                        .members
                        .get(usize::from(m.reference))
                        .and_then(base_size)
                        .and_then(|e| e.checked_mul(m.extra)),
                    _ => base_size(m),
                })
                .collect();
            for (m, size) in s.members.iter_mut().zip(sizes) {
                m.size = size;
            }
        }
    }
}

const STRUCT_DESC: usize = 32;
const MEMBER_DESC: usize = 16;
const ENUM_DESC: usize = 24;
const ENUM_VALUE: usize = 16;

/// Find the struct count that makes `32 * n + 16 * sum(member_count)` equal
/// the body length exactly (rage-tools-vi: counts are recovered by exact fit).
fn solve_struct_count(r: &Reader, base: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    let mut members = 0u64;
    for n in 1..=len / STRUCT_DESC {
        let hdr = base + (n - 1) * STRUCT_DESC;
        members += u64::from(r.u16_at(hdr + 0x1e).ok()?);
        let total = (n * STRUCT_DESC) as u64 + members * MEMBER_DESC as u64;
        if total == len as u64 {
            return Some(n);
        }
        if total > len as u64 {
            return None;
        }
    }
    None
}

fn solve_enum_count(r: &Reader, base: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    let mut values = 0u64;
    for n in 1..=len / ENUM_DESC {
        let hdr = base + (n - 1) * ENUM_DESC;
        values += u64::from(r.u32_at(hdr + 0x10).ok()?);
        let total = (n * ENUM_DESC) as u64 + values * ENUM_VALUE as u64;
        if total == len as u64 {
            return Some(n);
        }
        if total > len as u64 {
            return None;
        }
    }
    None
}

fn malformed(section: &'static str, offset: usize, message: String) -> PsoError {
    PsoError::Malformed {
        section,
        offset,
        message,
    }
}

/// Parse `LSCH` with an exact struct count.
fn parse_lsch_with(r: &Reader, base: usize, len: usize, count: usize) -> Result<Vec<StructDef>> {
    let hdr_bytes = count
        .checked_mul(STRUCT_DESC)
        .filter(|&b| b <= len)
        .ok_or_else(|| {
            malformed(
                "LSCH",
                base,
                format!("{count} struct descriptors do not fit in {len} bytes"),
            )
        })?;
    let mut heads = Vec::with_capacity(count);
    let mut total_members = 0usize;
    for i in 0..count {
        let h = base + i * STRUCT_DESC;
        let member_count = usize::from(r.u16_at(h + 0x1e)?);
        total_members += member_count;
        heads.push((
            Hash32(r.u32_at(h)?),
            r.u32_at(h + 4)?,
            r.u32_at(h + 8)?,
            r.u32_at(h + 0x18)?,
            member_count,
        ));
    }
    let expected = hdr_bytes + total_members * MEMBER_DESC;
    if expected != len {
        return Err(malformed(
            "LSCH",
            base,
            format!(
                "{count} structs with {total_members} members need {expected} bytes, body has {len}"
            ),
        ));
    }
    let mut pos = base + hdr_bytes;
    let mut structs = Vec::with_capacity(count);
    for (name_hash, signature, flags, size, member_count) in heads {
        let mut members = Vec::with_capacity(member_count);
        for _ in 0..member_count {
            members.push(MemberDef {
                name_hash: Hash32(r.u32_at(pos)?),
                offset: r.u32_at(pos + 4)?,
                type_code: r.u8_at(pos + 8)?,
                subtype: r.u8_at(pos + 9)?,
                reference: r.u16_at(pos + 10)?,
                extra: r.u32_at(pos + 12)?,
                size: None,
            });
            pos += MEMBER_DESC;
        }
        structs.push(StructDef {
            name_hash,
            signature,
            flags,
            size,
            members,
        });
    }
    Ok(structs)
}

/// Parse `LENM` with an exact enum count.
fn parse_lenm_with(r: &Reader, base: usize, len: usize, count: usize) -> Result<Vec<EnumDef>> {
    let hdr_bytes = count
        .checked_mul(ENUM_DESC)
        .filter(|&b| b <= len)
        .ok_or_else(|| {
            malformed(
                "LENM",
                base,
                format!("{count} enum descriptors do not fit in {len} bytes"),
            )
        })?;
    let mut heads = Vec::with_capacity(count);
    let mut total_values = 0usize;
    for i in 0..count {
        let h = base + i * ENUM_DESC;
        let n = r.u32_at(h + 0x10)? as usize;
        total_values = total_values.saturating_add(n);
        heads.push((Hash32(r.u32_at(h)?), r.u32_at(h + 4)?, n));
    }
    let expected = total_values
        .checked_mul(ENUM_VALUE)
        .and_then(|v| v.checked_add(hdr_bytes));
    if expected != Some(len) {
        return Err(malformed(
            "LENM",
            base,
            format!("{count} enums with {total_values} values do not fill {len} bytes"),
        ));
    }
    let mut pos = base + hdr_bytes;
    let mut enums = Vec::with_capacity(count);
    for (name_hash, signature, n) in heads {
        let mut values = Vec::with_capacity(n);
        for _ in 0..n {
            let raw = r.u64_at(pos + 8)?;
            values.push(EnumValue {
                value: r.i64_at(pos)?,
                name_hash: Hash32(raw as u32),
                name_hash_raw: raw,
            });
            pos += ENUM_VALUE;
        }
        enums.push(EnumDef {
            name_hash,
            signature,
            values,
        });
    }
    Ok(enums)
}

/// Parse `LSCH`. `hint` is the struct count from `LRSC` when present; if it
/// does not fit, the count is solved from the body length.
pub(crate) fn parse_lsch(
    r: &Reader,
    base: usize,
    len: usize,
    hint: Option<usize>,
    warnings: &mut Vec<String>,
) -> Result<Vec<StructDef>> {
    if let Some(n) = hint {
        match parse_lsch_with(r, base, len, n) {
            Ok(s) => return Ok(s),
            Err(e) => crate::document::push_warning(
                warnings,
                format!("LRSC struct count {n} does not fit LSCH ({e}); solving from body length"),
            ),
        }
    }
    let n = solve_struct_count(r, base, len).ok_or_else(|| {
        malformed(
            "LSCH",
            base,
            format!("no struct count fits the {len}-byte body exactly"),
        )
    })?;
    parse_lsch_with(r, base, len, n)
}

/// Parse `LENM`. See [`parse_lsch`] for `hint`.
pub(crate) fn parse_lenm(
    r: &Reader,
    base: usize,
    len: usize,
    hint: Option<usize>,
    warnings: &mut Vec<String>,
) -> Result<Vec<EnumDef>> {
    if let Some(n) = hint {
        match parse_lenm_with(r, base, len, n) {
            Ok(e) => return Ok(e),
            Err(e) => crate::document::push_warning(
                warnings,
                format!(
                    "LRSC +0x3A value {n} does not fit LENM as an enum count ({e}); solving from \
                     body length. GTA6-Asset-Info 17-other reads +0x3A as the root block index"
                ),
            ),
        }
    }
    let n = solve_enum_count(r, base, len).ok_or_else(|| {
        malformed(
            "LENM",
            base,
            format!("no enum count fits the {len}-byte body exactly"),
        )
    })?;
    parse_lenm_with(r, base, len, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_table_sizes() {
        assert_eq!(type_info(type_code::ARRAY, 0).size, Some(16));
        assert_eq!(type_info(type_code::HASH, 0).size, Some(4));
        assert!(type_info(type_code::STRING, 0).provisional);
        assert!(!type_info(type_code::U32, 0).provisional);
        assert_eq!(type_info(0xee, 0).name, "unknown");
        assert_eq!(type_info(type_code::WIDE, 0x0f).size, Some(16));
    }

    #[test]
    fn solve_counts() {
        // One struct with two members, then two member descriptors.
        let mut b = vec![0u8; 32 + 32];
        b[0x1e] = 2;
        let r = Reader::new(&b);
        assert_eq!(solve_struct_count(&r, 0, b.len()), Some(1));
        assert_eq!(solve_struct_count(&r, 0, 0), Some(0));
        assert_eq!(solve_struct_count(&r, 0, 40), None);
    }
}
