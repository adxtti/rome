//! `PsoDocument`: section walk, schema, block table and footer.

use u_rome_core::{Endian, Hash32, Reader};

use crate::error::{PsoError, Result};
use crate::schema::{self, Schema};
use crate::section::{self, Section};

/// Maximum warnings kept per document; further ones are only logged.
const MAX_WARNINGS: usize = 256;

/// Block indices in pool references are 12 bits wide (bits 0..11, as given by
/// rage9-re 2.4). GTA6-Asset-Info 08-meta also mentions bits 0..12; no file
/// with more than 4,095 blocks is known to settle it.
pub const MAX_BLOCK_INDEX: usize = 0xFFF;

/// Upper bound on blocks read from the table (the count is a u16).
const MAX_BLOCKS: usize = 0xFFFF;

const POISON_AA: u64 = 0xAAAA_AAAA_AAAA_AAAA;

pub(crate) fn push_warning(warnings: &mut Vec<String>, msg: String) {
    log::warn!("pso: {msg}");
    if warnings.len() < MAX_WARNINGS {
        warnings.push(msg);
    }
}

fn align16(x: usize) -> Option<usize> {
    x.checked_add(15).map(|v| v & !15)
}

/// Which section family the document uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// `PSIN -> LSCH -> LENM -> LSNS -> LGID -> LRSC -> CHKS` (script.rpf and
    /// most data PSOs). Fully decoded.
    Lsch,
    /// `PSIN -> PMAP -> PSCH -> PSIG -> STDN|STFN -> STNS -> CHKS`
    /// (localisation packs, GTA V-style). Sections are kept opaque.
    Pmap,
    /// Neither `LSCH` nor `PMAP` present. Sections are kept opaque.
    Unknown,
}

/// One entry of the `PSIN` block table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// 1-based index, as used by pool references.
    pub index: u32,
    /// Struct name hash, or a primitive type code (e.g. `0x16`) for a pool.
    pub type_hash: Hash32,
    /// Absolute offset of the block data in the document (16-byte aligned).
    pub offset: usize,
    /// Byte size from the table.
    pub size: u32,
}

impl Block {
    pub fn end(&self) -> usize {
        self.offset + self.size as usize
    }

    /// True when `type_hash` is a primitive type code rather than a struct
    /// hash (GTA6-Asset-Info 08-meta: "or a type code, making it a pool").
    pub fn is_pool(&self) -> bool {
        self.type_hash.0 < 0x100
    }
}

/// How the block count was determined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockCountSource {
    /// `LRSC +0x3C`, verified against the section length.
    Lrsc,
    /// Run of descriptors whose `+0x08` word is `0xAAAAAAAAAAAAAAAA`.
    PoisonRun,
    /// Exact fit of the block layout to the section length (rage-tools-vi).
    Solved,
}

/// The 80-byte `LRSC` body (a copy of the `0DRP` type 0x7a resource header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceHeader {
    /// `+0x04`, 0x7a in all known files.
    pub version: u32,
    /// `+0x10`, 1-based root block index (rage9-re 2.1).
    pub root_block_index: u32,
    /// `+0x38`.
    pub struct_count: u16,
    /// `+0x3A`. rage9-re: enum count. GTA6-Asset-Info 17-other: root block.
    /// Provisional; cross-checked against `LENM` during parsing.
    pub enum_count: u16,
    /// `+0x3C`.
    pub block_count: u16,
}

/// Where the root block index came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSource {
    /// `LRSC +0x10` (rage9-re). Not provisional.
    LrscRootField,
    /// `LRSC +0x3A` (GTA6-Asset-Info 17-other). Provisional.
    LrscAltField,
    /// The first struct block whose type no member references
    /// (GTA6-Asset-Info 08-meta, later note). Provisional.
    Unreferenced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootInfo {
    pub block_index: u32,
    pub source: RootSource,
    /// True unless the index came from `LRSC +0x10`.
    pub provisional: bool,
}

/// `LSNS` header. The body is encrypted or obfuscated and kept opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lsns {
    /// `+0x00`, table count (1 in script.rpf).
    pub count: u32,
    /// `+0x04`, 0x101 in all known files.
    pub flags: u32,
    /// Byte length of each table.
    pub table_lengths: Vec<u32>,
    /// Identifier between table lengths (only when `count > 1`).
    pub table_ids: Vec<u32>,
    /// True when header size plus table lengths equals the body length.
    pub framing_ok: bool,
    /// Multi-table framing comes from a single source (GTA6-Asset-Info
    /// structure/04); always true when `count > 1`.
    pub provisional: bool,
}

/// `CHKS` footer: 12 big-endian bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checks {
    pub total_size: u32,
    /// Checksum value. The algorithm is unknown (rage-tools-vi excludes
    /// joaat, fnv1a, crc32 and adler32), so it is not verified.
    pub checksum: u32,
    /// `"zppp"` in all known files.
    pub tag: [u8; 4],
    /// `total_size` equals the end of the last section.
    pub size_matches: bool,
}

/// A parsed PSIN document. Owns a copy of the input bytes.
#[derive(Clone, Debug)]
pub struct PsoDocument {
    data: Vec<u8>,
    sections: Vec<Section>,
    layout: Layout,
    schema: Schema,
    blocks: Vec<Block>,
    block_count_source: Option<BlockCountSource>,
    resource: Option<ResourceHeader>,
    root: Option<RootInfo>,
    lsns: Option<Lsns>,
    checks: Option<Checks>,
    warnings: Vec<String>,
}

impl PsoDocument {
    /// Parse a decompressed PSIN document.
    ///
    /// Fails only when the section framing itself is broken or a required
    /// table cannot be read; recoverable oddities are reported through
    /// [`PsoDocument::warnings`] and `log::warn!`.
    pub fn parse(bytes: &[u8]) -> Result<PsoDocument> {
        if bytes.get(..4) != Some(b"PSIN".as_slice()) {
            return Err(PsoError::NotPsin {
                found: bytes.get(..4).unwrap_or(bytes).to_vec(),
            });
        }
        let mut warnings = Vec::new();
        let sections = section::walk(bytes, &mut warnings)?;
        let find = |tag: &[u8; 4]| sections.iter().find(|s| s.is(tag)).copied();

        let layout = if find(b"LSCH").is_some() {
            Layout::Lsch
        } else if find(b"PMAP").is_some() || find(b"PSCH").is_some() {
            Layout::Pmap
        } else {
            Layout::Unknown
        };
        log::debug!(
            "pso: {} sections, layout {layout:?}, {} bytes",
            sections.len(),
            bytes.len()
        );

        let r = Reader::new(bytes);
        let resource = match find(b"LRSC") {
            Some(s) => Some(parse_lrsc(&r, s, &mut warnings)?),
            None => None,
        };

        let mut schema = Schema::default();
        let mut blocks = Vec::new();
        let mut block_count_source = None;
        if layout == Layout::Lsch {
            let lsch = find(b"LSCH").ok_or(PsoError::Malformed {
                section: "LSCH",
                offset: 0,
                message: "missing".into(),
            })?;
            let structs = schema::parse_lsch(
                &r,
                lsch.body_offset(),
                lsch.body_len(),
                resource.map(|h| usize::from(h.struct_count)),
                &mut warnings,
            )?;
            let enums = match find(b"LENM") {
                Some(s) => schema::parse_lenm(
                    &r,
                    s.body_offset(),
                    s.body_len(),
                    resource.map(|h| usize::from(h.enum_count)),
                    &mut warnings,
                )?,
                None => {
                    push_warning(&mut warnings, "no LENM section; no enums".into());
                    Vec::new()
                }
            };
            schema = Schema::new(structs, enums);
            if let Some(psin) = find(b"PSIN")
                && let Some((b, src)) = parse_blocks(
                    &r,
                    psin,
                    resource.map(|h| usize::from(h.block_count)),
                    &mut warnings,
                )?
            {
                blocks = b;
                block_count_source = Some(src);
            }
        } else {
            push_warning(
                &mut warnings,
                format!(
                    "layout {layout:?} is not decoded; all sections are kept opaque \
                     (no instance data or schema)"
                ),
            );
        }

        let lsns = find(b"LSNS").map(|s| parse_lsns(&r, s, &mut warnings));
        let logical_end = sections.last().map_or(0, Section::end);
        let checks = match find(b"CHKS") {
            Some(s) => Some(parse_chks(bytes, s, logical_end, &mut warnings)?),
            None => {
                push_warning(&mut warnings, "no CHKS section".into());
                None
            }
        };

        let mut doc = PsoDocument {
            data: bytes.to_vec(),
            sections,
            layout,
            schema,
            blocks,
            block_count_source,
            resource,
            root: None,
            lsns,
            checks,
            warnings,
        };
        doc.root = doc.find_root();
        Ok(doc)
    }

    /// All sections in file order, including unknown ones.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// First section with this tag.
    pub fn section(&self, tag: &[u8; 4]) -> Option<&Section> {
        self.sections.iter().find(|s| s.is(tag))
    }

    /// Raw body bytes of a section (opaque for unknown sections).
    pub fn section_body(&self, section: &Section) -> &[u8] {
        self.data
            .get(section.body_offset()..section.end())
            .unwrap_or(&[])
    }

    /// The whole document as given to `parse`.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// `PSIN` block table entries in order (index 1 first).
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Block by 1-based index.
    pub fn block(&self, index: u32) -> Option<&Block> {
        let i = usize::try_from(index).ok()?.checked_sub(1)?;
        self.blocks.get(i)
    }

    /// Data of a block by 1-based index.
    pub fn block_data(&self, index: u32) -> Option<&[u8]> {
        let b = self.block(index)?;
        self.data.get(b.offset..b.end())
    }

    pub fn block_count_source(&self) -> Option<BlockCountSource> {
        self.block_count_source
    }

    pub fn resource_header(&self) -> Option<&ResourceHeader> {
        self.resource.as_ref()
    }

    pub fn root_info(&self) -> Option<RootInfo> {
        self.root
    }

    pub fn lsns(&self) -> Option<&Lsns> {
        self.lsns.as_ref()
    }

    pub fn checks(&self) -> Option<&Checks> {
        self.checks.as_ref()
    }

    /// Non-fatal problems found while parsing (also logged with `log::warn!`).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn is_struct_block(&self, index: u32) -> bool {
        self.block(index)
            .is_some_and(|b| self.schema.find_struct(b.type_hash).is_some())
    }

    fn find_root(&mut self) -> Option<RootInfo> {
        if self.layout != Layout::Lsch || self.blocks.is_empty() {
            return None;
        }
        let hdr = self.resource?;
        if self.is_struct_block(hdr.root_block_index) {
            return Some(RootInfo {
                block_index: hdr.root_block_index,
                source: RootSource::LrscRootField,
                provisional: false,
            });
        }
        push_warning(
            &mut self.warnings,
            format!(
                "LRSC +0x10 root block index {} is not a struct block (of {}); trying fallbacks",
                hdr.root_block_index,
                self.blocks.len()
            ),
        );
        let alt = u32::from(hdr.enum_count);
        if self.is_struct_block(alt) {
            return Some(RootInfo {
                block_index: alt,
                source: RootSource::LrscAltField,
                provisional: true,
            });
        }
        let referenced: std::collections::HashSet<u32> = self
            .schema
            .structs
            .iter()
            .flat_map(|s| s.members.iter())
            .filter(|m| {
                matches!(
                    m.type_code,
                    schema::type_code::STRUCT | schema::type_code::POINTER
                )
            })
            .map(|m| m.extra)
            .collect();
        self.blocks
            .iter()
            .find(|b| {
                !referenced.contains(&b.type_hash.0)
                    && self.schema.find_struct(b.type_hash).is_some()
            })
            .map(|b| RootInfo {
                block_index: b.index,
                source: RootSource::Unreferenced,
                provisional: true,
            })
    }
}

fn parse_lrsc(r: &Reader, s: Section, warnings: &mut Vec<String>) -> Result<ResourceHeader> {
    let b = s.body_offset();
    if s.body_len() < 0x40 {
        return Err(PsoError::Malformed {
            section: "LRSC",
            offset: s.offset,
            message: format!("body is {} bytes, need at least 64", s.body_len()),
        });
    }
    if s.body_len() != 80 {
        push_warning(
            warnings,
            format!("LRSC body is {} bytes, expected 80", s.body_len()),
        );
    }
    let magic = r.bytes_at(b, 4)?;
    if magic != b"0DRP" {
        push_warning(
            warnings,
            format!(
                "LRSC at {:#x}: magic {magic:02X?}, expected \"0DRP\"",
                s.offset
            ),
        );
    }
    let hdr = ResourceHeader {
        version: r.u32_at(b + 4)?,
        root_block_index: r.u32_at(b + 0x10)?,
        struct_count: r.u16_at(b + 0x38)?,
        enum_count: r.u16_at(b + 0x3a)?,
        block_count: r.u16_at(b + 0x3c)?,
    };
    if hdr.version != 0x7a {
        push_warning(
            warnings,
            format!("LRSC version {:#x}, expected 0x7a", hdr.version),
        );
    }
    log::debug!("pso: LRSC {hdr:?}");
    Ok(hdr)
}

/// Lay out `count` blocks; returns `None` when they do not fit the section.
fn layout_blocks(r: &Reader, psin: Section, count: usize) -> Option<Vec<Block>> {
    let body = psin.body_offset();
    let end = psin.end();
    let table_end = body.checked_add(count.checked_mul(16)?)?;
    if table_end > end {
        return None;
    }
    // Blocks start on 16-byte file boundaries (rage9-re 2.4; rage-tools-vi
    // gives the same position as `16 * count + 0x10`).
    let mut cur = align16(table_end)?;
    let mut blocks = Vec::with_capacity(count);
    let mut last_end = cur;
    for i in 0..count {
        let h = body + i * 16;
        let type_hash = r.u32_at(h).ok()?;
        let size = r.u32_at(h + 4).ok()?;
        let block_end = cur.checked_add(size as usize)?;
        if block_end > end {
            return None;
        }
        blocks.push(Block {
            index: u32::try_from(i + 1).ok()?,
            type_hash: Hash32(type_hash),
            offset: cur,
            size,
        });
        last_end = block_end;
        cur = align16(block_end)?;
    }
    // The last block may or may not be padded to 16 bytes.
    if end >= last_end && end <= cur.max(last_end) {
        Some(blocks)
    } else {
        None
    }
}

fn parse_blocks(
    r: &Reader,
    psin: Section,
    hint: Option<usize>,
    warnings: &mut Vec<String>,
) -> Result<Option<(Vec<Block>, BlockCountSource)>> {
    let max = (psin.body_len() / 16).min(MAX_BLOCKS);
    if let Some(n) = hint {
        if let Some(b) = layout_blocks(r, psin, n) {
            return Ok(Some((b, BlockCountSource::Lrsc)));
        }
        push_warning(
            warnings,
            format!("LRSC block count {n} does not fit the PSIN section; trying fallbacks"),
        );
    }
    let body = psin.body_offset();
    let run = (0..max)
        .take_while(|i| r.u64_at(body + i * 16 + 8).ok() == Some(POISON_AA))
        .count();
    if run > 0
        && let Some(b) = layout_blocks(r, psin, run)
    {
        return Ok(Some((b, BlockCountSource::PoisonRun)));
    }
    for n in 1..=max.min(MAX_BLOCK_INDEX) {
        if let Some(b) = layout_blocks(r, psin, n) {
            push_warning(
                warnings,
                format!("PSIN block count {n} solved from the section length"),
            );
            return Ok(Some((b, BlockCountSource::Solved)));
        }
    }
    push_warning(
        warnings,
        format!(
            "PSIN at {:#x}: no block count fits the {}-byte body; no instance data",
            psin.offset,
            psin.body_len()
        ),
    );
    Ok(None)
}

fn parse_lsns(r: &Reader, s: Section, warnings: &mut Vec<String>) -> Lsns {
    let b = s.body_offset();
    let len = s.body_len();
    let count = r.u32_at(b).unwrap_or(0);
    let flags = r.u32_at(b + 4).unwrap_or(0);
    let mut lsns = Lsns {
        count,
        flags,
        table_lengths: Vec::new(),
        table_ids: Vec::new(),
        framing_ok: false,
        provisional: count > 1,
    };
    // GTA6-Asset-Info structure/04: after count and flags, a u32 length per
    // table with a u32 identifier after every length except the last. For
    // count 1 this is rage9-re's {1, 0x101, tail_len}.
    if count == 0 || count > 64 {
        push_warning(
            warnings,
            format!("LSNS at {:#x}: unexpected table count {count}", s.offset),
        );
        return lsns;
    }
    let mut pos = b + 8;
    for i in 0..count {
        let Ok(l) = r.u32_at(pos) else { break };
        lsns.table_lengths.push(l);
        pos += 4;
        if i + 1 < count {
            let Ok(id) = r.u32_at(pos) else { break };
            lsns.table_ids.push(id);
            pos += 4;
        }
    }
    let header = pos - b;
    let total: u64 = lsns.table_lengths.iter().map(|&l| u64::from(l)).sum();
    lsns.framing_ok =
        lsns.table_lengths.len() == count as usize && header as u64 + total == len as u64;
    if !lsns.framing_ok {
        push_warning(
            warnings,
            format!(
                "LSNS at {:#x}: header ({header} B) plus tables ({total} B) != body ({len} B)",
                s.offset
            ),
        );
    }
    lsns
}

fn parse_chks(
    data: &[u8],
    s: Section,
    logical_end: usize,
    warnings: &mut Vec<String>,
) -> Result<Checks> {
    let r = Reader::with_endian(data, Endian::Big);
    let b = s.body_offset();
    if s.body_len() != 12 {
        return Err(PsoError::Malformed {
            section: "CHKS",
            offset: s.offset,
            message: format!("body is {} bytes, expected 12", s.body_len()),
        });
    }
    let total_size = r.u32_at(b)?;
    let checksum = r.u32_at(b + 4)?;
    let mut tag = [0u8; 4];
    tag.copy_from_slice(r.bytes_at(b + 8, 4)?);
    let size_matches = total_size as usize == logical_end;
    if !size_matches {
        push_warning(
            warnings,
            format!("CHKS total size {total_size:#x} != document size {logical_end:#x}"),
        );
    }
    if &tag != b"zppp" {
        push_warning(warnings, format!("CHKS tag {tag:02X?}, expected \"zppp\""));
    }
    Ok(Checks {
        total_size,
        checksum,
        tag,
        size_matches,
    })
}
