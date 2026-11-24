//! Jenkins one-at-a-time (JOAAT) hashing, as used by RAGE for names.
//!
//! Community research disagrees on whether GTA VI lowercases names before
//! hashing, so both variants are exposed.

use std::fmt;

/// A 32-bit JOAAT name hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash32(pub u32);

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

/// Case-sensitive JOAAT over the raw bytes.
pub const fn joaat(s: &str) -> Hash32 {
    let b = s.as_bytes();
    let mut h: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        h = h.wrapping_add(b[i] as u32);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        i += 1;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h = h.wrapping_add(h << 15);
    Hash32(h)
}

/// JOAAT over the ASCII-lowercased bytes (the GTA V convention).
pub const fn joaat_lower(s: &str) -> Hash32 {
    let b = s.as_bytes();
    let mut h: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        h = h.wrapping_add(b[i].to_ascii_lowercase() as u32);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        i += 1;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h = h.wrapping_add(h << 15);
    Hash32(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // Well-known GTA V model hash: joaat("adder") = 0xB779A091.
        assert_eq!(joaat_lower("adder"), Hash32(0xB779_A091));
        assert_eq!(joaat_lower("ADDER"), Hash32(0xB779_A091));
        assert_eq!(joaat("adder"), Hash32(0xB779_A091));
        assert_ne!(joaat("ADDER"), Hash32(0xB779_A091));
        assert_eq!(joaat(""), Hash32(0));
    }
}
