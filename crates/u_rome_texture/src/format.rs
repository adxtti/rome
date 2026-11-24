//! DXGI pixel formats and chunk codecs.

use std::fmt;

/// A `DXGI_FORMAT` value as stored in ETPF descriptors (d1 bits 3-9).
///
/// IDs seen in the stores (ttpf-studio FORMATS.md, GTA6-Asset-Info §5): 28
/// RGBA8, 71 BC1, 77 BC3, 80 BC4, 83 BC5, 87 BGRA8, 95/96 BC6H, 98 BC7, plus
/// R16_FLOAT, RG8, R16G16B16A16, R16_UNORM, A8 and R8. sRGB variants are +1.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DxgiFormat(pub u32);

/// How a format's texels are stored and how they decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelKind {
    Bc1,
    Bc2,
    Bc3,
    Bc4 { signed: bool },
    Bc5 { signed: bool },
    Bc6h { signed: bool },
    Bc7,
    Rgba8,
    Bgra8,
    Bgrx8,
    Rgb10A2,
    Rgba16Unorm,
    Rgba16Float,
    Rgba32Float,
    Rg16Unorm,
    Rg16Float,
    Rg8,
    R32Float,
    R16Unorm,
    R16Float,
    R8,
    A8,
}

/// Storage layout of a format: one element is a 4x4 block for BCn, one texel otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FormatLayout {
    pub kind: PixelKind,
    /// 4 for block-compressed formats, 1 otherwise.
    pub block_dim: u32,
    /// Bytes per element (block or texel): 1, 2, 4, 8 or 16.
    pub bytes_per_element: u32,
}

impl FormatLayout {
    pub fn is_block_compressed(&self) -> bool {
        self.block_dim == 4
    }

    /// Element grid of a `width x height` pixel surface.
    pub fn elements(&self, width: u32, height: u32) -> (u32, u32) {
        (
            width.max(1).div_ceil(self.block_dim),
            height.max(1).div_ceil(self.block_dim),
        )
    }

    /// Unpadded linear byte size of a `width x height` pixel surface.
    pub fn linear_size(&self, width: u32, height: u32) -> u64 {
        let (ew, eh) = self.elements(width, height);
        ew as u64 * eh as u64 * self.bytes_per_element as u64
    }
}

impl DxgiFormat {
    pub const R32G32B32A32_FLOAT: Self = Self(2);
    pub const R16G16B16A16_TYPELESS: Self = Self(9);
    pub const R16G16B16A16_FLOAT: Self = Self(10);
    pub const R16G16B16A16_UNORM: Self = Self(11);
    pub const R10G10B10A2_UNORM: Self = Self(24);
    pub const R8G8B8A8_TYPELESS: Self = Self(27);
    pub const R8G8B8A8_UNORM: Self = Self(28);
    pub const R8G8B8A8_UNORM_SRGB: Self = Self(29);
    pub const R16G16_FLOAT: Self = Self(34);
    pub const R16G16_UNORM: Self = Self(35);
    pub const R32_FLOAT: Self = Self(41);
    pub const R8G8_UNORM: Self = Self(49);
    pub const R16_TYPELESS: Self = Self(53);
    pub const R16_FLOAT: Self = Self(54);
    pub const R16_UNORM: Self = Self(56);
    pub const R16_UINT: Self = Self(57);
    pub const R8_TYPELESS: Self = Self(60);
    pub const R8_UNORM: Self = Self(61);
    pub const A8_UNORM: Self = Self(65);
    pub const BC1_TYPELESS: Self = Self(70);
    pub const BC1_UNORM: Self = Self(71);
    pub const BC1_UNORM_SRGB: Self = Self(72);
    pub const BC2_TYPELESS: Self = Self(73);
    pub const BC2_UNORM: Self = Self(74);
    pub const BC2_UNORM_SRGB: Self = Self(75);
    pub const BC3_TYPELESS: Self = Self(76);
    pub const BC3_UNORM: Self = Self(77);
    pub const BC3_UNORM_SRGB: Self = Self(78);
    pub const BC4_TYPELESS: Self = Self(79);
    pub const BC4_UNORM: Self = Self(80);
    pub const BC4_SNORM: Self = Self(81);
    pub const BC5_TYPELESS: Self = Self(82);
    pub const BC5_UNORM: Self = Self(83);
    pub const BC5_SNORM: Self = Self(84);
    pub const B8G8R8A8_UNORM: Self = Self(87);
    pub const B8G8R8X8_UNORM: Self = Self(88);
    pub const B8G8R8A8_TYPELESS: Self = Self(90);
    pub const B8G8R8A8_UNORM_SRGB: Self = Self(91);
    pub const B8G8R8X8_TYPELESS: Self = Self(92);
    pub const B8G8R8X8_UNORM_SRGB: Self = Self(93);
    pub const BC6H_TYPELESS: Self = Self(94);
    pub const BC6H_UF16: Self = Self(95);
    pub const BC6H_SF16: Self = Self(96);
    pub const BC7_TYPELESS: Self = Self(97);
    pub const BC7_UNORM: Self = Self(98);
    pub const BC7_UNORM_SRGB: Self = Self(99);

    /// The `DXGI_FORMAT_*` name, or `None` for IDs this crate does not know.
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            2 => "R32G32B32A32_FLOAT",
            9 => "R16G16B16A16_TYPELESS",
            10 => "R16G16B16A16_FLOAT",
            11 => "R16G16B16A16_UNORM",
            24 => "R10G10B10A2_UNORM",
            27 => "R8G8B8A8_TYPELESS",
            28 => "R8G8B8A8_UNORM",
            29 => "R8G8B8A8_UNORM_SRGB",
            34 => "R16G16_FLOAT",
            35 => "R16G16_UNORM",
            41 => "R32_FLOAT",
            49 => "R8G8_UNORM",
            53 => "R16_TYPELESS",
            54 => "R16_FLOAT",
            56 => "R16_UNORM",
            57 => "R16_UINT",
            60 => "R8_TYPELESS",
            61 => "R8_UNORM",
            65 => "A8_UNORM",
            70 => "BC1_TYPELESS",
            71 => "BC1_UNORM",
            72 => "BC1_UNORM_SRGB",
            73 => "BC2_TYPELESS",
            74 => "BC2_UNORM",
            75 => "BC2_UNORM_SRGB",
            76 => "BC3_TYPELESS",
            77 => "BC3_UNORM",
            78 => "BC3_UNORM_SRGB",
            79 => "BC4_TYPELESS",
            80 => "BC4_UNORM",
            81 => "BC4_SNORM",
            82 => "BC5_TYPELESS",
            83 => "BC5_UNORM",
            84 => "BC5_SNORM",
            87 => "B8G8R8A8_UNORM",
            88 => "B8G8R8X8_UNORM",
            90 => "B8G8R8A8_TYPELESS",
            91 => "B8G8R8A8_UNORM_SRGB",
            92 => "B8G8R8X8_TYPELESS",
            93 => "B8G8R8X8_UNORM_SRGB",
            94 => "BC6H_TYPELESS",
            95 => "BC6H_UF16",
            96 => "BC6H_SF16",
            97 => "BC7_TYPELESS",
            98 => "BC7_UNORM",
            99 => "BC7_UNORM_SRGB",
            _ => return None,
        })
    }

    /// True for the `*_SRGB` variants.
    pub fn is_srgb(self) -> bool {
        matches!(self.0, 29 | 72 | 75 | 78 | 91 | 93 | 99)
    }

    /// Storage layout, or `None` when this crate cannot decode the format.
    pub fn layout(self) -> Option<FormatLayout> {
        use PixelKind::*;
        let (kind, block_dim, bpe) = match self.0 {
            70..=72 => (Bc1, 4, 8),
            73..=75 => (Bc2, 4, 16),
            76..=78 => (Bc3, 4, 16),
            79 | 80 => (Bc4 { signed: false }, 4, 8),
            81 => (Bc4 { signed: true }, 4, 8),
            82 | 83 => (Bc5 { signed: false }, 4, 16),
            84 => (Bc5 { signed: true }, 4, 16),
            94 | 95 => (Bc6h { signed: false }, 4, 16),
            96 => (Bc6h { signed: true }, 4, 16),
            97..=99 => (Bc7, 4, 16),
            27..=29 => (Rgba8, 1, 4),
            87 | 90 | 91 => (Bgra8, 1, 4),
            88 | 92 | 93 => (Bgrx8, 1, 4),
            24 => (Rgb10A2, 1, 4),
            9 | 11 => (Rgba16Unorm, 1, 8),
            10 => (Rgba16Float, 1, 8),
            2 => (Rgba32Float, 1, 16),
            35 => (Rg16Unorm, 1, 4),
            34 => (Rg16Float, 1, 4),
            49 => (Rg8, 1, 2),
            41 => (R32Float, 1, 4),
            53 | 56 | 57 => (R16Unorm, 1, 2),
            54 => (R16Float, 1, 2),
            60 | 61 => (R8, 1, 1),
            65 => (A8, 1, 1),
            _ => return None,
        };
        Some(FormatLayout {
            kind,
            block_dim,
            bytes_per_element: bpe,
        })
    }

    /// The same format with `TYPELESS` replaced by `UNORM`, for DDS writers
    /// (many tools refuse typeless DDS files).
    pub fn typed(self) -> DxgiFormat {
        DxgiFormat(match self.0 {
            9 => 11,
            27 => 28,
            53 => 56,
            60 => 61,
            70 => 71,
            73 => 74,
            76 => 77,
            79 => 80,
            82 => 83,
            90 => 87,
            92 => 88,
            94 => 95,
            97 => 98,
            x => x,
        })
    }
}

impl fmt::Display for DxgiFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => write!(f, "{n} ({})", self.0),
            None => write!(f, "DXGI format {}", self.0),
        }
    }
}

impl fmt::Debug for DxgiFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// How one RDS5 chunk is stored.
///
/// Codec ids (bcpack-rs `docs/BCPACK_SPEC.md` §1, GTA6-Asset-Info §5, Sentinel
/// `ttpf-rds5-etpf.md`, all agreeing): `0x00` stored, `0x20` zlib, BC number
/// x 4 for BCPack (`0x04` BC1 ... `0x1C` BC7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    Stored,
    Zlib,
    /// BCPack (Xbox Series MDU rANS) over BCn blocks; the value is the BC number 1-7.
    BcPack(u8),
    /// Any other id (raw value).
    Unknown(u8),
}

impl Codec {
    pub fn from_id(id: u8) -> Codec {
        match id {
            0x00 => Codec::Stored,
            0x20 => Codec::Zlib,
            0x04 | 0x08 | 0x0C | 0x10 | 0x14 | 0x18 | 0x1C => Codec::BcPack(id >> 2),
            x => Codec::Unknown(x),
        }
    }

    /// Stored or zlib: decodes exactly, never provisional.
    pub fn is_plain(self) -> bool {
        matches!(self, Codec::Stored | Codec::Zlib)
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Codec::Stored => f.write_str("stored"),
            Codec::Zlib => f.write_str("zlib"),
            Codec::BcPack(n) => write!(f, "BCPack BC{n}"),
            Codec::Unknown(x) => write!(f, "unknown codec {x:#04x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_formats() {
        let l = DxgiFormat::BC1_UNORM.layout().unwrap();
        assert_eq!((l.block_dim, l.bytes_per_element), (4, 8));
        assert_eq!(l.elements(5, 9), (2, 3));
        assert_eq!(
            DxgiFormat::BC7_UNORM.layout().unwrap().bytes_per_element,
            16
        );
        assert_eq!(DxgiFormat::R8G8B8A8_UNORM.layout().unwrap().block_dim, 1);
        assert!(DxgiFormat::BC7_UNORM_SRGB.is_srgb());
        assert!(!DxgiFormat::BC7_UNORM.is_srgb());
        assert!(DxgiFormat(127).layout().is_none());
        assert_eq!(DxgiFormat::BC1_TYPELESS.typed(), DxgiFormat::BC1_UNORM);
        assert_eq!(DxgiFormat(98).to_string(), "BC7_UNORM (98)");
        assert_eq!(DxgiFormat(126).to_string(), "DXGI format 126");
    }

    #[test]
    fn codecs() {
        assert_eq!(Codec::from_id(0x20), Codec::Zlib);
        assert_eq!(Codec::from_id(0x04), Codec::BcPack(1));
        assert_eq!(Codec::from_id(0x1C), Codec::BcPack(7));
        assert_eq!(Codec::from_id(0x24), Codec::Unknown(0x24));
        assert!(Codec::Stored.is_plain());
        assert!(!Codec::BcPack(1).is_plain());
    }
}
