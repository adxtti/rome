//! Uncompressed payloads stored directly in the archive: ADAT audio banks and
//! Bink 2 video.

use u_rome_core::Reader;

/// Length of an `ADAT` audio bank starting at `data[0]`, and whether that
/// length is provisional. `None` when the signature is not a plausible bank.
///
/// Header checks (task brief, community research): u16 version 1 at +4,
/// u32 stream count 1..=65536 at +8, u32 data offset 0x10..=0xFFF0 at +12.
///
/// The size comes from the chunk table, using the GTA V AWC layout as read by
/// CodeWalker `AwcFile.cs` (MIT): u16 flags at +6 (bit 0 = a u16 chunk-index
/// table of `stream_count` entries follows the 16-byte header), then one u32
/// stream info per stream (bits 29..31 = chunk count), then one u64 chunk info
/// per chunk (bits 0..27 offset, 28..55 size, 56..63 type), offsets absolute
/// from the bank start. The bank ends at the furthest `offset + size`.
/// It is unconfirmed that GTA VI keeps this layout, so when the table is not
/// self-consistent the bank is reported with `data_offset` as its length and
/// the provisional flag set.
pub(crate) fn adat_len(data: &[u8]) -> Option<(u64, bool)> {
    if data.get(..4)? != b"ADAT" {
        return None;
    }
    let r = Reader::new(data);
    let version = r.u16_at(4).ok()?;
    let flags = r.u16_at(6).ok()?;
    let streams = r.u32_at(8).ok()?;
    let data_offset = r.u32_at(12).ok()?;
    if version != 1 || !(1..=65536).contains(&streams) || !(0x10..=0xFFF0).contains(&data_offset) {
        return None;
    }
    if data_offset as usize > data.len() {
        log::debug!(
            "ADAT data offset {data_offset:#x} past end of {} bytes",
            data.len()
        );
        return None;
    }
    match adat_chunk_end(&r, flags, streams, data_offset) {
        Ok(end) => Some((end, false)),
        Err(why) => {
            log::debug!("ADAT chunk table not usable ({why}); length provisional");
            Some((u64::from(data_offset), true))
        }
    }
}

fn adat_chunk_end(
    r: &Reader<'_>,
    flags: u16,
    streams: u32,
    data_offset: u32,
) -> Result<u64, String> {
    let streams = streams as usize;
    let mut pos = 16usize;
    if flags & 1 != 0 {
        pos += 2 * streams;
    }
    if pos + 4 * streams > data_offset as usize {
        return Err(format!(
            "{streams} stream infos do not fit before data offset {data_offset:#x}"
        ));
    }
    let mut chunks = 0usize;
    for i in 0..streams {
        let info = r.u32_at(pos + 4 * i).map_err(|e| e.to_string())?;
        chunks += (info >> 29) as usize;
    }
    pos += 4 * streams;
    let tables_end = pos + 8 * chunks;
    if tables_end > data_offset as usize {
        return Err(format!(
            "tables end {tables_end:#x} past data offset {data_offset:#x}"
        ));
    }
    if chunks == 0 {
        return Err("no chunks".to_owned());
    }
    let mut end = u64::from(data_offset);
    for c in 0..chunks {
        let v = r.u64_at(pos + 8 * c).map_err(|e| e.to_string())?;
        let off = v & 0x0FFF_FFFF;
        let size = (v >> 28) & 0x0FFF_FFFF;
        if size == 0 {
            continue;
        }
        if off < tables_end as u64 {
            return Err(format!("chunk {c} offset {off:#x} inside the tables"));
        }
        end = end.max(off + size);
    }
    if end > r.len() as u64 {
        return Err(format!(
            "chunks end at {end:#x}, past {:#x} available bytes",
            r.len()
        ));
    }
    Ok(end)
}

/// Length of a Bink 2 video (`KB2` + revision byte) starting at `data[0]`.
/// The u32 at +4 is the file size minus 8 (Bink header convention). The u32
/// at +8 is the frame count and must be non-zero.
pub(crate) fn bink_len(data: &[u8]) -> Option<u64> {
    if data.get(..3)? != b"KB2" || !data.get(3)?.is_ascii_alphanumeric() {
        return None;
    }
    let r = Reader::new(data);
    let size = u64::from(r.u32_at(4).ok()?) + 8;
    let frames = r.u32_at(8).ok()?;
    if size < 44 || frames == 0 {
        return None;
    }
    if size > data.len() as u64 {
        log::debug!(
            "Bink video of {size} bytes truncated at {} bytes",
            data.len()
        );
        return None;
    }
    Some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn adat(chunk_sizes: &[u32]) -> Vec<u8> {
        // One stream with N chunks, data offset 0x40.
        let mut v = Vec::new();
        v.extend_from_slice(b"ADAT");
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0xFF00u16.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&0x40u32.to_le_bytes());
        v.extend_from_slice(&(((chunk_sizes.len() as u32) << 29) | 0x1234).to_le_bytes());
        let mut off = 0x40u64;
        for &s in chunk_sizes {
            v.extend_from_slice(&(off | (u64::from(s) << 28) | (3u64 << 56)).to_le_bytes());
            off += u64::from(s);
        }
        v.resize(0x40, 0);
        v.resize(off as usize, 0xAB);
        v
    }

    #[test]
    fn adat_sizes() {
        let a = adat(&[100, 200]);
        assert_eq!(adat_len(&a), Some((0x40 + 300, false)));
        let mut bad = a.clone();
        bad[4] = 2;
        assert_eq!(adat_len(&bad), None);
        // Truncated payload: provisional fallback to data offset.
        assert_eq!(adat_len(&a[..0x80]), Some((0x40, true)));
        assert_eq!(adat_len(b"ADAT"), None);
    }

    #[test]
    fn bink_sizes() {
        let mut b = vec![0u8; 100];
        b[..4].copy_from_slice(b"KB2j");
        b[4..8].copy_from_slice(&92u32.to_le_bytes());
        b[8..12].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(bink_len(&b), Some(100));
        assert_eq!(bink_len(&b[..50]), None);
        b[8] = 0;
        assert_eq!(bink_len(&b), None);
    }
}
