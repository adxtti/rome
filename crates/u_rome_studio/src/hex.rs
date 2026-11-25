//! Pure helpers behind the Hex panel: row formatting, offset and pattern
//! parsing, chunked pattern search and copy formats.

use crate::backend::{BackendError, ByteSource};
use crate::jobs::{JobContext, JobError};

/// Largest selection the Copy actions will materialise.
pub const MAX_COPY_BYTES: u64 = 16 * 1024 * 1024;
/// Longest accepted search pattern.
pub const MAX_PATTERN_LEN: usize = 4096;
/// Bytes read per search step.
pub const SEARCH_CHUNK: usize = 1 << 20;

/// Printable ASCII or `.`.
pub fn ascii_char(b: u8) -> char {
    if (0x20..0x7F).contains(&b) {
        b as char
    } else {
        '.'
    }
}

/// Number of hex digits needed to show offsets up to `len` (min 8).
pub fn offset_digits(len: u64) -> usize {
    let bits = 64 - len.saturating_sub(1).leading_zeros() as usize;
    bits.div_ceil(4).max(8)
}

/// One formatted row: hex bytes (with an extra gap every 8) and ASCII.
/// Missing trailing bytes are padded so columns stay aligned.
pub fn format_row(bytes: &[u8], per_row: usize) -> (String, String) {
    let mut hex = String::with_capacity(per_row * 3 + per_row / 8);
    let mut ascii = String::with_capacity(per_row);
    for i in 0..per_row {
        if i > 0 {
            hex.push(' ');
            if i % 8 == 0 {
                hex.push(' ');
            }
        }
        match bytes.get(i) {
            Some(b) => {
                hex.push_str(&format!("{b:02X}"));
                ascii.push(ascii_char(*b));
            }
            None => {
                hex.push_str("  ");
                ascii.push(' ');
            }
        }
    }
    (hex, ascii)
}

/// Character column of byte `i` inside a row produced by [`format_row`].
pub fn hex_column(i: usize) -> usize {
    i * 3 + i / 8
}

/// Inverse of [`hex_column`]: which byte a character column falls on.
pub fn byte_at_column(col: usize, per_row: usize) -> Option<usize> {
    (0..per_row).find(|&i| {
        let start = hex_column(i);
        col >= start && col < start + 3
    })
}

/// Parses a goto expression.
///
/// Accepts `0x1F`, `1Fh`, `$1F` (hex), `1234` (decimal), `+0x10` / `-16`
/// (relative to `current`), `end`. Result must be `< len` (or 0 when empty).
pub fn parse_offset(input: &str, current: u64, len: u64) -> Result<u64, String> {
    let s = input.trim().replace('_', "");
    if s.is_empty() {
        return Err("empty offset".into());
    }
    if s.eq_ignore_ascii_case("end") {
        return Ok(len.saturating_sub(1));
    }
    let (sign, body) = match s.as_bytes()[0] {
        b'+' => (1i8, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (0, s.as_str()),
    };
    let body = body.trim();
    let lower = body.to_ascii_lowercase();
    let value = if let Some(h) = lower.strip_prefix("0x").or_else(|| lower.strip_prefix('$')) {
        u64::from_str_radix(h, 16)
    } else if let Some(h) = lower.strip_suffix('h') {
        u64::from_str_radix(h, 16)
    } else {
        lower.parse::<u64>()
    }
    .map_err(|_| format!("not a number: {body:?}"))?;
    let target = match sign {
        1 => current.checked_add(value),
        -1 => current.checked_sub(value),
        _ => Some(value),
    }
    .ok_or_else(|| "offset overflows".to_string())?;
    if target >= len.max(1) {
        return Err(format!(
            "offset {target:#x} is past the end ({len:#x} bytes)"
        ));
    }
    Ok(target)
}

/// A byte pattern; `None` matches any byte.
pub type Pattern = Vec<Option<u8>>;

/// Parses `DE AD ?? EF`, `DEADBEEF`, or a quoted ASCII string `"UROM"`.
pub fn parse_pattern(input: &str) -> Result<Pattern, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty pattern".into());
    }
    let pat: Pattern = if let Some(text) = s
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
    {
        text.bytes().map(Some).collect()
    } else {
        let compact: String = s
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ',')
            .collect();
        let compact = compact.replace("0x", "").replace("0X", "");
        if !compact.len().is_multiple_of(2) {
            return Err("hex pattern needs an even number of digits".into());
        }
        let chars: Vec<char> = compact.chars().collect();
        chars
            .chunks(2)
            .map(|pair| {
                let p: String = pair.iter().collect();
                if p == "??" {
                    Ok(None)
                } else {
                    u8::from_str_radix(&p, 16)
                        .map(Some)
                        .map_err(|_| format!("invalid hex byte {p:?}"))
                }
            })
            .collect::<Result<_, _>>()?
    };
    if pat.is_empty() {
        return Err("empty pattern".into());
    }
    if pat.len() > MAX_PATTERN_LEN {
        return Err(format!("pattern longer than {MAX_PATTERN_LEN} bytes"));
    }
    if pat.iter().all(Option::is_none) {
        return Err("pattern is all wildcards".into());
    }
    Ok(pat)
}

fn find_in(hay: &[u8], pat: &Pattern) -> Option<usize> {
    if pat.len() > hay.len() {
        return None;
    }
    let first = pat.iter().position(Option::is_some)?;
    let fb = pat[first]?;
    let last_start = hay.len() - pat.len();
    let mut i = 0;
    while i <= last_start {
        // Skip ahead to the next occurrence of the first concrete byte.
        let window = hay.get(i + first..=last_start + first)?;
        i += window.iter().position(|&b| b == fb)?;
        let cand = hay.get(i..i + pat.len())?;
        if pat.iter().zip(cand).all(|(p, b)| p.is_none_or(|p| p == *b)) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Finds the first match at or after `start`, wrapping to the beginning once.
/// Runs chunked (overlapping by `pattern.len() - 1`) and is cancellable.
pub fn find_pattern(
    src: &dyn ByteSource,
    pat: &Pattern,
    start: u64,
    ctx: &JobContext,
) -> Result<Option<u64>, JobError> {
    let len = src.len();
    if pat.is_empty() || len < pat.len() as u64 {
        return Ok(None);
    }
    let start = start.min(len);
    let total = len.max(1);
    let mut scanned = 0u64;
    let ranges = [(start, len), (0, (start + pat.len() as u64 - 1).min(len))];
    let overlap = pat.len() - 1;
    let mut buf = vec![0u8; SEARCH_CHUNK + overlap];
    for (from, to) in ranges {
        let mut pos = from;
        while pos < to {
            ctx.check()?;
            let want = ((to - pos) as usize).min(buf.len());
            let got = read_full(src, pos, &mut buf[..want]).map_err(JobError::from)?;
            if got == 0 {
                break;
            }
            if let Some(i) = find_in(&buf[..got], pat) {
                return Ok(Some(pos + i as u64));
            }
            if got < pat.len() || pos + got as u64 >= to {
                break;
            }
            let advance = (got - overlap) as u64;
            pos += advance;
            scanned += advance;
            ctx.set_progress(scanned, total);
        }
    }
    Ok(None)
}

fn read_full(src: &dyn ByteSource, offset: u64, buf: &mut [u8]) -> Result<usize, BackendError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = src.read_at(offset + filled as u64, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// `DE AD BE EF`
pub fn to_hex_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Printable ASCII with `.` for everything else.
pub fn to_ascii_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| ascii_char(*b)).collect()
}

/// `{ 0xDE, 0xAD, ... }` wrapped every 16 bytes.
pub fn to_c_array(bytes: &[u8]) -> String {
    let mut s = String::from("{\n");
    for chunk in bytes.chunks(16) {
        s.push_str("    ");
        let items: Vec<String> = chunk.iter().map(|b| format!("0x{b:02X}")).collect();
        s.push_str(&items.join(", "));
        s.push_str(",\n");
    }
    s.push('}');
    s
}

/// Inclusive selection normalised to `(start, len)`.
pub fn selection_range(anchor: u64, cursor: u64) -> (u64, u64) {
    let (a, b) = if anchor <= cursor {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    };
    (a, b - a + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemorySource;

    #[test]
    fn rows_format() {
        let (h, a) = format_row(&[0x41, 0x00, 0xFF, 0x7E], 4);
        assert_eq!(h, "41 00 FF 7E");
        assert_eq!(a, "A..~");
        let (h, a) = format_row(&(0u8..16).collect::<Vec<_>>(), 16);
        assert_eq!(h, "00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F");
        assert_eq!(a.len(), 16);
        let (h, a) = format_row(&[0x30], 4);
        assert_eq!(h, "30         ");
        assert_eq!(a, "0   ");
        assert_eq!(hex_column(8), 25);
        assert_eq!(&h[hex_column(0)..hex_column(0) + 2], "30");
        assert_eq!(byte_at_column(25, 16), Some(8));
        assert_eq!(byte_at_column(24, 16), None);
        assert_eq!(byte_at_column(1, 16), Some(0));
        assert_eq!(byte_at_column(999, 16), None);
    }

    #[test]
    fn offset_digits_grow() {
        assert_eq!(offset_digits(0), 8);
        assert_eq!(offset_digits(0x1_0000_0000), 8);
        assert_eq!(offset_digits(0x1_0000_0001), 9);
    }

    #[test]
    fn offsets_parse() {
        assert_eq!(parse_offset("0x10", 0, 100), Ok(16));
        assert_eq!(parse_offset("10h", 0, 100), Ok(16));
        assert_eq!(parse_offset("$10", 0, 100), Ok(16));
        assert_eq!(parse_offset("16", 0, 100), Ok(16));
        assert_eq!(parse_offset("+0x10", 4, 100), Ok(20));
        assert_eq!(parse_offset("-4", 10, 100), Ok(6));
        assert_eq!(parse_offset("end", 0, 100), Ok(99));
        assert_eq!(parse_offset("1_000", 0, 2000), Ok(1000));
        assert!(parse_offset("-20", 10, 100).is_err());
        assert!(parse_offset("100", 0, 100).is_err());
        assert!(parse_offset("zz", 0, 100).is_err());
        assert!(parse_offset("", 0, 100).is_err());
        assert!(parse_offset("+0xFFFFFFFFFFFFFFFF", 1, u64::MAX).is_err());
    }

    #[test]
    fn patterns_parse() {
        assert_eq!(
            parse_pattern("DE AD ?? ef"),
            Ok(vec![Some(0xDE), Some(0xAD), None, Some(0xEF)])
        );
        assert_eq!(parse_pattern("DEADBEEF").unwrap().len(), 4);
        assert_eq!(
            parse_pattern("0xDE, 0xAD").unwrap(),
            vec![Some(0xDE), Some(0xAD)]
        );
        assert_eq!(parse_pattern("\"AB\""), Ok(vec![Some(b'A'), Some(b'B')]));
        assert!(parse_pattern("ABC").is_err());
        assert!(parse_pattern("ZZ").is_err());
        assert!(parse_pattern("?? ??").is_err());
        assert!(parse_pattern("   ").is_err());
        assert!(parse_pattern("\"\"").is_err());
    }

    #[test]
    fn search_finds_across_chunks_and_wraps() {
        let mut data = vec![0u8; SEARCH_CHUNK * 2 + 100];
        let at = SEARCH_CHUNK - 2; // straddles the first chunk boundary
        data[at..at + 4].copy_from_slice(b"UROM");
        data[10..14].copy_from_slice(b"UROM");
        let src = MemorySource::new(data);
        let pat = parse_pattern("\"UROM\"").unwrap();
        let ctx = JobContext::detached();
        assert_eq!(find_pattern(&src, &pat, 0, &ctx), Ok(Some(10)));
        assert_eq!(find_pattern(&src, &pat, 11, &ctx), Ok(Some(at as u64)));
        // Wraps around to the first match.
        assert_eq!(find_pattern(&src, &pat, at as u64 + 1, &ctx), Ok(Some(10)));
        let wild = vec![Some(b'U'), None, Some(b'O')];
        assert_eq!(find_pattern(&src, &wild, 0, &ctx), Ok(Some(10)));
        let missing = parse_pattern("\"NOPE\"").unwrap();
        assert_eq!(find_pattern(&src, &missing, 0, &ctx), Ok(None));
        ctx.cancel();
        assert_eq!(
            find_pattern(&src, &missing, 0, &ctx),
            Err(JobError::Cancelled)
        );
    }

    #[test]
    fn search_edge_cases() {
        let ctx = JobContext::detached();
        let src = MemorySource::new(b"ab".to_vec());
        assert_eq!(find_pattern(&src, &vec![Some(b'b')], 5, &ctx), Ok(Some(1)));
        assert_eq!(find_pattern(&src, &vec![Some(1); 3], 0, &ctx), Ok(None));
        assert_eq!(find_in(b"aab", &vec![Some(b'a'), Some(b'b')]), Some(1));
        assert_eq!(find_in(b"xyz", &vec![None, Some(b'z')]), Some(1));
    }

    #[test]
    fn copy_formats() {
        assert_eq!(to_hex_string(&[1, 0xAB]), "01 AB");
        assert_eq!(to_ascii_string(b"a\x00b"), "a.b");
        assert_eq!(to_c_array(&[1, 2]), "{\n    0x01, 0x02,\n}");
        assert_eq!(selection_range(5, 2), (2, 4));
        assert_eq!(selection_range(2, 2), (2, 1));
    }
}
