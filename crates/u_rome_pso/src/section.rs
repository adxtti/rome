//! Generic section framing.
//!
//! Every section is an 8-byte header: a 4-byte ASCII tag followed by a
//! **big-endian** u32 size that includes the header itself. Section bodies are
//! little-endian. Sources: rage9-re `docs/SCRIPT_FORMAT.md` section 2,
//! rage-tools-vi `docs/data/script-schema.md`, GTA6-Asset-Info
//! `structure/04-pso-objectgraph.md` (all agree).

use u_rome_core::{Endian, Reader};

use crate::error::{PsoError, Result};

/// Upper bound on the number of sections walked. Known layouts use seven.
pub const MAX_SECTIONS: usize = 256;

/// One section of a PSIN document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Section {
    /// Four ASCII tag bytes, e.g. `*b"LSCH"`.
    pub tag: [u8; 4],
    /// Absolute offset of the section header in the document.
    pub offset: usize,
    /// Size including the 8-byte header (as stored, big-endian).
    pub size: u32,
}

impl Section {
    /// Tag as text. Tags are validated as printable ASCII during parsing.
    pub fn tag_str(&self) -> &str {
        std::str::from_utf8(&self.tag).unwrap_or("????")
    }

    /// Absolute offset of the first body byte.
    pub fn body_offset(&self) -> usize {
        self.offset + 8
    }

    /// Body length in bytes (size minus the header).
    pub fn body_len(&self) -> usize {
        (self.size as usize).saturating_sub(8)
    }

    /// Absolute offset one past the last byte of the section.
    pub fn end(&self) -> usize {
        self.offset + self.size as usize
    }

    pub fn is(&self, tag: &[u8; 4]) -> bool {
        &self.tag == tag
    }
}

fn is_tag_byte(b: u8) -> bool {
    b.is_ascii_graphic() || b == b' '
}

/// Walk all sections. Trailing all-zero padding after the last section is
/// tolerated (reported through `warnings`); anything else that does not frame
/// is an error.
pub(crate) fn walk(data: &[u8], warnings: &mut Vec<String>) -> Result<Vec<Section>> {
    let r = Reader::with_endian(data, Endian::Big);
    let mut sections = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let rest = data.get(pos..).unwrap_or(&[]);
        if rest.iter().all(|&b| b == 0) && !sections.is_empty() {
            crate::document::push_warning(
                warnings,
                format!(
                    "{} bytes of zero padding after the last section at offset {pos:#x}; ignored",
                    rest.len()
                ),
            );
            break;
        }
        if rest.len() < 8 {
            return Err(PsoError::BadSectionSize {
                tag: String::from_utf8_lossy(rest.get(..4).unwrap_or(rest)).into_owned(),
                offset: pos,
                size: rest.len() as u64,
                remaining: rest.len(),
                reason: "truncated section header",
            });
        }
        if sections.len() >= MAX_SECTIONS {
            return Err(PsoError::TooManySections { max: MAX_SECTIONS });
        }
        let mut tag = [0u8; 4];
        tag.copy_from_slice(r.bytes_at(pos, 4)?);
        if !tag.iter().all(|&b| is_tag_byte(b)) {
            return Err(PsoError::BadSectionTag { offset: pos, tag });
        }
        let size = r.u32_at(pos + 4)?;
        let tag_s = String::from_utf8_lossy(&tag).into_owned();
        if size < 8 {
            return Err(PsoError::BadSectionSize {
                tag: tag_s,
                offset: pos,
                size: size.into(),
                remaining: rest.len(),
                reason: "smaller than the 8-byte header",
            });
        }
        if size as usize > rest.len() {
            return Err(PsoError::BadSectionSize {
                tag: tag_s,
                offset: pos,
                size: size.into(),
                remaining: rest.len(),
                reason: "runs past the end of the buffer",
            });
        }
        log::debug!("pso: section {tag_s} at {pos:#x}, size {size:#x}");
        sections.push(Section {
            tag,
            offset: pos,
            size,
        });
        pos += size as usize;
    }
    Ok(sections)
}
