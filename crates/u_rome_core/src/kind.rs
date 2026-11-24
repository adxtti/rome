//! Asset kind detection from leading bytes.
//!
//! The RPF9 table of contents is encrypted, so file types are recognised from
//! content. Signatures come from public community format notes; extend
//! `detect` as new formats are identified.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AssetKind {
    /// Nested RPF9 archive (`9FPR`).
    Archive,
    /// Texture chunk store (`RDS5`).
    TextureStore,
    /// Texture descriptor table (`ETPF`).
    TextureTable,
    /// PSO data document (`PSIN`).
    Pso,
    /// Meta resource (`0DRP`, PRD0 type 0x7a), e.g. map placements.
    MetaResource,
    /// Audio bank (`ADAT`, AWC).
    Audio,
    /// Bink 2 video (`KB2*`).
    Video,
    /// Model vertex buffer blob (u32 magic 0x4042).
    MeshBlob,
    /// XML text.
    Xml,
    /// Anything else.
    Unknown,
}

impl AssetKind {
    pub fn label(self) -> &'static str {
        match self {
            AssetKind::Archive => "Archive",
            AssetKind::TextureStore => "Texture store",
            AssetKind::TextureTable => "Texture table",
            AssetKind::Pso => "PSO data",
            AssetKind::MetaResource => "Meta resource",
            AssetKind::Audio => "Audio bank",
            AssetKind::Video => "Video",
            AssetKind::MeshBlob => "Mesh",
            AssetKind::Xml => "XML",
            AssetKind::Unknown => "Unknown",
        }
    }
}

/// Guess the asset kind from the first bytes of a decoded entry.
pub fn detect(data: &[u8]) -> AssetKind {
    let m = |sig: &[u8]| data.len() >= sig.len() && &data[..sig.len()] == sig;
    if m(b"9FPR") {
        AssetKind::Archive
    } else if m(b"RDS5") {
        AssetKind::TextureStore
    } else if m(b"ETPF") {
        AssetKind::TextureTable
    } else if m(b"PSIN") {
        AssetKind::Pso
    } else if m(b"0DRP") {
        AssetKind::MetaResource
    } else if m(b"ADAT") {
        AssetKind::Audio
    } else if m(b"KB2") || m(b"BIK") {
        AssetKind::Video
    } else if m(&0x4042u32.to_le_bytes()) {
        AssetKind::MeshBlob
    } else if m(b"<?xml") || m(b"\xEF\xBB\xBF<?xml") {
        AssetKind::Xml
    } else {
        AssetKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects() {
        assert_eq!(detect(b"9FPR...."), AssetKind::Archive);
        assert_eq!(detect(b"ADAT\x01\x00"), AssetKind::Audio);
        assert_eq!(detect(b"<?xml version"), AssetKind::Xml);
        assert_eq!(detect(b"zz"), AssetKind::Unknown);
        assert_eq!(detect(b""), AssetKind::Unknown);
    }
}
