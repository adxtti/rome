//! Generic value tree and the instance decoder.
//!
//! Instances are decoded by walking the `LSCH` schema: each member is read at
//! its offset according to its type code, embedded structs recurse, arrays and
//! pointers follow u64 pool references into other blocks.
//!
//! Pool reference (rage9-re 2.4, GTA6-Asset-Info 08-meta): bits 0..11 are the
//! 1-based block index, bits 20..63 the byte offset inside the block, bits
//! 12..19 are undefined. A zero reference is null.
//!
//! Safety: depth and node count are capped ([`DecodeOptions`]), every read is
//! bounds-checked against its block, and a struct that is reached again while
//! it is still being decoded (a reference cycle) is reported as
//! [`PsoError::Cycle`] instead of recursing forever.

use u_rome_core::{Hash32, Reader};

use crate::document::{Block, Layout, PsoDocument};
use crate::error::{PsoError, Result};
use crate::schema::{MemberDef, Schema, StructDef, type_code as tc};

/// A decoded PSO value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A struct instance; fields in schema order, anonymous array element
    /// descriptors omitted.
    Struct {
        type_hash: Hash32,
        fields: Vec<(Hash32, Value)>,
    },
    /// `0x5c` / `0x52` arrays and `0x50` inline arrays.
    Array(Vec<Value>),
    Bool(bool),
    /// Signed integers (`0x10`, `0x12`, `0x14`).
    Int(i64),
    /// Unsigned integers (`0x11`, `0x13`, `0x15`, `0x16`, `0x08`), and the first
    /// u64 of a bitset (`0x65`, provisional).
    UInt(u64),
    /// `0x20` (f16) and `0x21` (f32).
    Float(f32),
    /// `0x44` when it resolves to a NUL-terminated string in a pool block
    /// (provisional).
    String(String),
    /// `0x4a` 32-bit JOAAT.
    Hash(Hash32),
    /// `0x4d` 64-bit hash.
    Hash64(u64),
    /// `0x60` u8 enum; `enum_hash` is the member's `extra`.
    Enum {
        enum_hash: Hash32,
        value: i64,
    },
    /// `0x31`, `0x32`, `0x33`, `0x34`.
    Vector(Vec<f32>),
    /// Anything not decoded: unknown or provisional type codes (maps, tagged
    /// values, unresolved strings), null pointers (`type_code` 0x07, 8 zero
    /// bytes) and, in lenient mode, members whose decoding failed.
    Opaque {
        type_code: u8,
        bytes: Vec<u8>,
    },
}

impl Value {
    /// Field of a struct by name hash.
    pub fn get(&self, field: Hash32) -> Option<&Value> {
        match self {
            Value::Struct { fields, .. } => {
                fields.iter().find(|(h, _)| *h == field).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::UInt(v) | Value::Hash64(v) => Some(v),
            Value::Int(v) => u64::try_from(v).ok(),
            Value::Hash(h) => Some(u64::from(h.0)),
            Value::Bool(b) => Some(u64::from(b)),
            Value::Enum { value, .. } => u64::try_from(value).ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int(v) | Value::Enum { value: v, .. } => Some(v),
            Value::UInt(v) => i64::try_from(v).ok(),
            Value::Bool(b) => Some(i64::from(b)),
            _ => None,
        }
    }

    pub fn as_f32(&self) -> Option<f32> {
        match *self {
            Value::Float(f) => Some(f),
            _ => None,
        }
    }

    /// Total number of values in this tree, including `self`.
    pub fn node_count(&self) -> usize {
        1 + match self {
            Value::Struct { fields, .. } => fields.iter().map(|(_, v)| v.node_count()).sum(),
            Value::Array(a) => a.iter().map(Value::node_count).sum(),
            _ => 0,
        }
    }
}

/// Limits and strictness for decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Maximum nesting depth (struct, array and pointer levels).
    pub max_depth: usize,
    /// Maximum number of values produced by one decode call.
    pub max_nodes: usize,
    /// Maximum length of a string read from a pool block.
    pub max_string_len: usize,
    /// When false (the default), a member that fails to decode for a data
    /// reason (bad reference, cycle, unknown struct, out-of-block read) becomes
    /// [`Value::Opaque`] with its raw bytes and a `log::warn!`. When true, the
    /// error is returned. Limit errors are always returned.
    pub strict: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        DecodeOptions {
            max_depth: 64,
            max_nodes: 2_000_000,
            max_string_len: 64 * 1024,
            strict: false,
        }
    }
}

/// One struct instance in the block table: block `block_index` holds
/// `size / struct_size` consecutive instances of `type_hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance {
    pub block_index: u32,
    pub type_hash: Hash32,
    /// Byte offset inside the block.
    pub offset: u32,
}

/// Upper bound on instances listed by [`PsoDocument::instances`].
pub const MAX_INSTANCES: usize = 4_000_000;

/// IEEE 754 binary16 to f32.
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let frac = u32::from(h & 0x3ff);
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalise.
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, _) => sign | 0x7fc0_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

impl PsoDocument {
    /// Decode the root instance with default options.
    pub fn root(&self) -> Result<Value> {
        self.root_with(&DecodeOptions::default())
    }

    /// Decode the root instance.
    pub fn root_with(&self, opts: &DecodeOptions) -> Result<Value> {
        if self.layout() != Layout::Lsch {
            return Err(PsoError::Unsupported(format!(
                "{:?} layout has no decodable instances",
                self.layout()
            )));
        }
        let info = self.root_info().ok_or_else(|| {
            PsoError::NoRoot(format!(
                "no root block (LRSC root index {:?}, {} blocks)",
                self.resource_header().map(|h| h.root_block_index),
                self.blocks().len()
            ))
        })?;
        if info.provisional {
            log::warn!(
                "pso: decoding root block {} chosen by {:?} (provisional)",
                info.block_index,
                info.source
            );
        }
        let block = self
            .block(info.block_index)
            .ok_or_else(|| PsoError::NoRoot(format!("block {} missing", info.block_index)))?;
        self.decode_struct_at(block.type_hash, info.block_index, 0, opts)
    }

    /// All struct instances in the block table, in block order. Pool blocks
    /// (primitive type codes) and blocks whose type is not in the schema are
    /// skipped.
    pub fn instances(&self) -> Vec<Instance> {
        let mut out = Vec::new();
        for b in self.blocks() {
            let Some(def) = self.schema().find_struct(b.type_hash) else {
                if !b.is_pool() {
                    log::debug!(
                        "pso: block {} type {} not in schema; skipped",
                        b.index,
                        b.type_hash
                    );
                }
                continue;
            };
            if def.size == 0 {
                log::warn!(
                    "pso: struct {} has size 0; block {} skipped",
                    def.name_hash,
                    b.index
                );
                continue;
            }
            if b.size % def.size != 0 {
                log::warn!(
                    "pso: block {} size {:#x} is not a multiple of struct {} size {:#x}",
                    b.index,
                    b.size,
                    def.name_hash,
                    def.size
                );
            }
            for i in 0..b.size / def.size {
                if out.len() >= MAX_INSTANCES {
                    log::warn!("pso: instance list capped at {MAX_INSTANCES}");
                    return out;
                }
                out.push(Instance {
                    block_index: b.index,
                    type_hash: b.type_hash,
                    offset: i * def.size,
                });
            }
        }
        out
    }

    /// Decode one instance with default options.
    pub fn instance_value(&self, inst: &Instance) -> Result<Value> {
        self.decode_struct_at(
            inst.type_hash,
            inst.block_index,
            inst.offset,
            &DecodeOptions::default(),
        )
    }

    /// Decode a struct of type `type_hash` at `offset` inside block
    /// `block_index` (1-based).
    pub fn decode_struct_at(
        &self,
        type_hash: Hash32,
        block_index: u32,
        offset: u32,
        opts: &DecodeOptions,
    ) -> Result<Value> {
        let block = self.block(block_index).ok_or_else(|| PsoError::Malformed {
            section: "PSIN",
            offset: 0,
            message: format!(
                "block index {block_index} not in 1..={}",
                self.blocks().len()
            ),
        })?;
        let def = self
            .schema()
            .find_struct(type_hash)
            .ok_or_else(|| PsoError::UnknownStruct {
                hash: type_hash,
                offset: block.offset,
                context: format!("instance in block {block_index}"),
            })?;
        let mut d = Decoder {
            doc: self,
            schema: self.schema(),
            r: Reader::new(self.data()),
            opts: *opts,
            nodes: 0,
            stack: Vec::new(),
        };
        d.node()?;
        d.decode_struct(def, block, block.offset + offset as usize, 0)
    }
}

struct Decoder<'a> {
    doc: &'a PsoDocument,
    schema: &'a Schema,
    r: Reader<'a>,
    opts: DecodeOptions,
    nodes: usize,
    /// Structs currently being decoded: (absolute offset, type).
    stack: Vec<(usize, Hash32)>,
}

impl<'a> Decoder<'a> {
    fn node(&mut self) -> Result<()> {
        self.nodes += 1;
        if self.nodes > self.opts.max_nodes {
            return Err(PsoError::NodeLimit {
                limit: self.opts.max_nodes,
            });
        }
        Ok(())
    }

    /// Bounds-checked read that must stay inside `block`.
    fn read(&self, block: &Block, abs: usize, len: usize) -> Result<&'a [u8]> {
        let inside = abs >= block.offset && abs.checked_add(len).is_some_and(|e| e <= block.end());
        if !inside {
            return Err(PsoError::Malformed {
                section: "PSIN",
                offset: abs,
                message: format!(
                    "{len}-byte read leaves block {} ({:#x}..{:#x})",
                    block.index,
                    block.offset,
                    block.end()
                ),
            });
        }
        Ok(self.r.bytes_at(abs, len)?)
    }

    fn u8(&self, b: &Block, abs: usize) -> Result<u8> {
        self.read(b, abs, 1)?;
        Ok(self.r.u8_at(abs)?)
    }
    fn u16(&self, b: &Block, abs: usize) -> Result<u16> {
        self.read(b, abs, 2)?;
        Ok(self.r.u16_at(abs)?)
    }
    fn u32(&self, b: &Block, abs: usize) -> Result<u32> {
        self.read(b, abs, 4)?;
        Ok(self.r.u32_at(abs)?)
    }
    fn u64(&self, b: &Block, abs: usize) -> Result<u64> {
        self.read(b, abs, 8)?;
        Ok(self.r.u64_at(abs)?)
    }
    fn f32(&self, b: &Block, abs: usize) -> Result<f32> {
        self.u32(b, abs).map(f32::from_bits)
    }
    fn floats(&self, b: &Block, abs: usize, n: usize) -> Result<Value> {
        (0..n)
            .map(|i| self.f32(b, abs + i * 4))
            .collect::<Result<Vec<_>>>()
            .map(Value::Vector)
    }

    fn resolve(&self, reference: u64, at: usize) -> Result<(&'a Block, usize)> {
        let index = (reference & 0xFFF) as u32;
        if reference & 0xF_F000 != 0 {
            log::debug!("pso: reference {reference:#x} at {at:#x} has bits 12..19 set; ignored");
        }
        let block = self
            .doc
            .block(index)
            .ok_or_else(|| PsoError::BadReference {
                reference,
                offset: at,
                message: format!("block index {index} not in 1..={}", self.doc.blocks().len()),
            })?;
        let off = usize::try_from(reference >> 20)
            .ok()
            .filter(|&o| o <= block.size as usize)
            .ok_or_else(|| PsoError::BadReference {
                reference,
                offset: at,
                message: format!(
                    "offset {:#x} past the end of block {index} ({:#x} bytes)",
                    reference >> 20,
                    block.size
                ),
            })?;
        Ok((block, block.offset + off))
    }

    fn recover(
        &self,
        e: PsoError,
        code: u8,
        block: &Block,
        abs: usize,
        span: usize,
    ) -> Result<Value> {
        if self.opts.strict || e.is_limit() {
            return Err(e);
        }
        log::warn!("pso: member type {code:#04x} at {abs:#x} kept opaque: {e}");
        let bytes = self
            .read(block, abs, span)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        Ok(Value::Opaque {
            type_code: code,
            bytes,
        })
    }

    fn decode_struct(
        &mut self,
        def: &'a StructDef,
        block: &'a Block,
        abs: usize,
        depth: usize,
    ) -> Result<Value> {
        if depth > self.opts.max_depth {
            return Err(PsoError::DepthLimit {
                limit: self.opts.max_depth,
                block: block.index,
                offset: abs,
            });
        }
        if self.stack.contains(&(abs, def.name_hash)) {
            return Err(PsoError::Cycle {
                type_hash: def.name_hash,
                block: block.index,
                offset: abs,
            });
        }
        self.read(block, abs, def.size as usize)?;
        self.stack.push((abs, def.name_hash));
        let fields = self.decode_fields(def, block, abs, depth);
        self.stack.pop();
        Ok(Value::Struct {
            type_hash: def.name_hash,
            fields: fields?,
        })
    }

    fn decode_fields(
        &mut self,
        def: &'a StructDef,
        block: &'a Block,
        abs: usize,
        depth: usize,
    ) -> Result<Vec<(Hash32, Value)>> {
        let mut fields = Vec::with_capacity(def.members.len());
        for (i, m) in def.members.iter().enumerate() {
            if m.is_anonymous() {
                continue;
            }
            let at = abs + m.offset as usize;
            let v = match self.member(def, i, block, at, depth + 1) {
                Ok(v) => v,
                Err(e) => {
                    let span = m.size.unwrap_or_else(|| def.member_span(i)) as usize;
                    self.recover(e, m.type_code, block, at, span)?
                }
            };
            fields.push((m.name_hash, v));
        }
        Ok(fields)
    }

    /// Decode `owner.members[index]` located at absolute offset `abs`.
    fn member(
        &mut self,
        owner: &'a StructDef,
        index: usize,
        block: &'a Block,
        abs: usize,
        depth: usize,
    ) -> Result<Value> {
        if depth > self.opts.max_depth {
            return Err(PsoError::DepthLimit {
                limit: self.opts.max_depth,
                block: block.index,
                offset: abs,
            });
        }
        self.node()?;
        let m: &'a MemberDef = owner
            .members
            .get(index)
            .ok_or_else(|| PsoError::Malformed {
                section: "LSCH",
                offset: abs,
                message: format!("member index {index} out of range for {}", owner.name_hash),
            })?;
        let b = block;
        Ok(match m.type_code {
            tc::BOOL => Value::Bool(self.u8(b, abs)? != 0),
            tc::S8 => Value::Int(i64::from(self.u8(b, abs)? as i8)),
            tc::U8 => Value::UInt(u64::from(self.u8(b, abs)?)),
            tc::S16 => Value::Int(i64::from(self.u16(b, abs)? as i16)),
            tc::U16 => Value::UInt(u64::from(self.u16(b, abs)?)),
            tc::S32 => Value::Int(i64::from(self.u32(b, abs)? as i32)),
            tc::U32 => Value::UInt(u64::from(self.u32(b, abs)?)),
            tc::U64 | tc::U64_ALT => Value::UInt(self.u64(b, abs)?),
            tc::F16 => Value::Float(f16_to_f32(self.u16(b, abs)?)),
            tc::F32 => Value::Float(self.f32(b, abs)?),
            tc::VEC3 | tc::VEC3_PADDED => self.floats(b, abs, 3)?,
            tc::VEC4 | tc::VEC4_ALT => self.floats(b, abs, 4)?,
            tc::HASH => Value::Hash(Hash32(self.u32(b, abs)?)),
            tc::HASH64 => Value::Hash64(self.u64(b, abs)?),
            tc::WIDE if m.subtype != 0x0f => Value::UInt(self.u64(b, abs)?),
            tc::ENUM8 => Value::Enum {
                enum_hash: Hash32(m.extra),
                value: i64::from(self.u8(b, abs)?),
            },
            tc::BITSET => Value::UInt(self.u64(b, abs)?),
            tc::STRUCT => {
                let def = self.struct_def(m.extra, abs, "embedded struct member")?;
                self.decode_struct(def, b, abs, depth)?
            }
            tc::POINTER => {
                let reference = self.u64(b, abs)?;
                if reference == 0 {
                    Value::Opaque {
                        type_code: tc::POINTER,
                        bytes: vec![0; 8],
                    }
                } else {
                    let (tb, tabs) = self.resolve(reference, abs)?;
                    // 08-meta: extra 0 means polymorphic, use the target
                    // block's struct.
                    let hash = if m.extra != 0 {
                        m.extra
                    } else {
                        tb.type_hash.0
                    };
                    let def = self.struct_def(hash, tabs, "pointer target")?;
                    self.decode_struct(def, tb, tabs, depth)?
                }
            }
            tc::ARRAY => {
                let count = self.u32(b, abs)? as usize;
                let reference = self.u64(b, abs + 8)?;
                self.ref_array(owner, index, count, reference, abs, depth)?
            }
            tc::ARRAY_REF16 => {
                let reference = self.u64(b, abs)?;
                let count = usize::from(self.u16(b, abs + 8)?);
                self.ref_array(owner, index, count, reference, abs, depth)?
            }
            tc::INLINE_ARRAY => self.array(owner, index, m.extra as usize, b, abs, depth)?,
            tc::STRING => self.string(b, abs)?,
            code => {
                let span = m.size.unwrap_or_else(|| owner.member_span(index)) as usize;
                log::debug!("pso: type {code:#04x} at {abs:#x} kept opaque ({span} bytes)");
                Value::Opaque {
                    type_code: code,
                    bytes: self.read(b, abs, span)?.to_vec(),
                }
            }
        })
    }

    fn struct_def(&self, hash: u32, at: usize, context: &str) -> Result<&'a StructDef> {
        self.schema
            .find_struct(Hash32(hash))
            .ok_or_else(|| PsoError::UnknownStruct {
                hash: Hash32(hash),
                offset: at,
                context: context.to_owned(),
            })
    }

    fn ref_array(
        &mut self,
        owner: &'a StructDef,
        index: usize,
        count: usize,
        reference: u64,
        at: usize,
        depth: usize,
    ) -> Result<Value> {
        if count == 0 {
            return Ok(Value::Array(Vec::new()));
        }
        let (tb, tabs) = self.resolve(reference, at)?;
        self.array(owner, index, count, tb, tabs, depth)
    }

    /// `count` elements of the array member `owner.members[index]` starting
    /// at `abs` in `block`.
    fn array(
        &mut self,
        owner: &'a StructDef,
        index: usize,
        count: usize,
        block: &'a Block,
        abs: usize,
        depth: usize,
    ) -> Result<Value> {
        if count == 0 {
            return Ok(Value::Array(Vec::new()));
        }
        let ei = owner
            .element_index(index)
            .ok_or_else(|| PsoError::Malformed {
                section: "LSCH",
                offset: abs,
                message: format!(
                    "array member {index} of {} has no element descriptor",
                    owner.name_hash
                ),
            })?;
        let em = owner.members.get(ei).ok_or_else(|| PsoError::Malformed {
            section: "LSCH",
            offset: abs,
            message: format!("element descriptor {ei} missing"),
        })?;
        let size = em
            .size
            .filter(|&s| s > 0)
            .ok_or_else(|| PsoError::Malformed {
                section: "LSCH",
                offset: abs,
                message: format!(
                    "array element type {:#04x} of {} has no known size",
                    em.type_code, owner.name_hash
                ),
            })? as usize;
        let total = count.checked_mul(size).ok_or_else(|| PsoError::Malformed {
            section: "PSIN",
            offset: abs,
            message: format!("array of {count} x {size} bytes overflows"),
        })?;
        self.read(block, abs, total)?;
        if count > self.opts.max_nodes.saturating_sub(self.nodes) {
            return Err(PsoError::NodeLimit {
                limit: self.opts.max_nodes,
            });
        }
        let mut out = Vec::with_capacity(count.min(4096));
        for i in 0..count {
            let at = abs + i * size;
            let v = match self.member(owner, ei, block, at, depth + 1) {
                Ok(v) => v,
                Err(e) => self.recover(e, em.type_code, block, at, size)?,
            };
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    /// `0x44`: 16 bytes (GTA6-Asset-Info 17-other, "probably a string
    /// reference"). Either u64 word is tried as a pool reference to a
    /// NUL-terminated UTF-8 string; otherwise the bytes are kept opaque.
    /// Provisional.
    fn string(&self, block: &Block, abs: usize) -> Result<Value> {
        let raw = self.read(block, abs, 16)?;
        for word in raw.as_chunks::<8>().0 {
            if let Some(s) = self.pool_string(u64::from_le_bytes(*word)) {
                return Ok(Value::String(s));
            }
        }
        Ok(Value::Opaque {
            type_code: tc::STRING,
            bytes: raw.to_vec(),
        })
    }

    fn pool_string(&self, reference: u64) -> Option<String> {
        if reference == 0 || reference & 0xF_F000 != 0 {
            return None;
        }
        let (block, abs) = self.resolve(reference, 0).ok()?;
        let avail = block.end().checked_sub(abs)?.min(self.opts.max_string_len);
        let bytes = self.r.bytes_at(abs, avail).ok()?;
        let nul = bytes.iter().position(|&c| c == 0)?;
        let s = std::str::from_utf8(bytes.get(..nul)?).ok()?;
        if s.chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
        {
            return None;
        }
        Some(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(f16_to_f32(0x8000).to_bits(), 0x8000_0000);
    }
}
