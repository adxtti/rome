//! PNG and DDS export.

use crate::error::{Error, Result};
use crate::format::DxgiFormat;
use u_rome_core::{Image, ImageFormat};

/// Detiled but not decoded texture data: what [`crate::TextureStore::decode_raw`]
/// returns and [`export_dds_raw`] writes losslessly.
#[derive(Clone, Debug)]
pub struct RawTexture {
    pub format: DxgiFormat,
    /// Size of `mips[0]` in pixels.
    pub width: u32,
    pub height: u32,
    /// Store mip index of `mips[0]` (non-zero when larger mips were skipped).
    pub first_mip: u32,
    /// Linear element data per level, largest first: `ceil(w/4) * ceil(h/4)`
    /// blocks for BCn, `w * h` texels otherwise.
    pub mips: Vec<Vec<u8>>,
    /// True when BCPack or a disputed layout field was involved.
    pub provisional: bool,
    /// Why the data is provisional, for display.
    pub notes: Vec<String>,
}

fn level_dims(width: u32, height: u32, level: usize) -> (u32, u32) {
    let l = level.min(31) as u32;
    ((width >> l).max(1), (height >> l).max(1))
}

/// Encode mip `mip` of `image` as an RGBA8 PNG.
pub fn export_png(image: &Image, mip: usize) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    let data = image.mips.get(mip).ok_or(Error::MipOutOfRange {
        mip: mip as u32,
        count: image.mips.len() as u32,
    })?;
    let (w, h) = level_dims(image.width, image.height, mip);
    let need = w as u64 * h as u64 * 4;
    if data.len() as u64 != need {
        return Err(Error::InvalidArgument(format!(
            "mip {mip} has {} bytes, expected {need} for {w}x{h} RGBA8",
            data.len()
        )));
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(data, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| Error::Encode(e.to_string()))?;
    Ok(out)
}

/// Write a decoded RGBA8 image (all mips) as a DDS file with a DX10 header
/// (`R8G8B8A8_UNORM`, or `_SRGB` for sRGB images).
pub fn export_dds(image: &Image) -> Result<Vec<u8>> {
    let format = match image.format {
        ImageFormat::Rgba8 => DxgiFormat::R8G8B8A8_UNORM,
        ImageFormat::Rgba8Srgb => DxgiFormat::R8G8B8A8_UNORM_SRGB,
    };
    write_dds(format, image.width, image.height, &image.mips)
}

/// Write detiled texture data in its original DXGI format (lossless; BCn stays
/// block-compressed) as a DDS file with a DX10 header. Typeless formats are
/// written as their UNORM variant.
pub fn export_dds_raw(raw: &RawTexture) -> Result<Vec<u8>> {
    write_dds(raw.format.typed(), raw.width, raw.height, &raw.mips)
}

const DDSD_CAPS: u32 = 0x1;
const DDSD_HEIGHT: u32 = 0x2;
const DDSD_WIDTH: u32 = 0x4;
const DDSD_PITCH: u32 = 0x8;
const DDSD_PIXELFORMAT: u32 = 0x1000;
const DDSD_MIPMAPCOUNT: u32 = 0x20000;
const DDSD_LINEARSIZE: u32 = 0x80000;
const DDPF_FOURCC: u32 = 0x4;
const DDSCAPS_COMPLEX: u32 = 0x8;
const DDSCAPS_TEXTURE: u32 = 0x1000;
const DDSCAPS_MIPMAP: u32 = 0x40_0000;
const D3D10_RESOURCE_DIMENSION_TEXTURE2D: u32 = 3;

fn write_dds(format: DxgiFormat, width: u32, height: u32, mips: &[Vec<u8>]) -> Result<Vec<u8>> {
    let layout = format.layout().ok_or_else(|| Error::Unsupported {
        codec: "DDS export".into(),
        format: format.to_string(),
    })?;
    if mips.is_empty() || width == 0 || height == 0 {
        return Err(Error::InvalidArgument(
            "DDS export needs at least one mip".into(),
        ));
    }
    let mut total = 148u64;
    for (i, m) in mips.iter().enumerate() {
        let (w, h) = level_dims(width, height, i);
        let need = layout.linear_size(w, h);
        if m.len() as u64 != need {
            return Err(Error::InvalidArgument(format!(
                "mip {i}: {} bytes, expected {need} for {w}x{h} {format}",
                m.len()
            )));
        }
        total += need;
    }
    let mut d = Vec::new();
    crate::gfx10::reserve(&mut d, total, "DDS file")?;
    let mut put = |v: u32| d.extend_from_slice(&v.to_le_bytes());
    let compressed = layout.is_block_compressed();
    let mut flags = DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PIXELFORMAT | DDSD_MIPMAPCOUNT;
    flags |= if compressed {
        DDSD_LINEARSIZE
    } else {
        DDSD_PITCH
    };
    let pitch = if compressed {
        layout.linear_size(width, height).min(u32::MAX as u64) as u32
    } else {
        width.saturating_mul(layout.bytes_per_element)
    };
    let mut caps = DDSCAPS_TEXTURE;
    if mips.len() > 1 {
        caps |= DDSCAPS_COMPLEX | DDSCAPS_MIPMAP;
    }
    put(u32::from_le_bytes(*b"DDS "));
    put(124); // dwSize
    put(flags);
    put(height);
    put(width);
    put(pitch);
    put(0); // depth
    put(mips.len() as u32);
    for _ in 0..11 {
        put(0); // dwReserved1
    }
    put(32); // ddspf.dwSize
    put(DDPF_FOURCC);
    put(u32::from_le_bytes(*b"DX10"));
    for _ in 0..5 {
        put(0); // bit count and masks
    }
    put(caps);
    for _ in 0..4 {
        put(0); // caps2, caps3, caps4, reserved2
    }
    // DDS_HEADER_DXT10
    put(format.0);
    put(D3D10_RESOURCE_DIMENSION_TEXTURE2D);
    put(0); // miscFlag
    put(1); // arraySize
    put(0); // miscFlags2 (alpha mode unknown)
    for m in mips {
        d.extend_from_slice(m);
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_image() -> Image {
        let mut m0 = Vec::new();
        for y in 0..4u8 {
            for x in 0..6u8 {
                m0.extend_from_slice(&[x * 40, y * 60, 7, 255 - x]);
            }
        }
        let m1 = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        ];
        Image {
            width: 6,
            height: 4,
            format: ImageFormat::Rgba8,
            mips: vec![m0, m1],
            provisional: false,
        }
    }

    #[test]
    fn png_round_trip() {
        let img = test_image();
        for mip in 0..2 {
            let png = export_png(&img, mip).unwrap();
            let back = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8();
            let (w, h) = level_dims(6, 4, mip);
            assert_eq!(back.dimensions(), (w, h));
            assert_eq!(back.as_raw(), &img.mips[mip]);
        }
        assert!(export_png(&img, 2).is_err());
        let mut bad = img.clone();
        bad.mips[0].pop();
        assert!(export_png(&bad, 0).is_err());
    }

    fn u32_at(d: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
    }

    #[test]
    fn dds_rgba_header() {
        let img = test_image();
        let d = export_dds(&img).unwrap();
        assert_eq!(&d[0..4], b"DDS ");
        assert_eq!(u32_at(&d, 4), 124);
        assert_eq!(u32_at(&d, 12), 4); // height
        assert_eq!(u32_at(&d, 16), 6); // width
        assert_eq!(u32_at(&d, 20), 24); // pitch
        assert_eq!(u32_at(&d, 28), 2); // mips
        assert_eq!(&d[84..88], b"DX10");
        assert_eq!(u32_at(&d, 128), 28);
        assert_eq!(u32_at(&d, 132), 3);
        assert_eq!(u32_at(&d, 140), 1);
        assert_eq!(d.len(), 148 + 96 + 24);
        assert_eq!(&d[148..148 + 96], &img.mips[0][..]);
    }

    #[test]
    fn dds_raw_bc1() {
        let raw = RawTexture {
            format: DxgiFormat::BC1_TYPELESS,
            width: 8,
            height: 8,
            first_mip: 0,
            mips: vec![vec![0xAB; 32], vec![0xCD; 8], vec![0xEF; 8], vec![0x12; 8]],
            provisional: false,
            notes: vec![],
        };
        let d = export_dds_raw(&raw).unwrap();
        assert_eq!(u32_at(&d, 8) & DDSD_LINEARSIZE, DDSD_LINEARSIZE);
        assert_eq!(u32_at(&d, 20), 32);
        assert_eq!(u32_at(&d, 128), 71); // typeless -> UNORM
        assert_eq!(d.len(), 148 + 56);
        let mut bad = raw.clone();
        bad.mips[1].push(0);
        assert!(export_dds_raw(&bad).is_err());
        bad.format = DxgiFormat(126);
        assert!(matches!(
            export_dds_raw(&bad),
            Err(Error::Unsupported { .. })
        ));
    }
}
