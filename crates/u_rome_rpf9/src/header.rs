//! The plaintext 64-byte RPF9 header.
//!
//! Field layout follows community research:
//! - SentinelInteractive/0x52504639 `docs/rpf9-container.md` (MIT), 4,330 headers measured.
//! - lynxnathan/rage-tools-vi `format.md` (MIT).
//! - JoshRob297/rpf9-research `docs/RPF9.md` (GPL documentation, facts only), 77 headers.
//!
//! The two tables between `0x40` and `payload_start`, and the trailer, are
//! encrypted with an unknown cipher and key. This crate never tries to decrypt
//! them; the header is exposed for display and for validating nested archives.

use crate::error::Rpf9Error;
use u_rome_core::Reader;

/// `"9FPR"` on disk, `0x52504639` as a little-endian u32.
pub const MAGIC: u32 = 0x5250_4639;
/// Size of the plaintext header.
pub const HEADER_LEN: usize = 0x40;

/// Largest front region (header + encrypted tables) accepted for a nested
/// archive. The largest observed table is about 1.4 MB (0x52504639 docs); this
/// leaves generous headroom while rejecting garbage.
pub const MAX_FRONT_LEN: u32 = 256 << 20;
/// Largest entry count accepted for a nested archive. Observed maximum is a few
/// thousand per archive (rpf9-research: 3,224).
pub const MAX_ENTRY_COUNT: u32 = 16 << 20;

/// Parsed RPF9 header. All fields are little-endian u32/u64 at fixed offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// `0x04`: end of the encrypted front tables; the payload starts here.
    pub payload_start: u32,
    /// `0x08`: split between table 1 (`0x40..split`) and table 2 (`split..payload_start`).
    pub table_split: u32,
    /// `0x0C`: trailer length; the trailer is the last `trailer_len` bytes.
    pub trailer_len: u32,
    /// `0x10`: per-archive id / salt. Role undetermined.
    pub archive_id: u64,
    /// `0x18`: generation word, observed as `(gen << 16) | 0x7A00` with gen 4..=8.
    pub generation: u32,
    /// `0x20`: version pair. High u16 observed 1..=8, low u16 3..=13.
    pub version: u32,
    /// `0x24`: declared entry count.
    pub entry_count: u32,
    /// `0x28`: profile / key-slot id.
    pub key_id: u32,
    /// `0x2C`: flags, observed `0x90F01` or `0x90F02`. Low byte selects the table-2 record width.
    pub flags: u32,
    /// `0x1C`: generation-dependent constant (`0x150` in every gen-4 header). Meaning unknown.
    pub unknown_1c: u32,
    /// `0x30`: generation-dependent constant (`0x10` in every gen-4 header), described
    /// as a block unit. Other generations show other values, so treat as unknown.
    pub block_unit: u32,
    /// `0x34..0x40` was zero in every header measured by both sources.
    pub reserved_zero: bool,
}

impl Header {
    /// High u16 of `version` (selects the table-2 layout per 0x52504639 docs).
    pub fn version_major(&self) -> u16 {
        (self.version >> 16) as u16
    }

    /// Low u16 of `version`.
    pub fn version_minor(&self) -> u16 {
        self.version as u16
    }

    /// Generation number when the generation word has the observed
    /// `(gen << 16) | 0x7A00` shape.
    pub fn generation_number(&self) -> Option<u16> {
        (self.generation & 0xFFFF == 0x7A00).then_some((self.generation >> 16) as u16)
    }

    /// Table-2 record stride in bytes: `8 + low_byte(flags)`, i.e. 9 or 10.
    pub fn table2_stride(&self) -> Option<u32> {
        match self.flags & 0xFF {
            1 => Some(9),
            2 => Some(10),
            _ => None,
        }
    }

    /// Length of table 1 (`0x40..table_split`).
    pub fn table1_len(&self) -> u32 {
        self.table_split.saturating_sub(HEADER_LEN as u32)
    }

    /// Length of table 2 (`table_split..payload_start`).
    pub fn table2_len(&self) -> u32 {
        self.payload_start.saturating_sub(self.table_split)
    }

    /// Whether table 2 has the length `align16(stride * entry_count)`.
    ///
    /// The formula is only documented for `version_major() == 1` (all such headers
    /// in both corpora); `None` for other layouts, whose record width is unknown.
    pub fn table2_matches_count(&self) -> Option<bool> {
        if self.version_major() != 1 {
            return None;
        }
        let stride = u64::from(self.table2_stride()?);
        let expected = (stride * u64::from(self.entry_count)).div_ceil(16) * 16;
        Some(expected == u64::from(self.table2_len()))
    }

    /// Human-readable list of fields that deviate from everything observed so
    /// far. `available` is the number of bytes from the header start to the end
    /// of the containing buffer, when known.
    pub fn issues(&self, available: Option<u64>) -> Vec<String> {
        let mut out = Vec::new();
        if !self.payload_start.is_multiple_of(16) {
            out.push(format!(
                "payload_start {:#x} not 16-byte aligned",
                self.payload_start
            ));
        }
        if !self.table_split.is_multiple_of(16) {
            out.push(format!(
                "table_split {:#x} not 16-byte aligned",
                self.table_split
            ));
        }
        if !self.trailer_len.is_multiple_of(16) {
            out.push(format!(
                "trailer_len {:#x} not 16-byte aligned",
                self.trailer_len
            ));
        }
        if self.payload_start > MAX_FRONT_LEN {
            out.push(format!(
                "payload_start {:#x} larger than {MAX_FRONT_LEN:#x}",
                self.payload_start
            ));
        }
        if let Some(avail) = available {
            if u64::from(self.payload_start) > avail {
                out.push(format!(
                    "payload_start {:#x} past end of data ({avail:#x} bytes)",
                    self.payload_start
                ));
            }
            if u64::from(self.payload_start) + u64::from(self.trailer_len) > avail {
                out.push(format!(
                    "front region plus trailer ({:#x} + {:#x}) exceeds {avail:#x} bytes",
                    self.payload_start, self.trailer_len
                ));
            }
        }
        if self.entry_count == 0 || self.entry_count > MAX_ENTRY_COUNT {
            out.push(format!("entry_count {} out of range", self.entry_count));
        }
        if self.table2_stride().is_none() {
            out.push(format!("flags {:#x}: low byte is not 1 or 2", self.flags));
        }
        if self.flags & !0xFF != 0x9_0F00 {
            out.push(format!(
                "flags {:#x}: upper bits differ from observed 0x90Fxx",
                self.flags
            ));
        }
        match self.generation_number() {
            None => out.push(format!(
                "generation word {:#x} lacks the 0x7A00 low half",
                self.generation
            )),
            Some(g) if !(4..=8).contains(&g) => {
                out.push(format!("generation {g} outside observed 4..=8"))
            }
            Some(_) => {}
        }
        if !(1..=8).contains(&self.version_major()) || !(3..=13).contains(&self.version_minor()) {
            out.push(format!(
                "version {}.{} outside observed 1..=8 / 3..=13",
                self.version_major(),
                self.version_minor()
            ));
        }
        if self.table2_matches_count() == Some(false) {
            out.push(format!(
                "table 2 length {:#x} does not match {} records of {} bytes",
                self.table2_len(),
                self.entry_count,
                self.table2_stride().unwrap_or(0)
            ));
        }
        if !self.reserved_zero {
            out.push("bytes 0x34..0x40 are not zero".to_owned());
        }
        out
    }

    /// Strict check used before accepting a `9FPR` signature found inside other
    /// data as a nested archive. Only fields that were constant across every
    /// measured header are hard requirements; the rest is logged by callers.
    pub fn is_plausible_nested(&self, available: u64) -> bool {
        self.payload_start.is_multiple_of(16)
            && self.table_split.is_multiple_of(16)
            && self.trailer_len.is_multiple_of(16)
            && self.payload_start >= 0x60
            && self.payload_start <= MAX_FRONT_LEN
            && u64::from(self.payload_start) <= available
            && (1..=MAX_ENTRY_COUNT).contains(&self.entry_count)
            && self.table2_stride().is_some()
            && self.generation_number().is_some()
            && (1..=16).contains(&self.version_major())
            && self.reserved_zero
    }
}

/// Parse the 64-byte header at the start of `data`.
///
/// Requires the magic and a structurally possible layout
/// (`0x40 <= table_split <= payload_start`). It does not require
/// `payload_start <= data.len()`, so a bare 64-byte slice can be parsed; use
/// [`Header::issues`] or [`Header::is_plausible_nested`] for stricter checks.
pub fn parse_header(data: &[u8]) -> Result<Header, Rpf9Error> {
    let bad = |reason: String| Rpf9Error::BadHeader { offset: 0, reason };
    let r = Reader::new(data);
    let magic = r.u32_at(0).map_err(|e| bad(e.to_string()))?;
    if magic != MAGIC {
        return Err(bad(format!(
            "magic {magic:#010x}, expected {MAGIC:#010x} (\"9FPR\")"
        )));
    }
    let field = |off: usize| r.u32_at(off).map_err(|e| bad(e.to_string()));
    let payload_start = field(0x04)?;
    let table_split = field(0x08)?;
    let trailer_len = field(0x0C)?;
    let archive_id = r.u64_at(0x10).map_err(|e| bad(e.to_string()))?;
    let generation = field(0x18)?;
    let unknown_1c = field(0x1C)?;
    let version = field(0x20)?;
    let entry_count = field(0x24)?;
    let key_id = field(0x28)?;
    let flags = field(0x2C)?;
    let block_unit = field(0x30)?;
    let reserved_zero = r
        .bytes_at(0x34, 12)
        .map_err(|e| bad(e.to_string()))?
        .iter()
        .all(|&b| b == 0);

    if (payload_start as usize) < HEADER_LEN {
        return Err(bad(format!(
            "payload_start {payload_start:#x} inside the 64-byte header"
        )));
    }
    if (table_split as usize) < HEADER_LEN || table_split > payload_start {
        return Err(bad(format!(
            "table_split {table_split:#x} not within 0x40..={payload_start:#x}"
        )));
    }
    Ok(Header {
        payload_start,
        table_split,
        trailer_len,
        archive_id,
        generation,
        version,
        entry_count,
        key_id,
        flags,
        unknown_1c,
        block_unit,
        reserved_zero,
    })
}

/// Parse and validate a header at `offset` inside `data` as a nested archive.
/// Returns `None` (with a debug log) when the signature is a false positive.
pub(crate) fn nested_header_at(data: &[u8], offset: usize) -> Option<Header> {
    let slice = data.get(offset..)?;
    if slice.get(..4)? != b"9FPR" {
        return None;
    }
    match parse_header(slice) {
        Ok(h) if h.is_plausible_nested(slice.len() as u64) => {
            let issues = h.issues(Some(slice.len() as u64));
            if !issues.is_empty() {
                log::debug!("nested RPF9 at {offset:#x} accepted with notes: {issues:?}");
            }
            Some(h)
        }
        Ok(h) => {
            log::debug!(
                "9FPR at {offset:#x} rejected as nested archive: {:?}",
                h.issues(Some(slice.len() as u64))
            );
            None
        }
        Err(e) => {
            log::debug!("9FPR at {offset:#x} rejected: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Vec<u8> {
        let mut h = vec![0u8; 64];
        let put =
            |h: &mut Vec<u8>, off: usize, v: u32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
        h[0..4].copy_from_slice(b"9FPR");
        put(&mut h, 0x04, 0x340);
        put(&mut h, 0x08, 0x310);
        put(&mut h, 0x0C, 0x90);
        h[0x10..0x18].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        put(&mut h, 0x18, 0x4_7A00);
        put(&mut h, 0x1C, 0x150);
        put(&mut h, 0x20, 0x1_0005);
        put(&mut h, 0x24, 5);
        put(&mut h, 0x28, 2);
        put(&mut h, 0x2C, 0x9_0F01);
        put(&mut h, 0x30, 0x10);
        h
    }

    #[test]
    fn parses_fields() {
        let h = parse_header(&sample()).unwrap();
        assert_eq!(h.payload_start, 0x340);
        assert_eq!(h.table_split, 0x310);
        assert_eq!(h.trailer_len, 0x90);
        assert_eq!(h.archive_id, 0x1122_3344_5566_7788);
        assert_eq!(h.generation_number(), Some(4));
        assert_eq!(h.version_major(), 1);
        assert_eq!(h.version_minor(), 5);
        assert_eq!(h.table2_stride(), Some(9));
        // 5 * 9 = 45 -> 48 = 0x30 = 0x340 - 0x310.
        assert_eq!(h.table2_matches_count(), Some(true));
        assert!(
            h.issues(Some(0x1000)).is_empty(),
            "{:?}",
            h.issues(Some(0x1000))
        );
        assert!(h.is_plausible_nested(0x1000));
        assert!(!h.is_plausible_nested(0x100));
    }

    #[test]
    fn rejects_bad() {
        assert!(parse_header(b"9FPR").is_err());
        assert!(parse_header(&[0u8; 64]).is_err());
        let mut s = sample();
        s[0x08..0x0C].copy_from_slice(&0x400u32.to_le_bytes());
        assert!(parse_header(&s).is_err());
        let mut s = sample();
        s[0x04..0x08].copy_from_slice(&0x10u32.to_le_bytes());
        assert!(parse_header(&s).is_err());
    }
}
