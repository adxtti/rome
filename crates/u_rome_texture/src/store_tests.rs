//! Store tests on synthetic RDS5 / ETPF data built in code.

use std::io::Write;

use super::*;
use crate::bcn::tests::bc1_block;
use crate::gfx10::{TileGeometry, detile};

struct TestChunk {
    codec: u8,
    stored: Vec<u8>,
    vlen: u64,
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn chunk(codec: u8, virtual_bytes: &[u8]) -> TestChunk {
    let stored = if codec == 0x20 {
        zlib(virtual_bytes)
    } else {
        virtual_bytes.to_vec()
    };
    TestChunk {
        codec,
        stored,
        vlen: virtual_bytes.len() as u64,
    }
}

/// Descriptor words: (w, h, mip field, dxgi, array size, kind, tile mode, page).
#[allow(clippy::too_many_arguments)]
fn desc(
    w: u32,
    h: u32,
    field: u32,
    dxgi: u32,
    array: u32,
    kind: u32,
    tile: u32,
    page: u32,
) -> [u32; 4] {
    [
        (w - 1) | (h - 1) << 14 | field << 28,
        dxgi << 3 | (array - 1) << 10 | kind << 20 | tile << 24,
        page,
        0,
    ]
}

fn put(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn pad_to(v: &mut Vec<u8>, a: usize) {
    while !v.len().is_multiple_of(a) {
        v.push(0);
    }
}

fn etpf(textures: &[(u64, [u32; 4])]) -> Vec<u8> {
    let mut v = b"ETPF".to_vec();
    put(&mut v, 634);
    put(&mut v, textures.len() as u32);
    put(&mut v, (textures.len() * 24) as u32);
    for (_, d) in textures {
        for &w in d {
            put(&mut v, w);
        }
    }
    for (h, _) in textures {
        v.extend_from_slice(&h.to_le_bytes());
    }
    v
}

fn build(chunks: &[TestChunk], textures: &[(u64, [u32; 4])]) -> Vec<u8> {
    let n = chunks.len() + 1;
    let mut f = b"RDS5".to_vec();
    put(&mut f, n as u32);
    let vsize: u64 = chunks.iter().map(|c| c.vlen).sum();
    put(&mut f, vsize as u32);
    put(&mut f, 0x1600_0000 | (vsize >> 32) as u32);
    let table_end = 0x18 + 8 * n;
    let etpf_at = table_end.div_ceil(0x100) * 0x100;
    put(&mut f, (etpf_at >> 8) as u32);
    put(&mut f, 0);
    f.resize(etpf_at, 0);
    f.extend_from_slice(&etpf(textures));
    pad_to(&mut f, 0x100);
    let mut entries = Vec::new();
    let mut v = 0u64;
    for c in chunks {
        assert_eq!(c.vlen % 4096, 0);
        let off = f.len() as u64;
        f.extend_from_slice(&c.stored);
        pad_to(&mut f, 0x100);
        let pad = f.len() as u64 - off - c.stored.len() as u64;
        entries.push((off, c.codec, v, pad));
        v += c.vlen;
    }
    entries.push((f.len() as u64, 0, v, 0));
    for (i, &(off, codec, vs, pad)) in entries.iter().enumerate() {
        let page = vs >> 12;
        let a = (off >> 8) as u32 | (codec as u32) << 24 | ((page & 3) as u32) << 30;
        let b = (page >> 2) as u32 | (pad as u32) << 20;
        let o = 0x18 + 8 * i;
        f[o..o + 4].copy_from_slice(&a.to_le_bytes());
        f[o + 4..o + 8].copy_from_slice(&b.to_le_bytes());
    }
    f
}

/// Write row-major `linear` elements of level `m` into `buf` at `base`.
fn place(buf: &mut [u8], chain: &MipChain, m: usize, base: u64, linear: &[u8]) {
    let l = &chain.levels[m];
    let g: &TileGeometry = &chain.geometry;
    let bpe = g.bytes_per_element as usize;
    for y in 0..l.elem_height {
        for x in 0..l.elem_width {
            let s = ((y * l.elem_width + x) as usize) * bpe;
            let d =
                (base + g.element_offset(l.origin_x + x, l.origin_y + y, l.pitch_blocks)) as usize;
            buf[d..d + bpe].copy_from_slice(&linear[s..s + bpe]);
        }
    }
}

fn bc1_blocks(bw: u32, bh: u32, salt: u16) -> Vec<u8> {
    let mut v = Vec::new();
    for by in 0..bh {
        for bx in 0..bw {
            let c0 =
                ((bx as u16 * 2 + salt) & 31) << 11 | ((by as u16 * 3) & 63) << 5 | (salt & 31);
            v.extend_from_slice(&bc1_block(c0 | 0x8000, 0x0001, [((bx + by) % 4) as u8; 16]));
        }
    }
    v
}

fn rgba_level(w: u32, h: u32, b: u8) -> Vec<u8> {
    (0..w * h)
        .flat_map(|i| [(i % w) as u8 * 9, (i / w) as u8 * 13, b, 255])
        .collect()
}

struct Fixture {
    bytes: Vec<u8>,
    a_blocks: Vec<u8>,
    b_levels: Vec<Vec<u8>>,
    c_levels: Vec<Vec<u8>>,
    d_expected: Vec<u8>,
    f_slices: Vec<Vec<u8>>,
    g_levels: Vec<Vec<u8>>,
}

const TILE1: u32 = 1;

fn fixture() -> Fixture {
    // A: BC1 64x64, 1 mip, SW_4KB_S, v = 0. B: RGBA8 16x16, 2 mips (tail), v = 4096. Zlib chunk.
    let mut c0 = vec![0u8; 8192];
    let a_blocks = bc1_blocks(16, 16, 1);
    let ch = mip_chain(Swizzle::Sw4KbS, 8, 4, 64, 64, 1).unwrap();
    place(&mut c0, &ch, 0, 0, &a_blocks);
    let ch = mip_chain(Swizzle::Sw4KbS, 4, 1, 16, 16, 2).unwrap();
    assert_eq!(ch.first_tail_mip, Some(0));
    let b_levels = vec![rgba_level(16, 16, 1), rgba_level(8, 8, 2)];
    for (m, l) in b_levels.iter().enumerate() {
        place(&mut c0, &ch, m, 4096, l);
    }
    // C: BC1 128x128, 2 mips, tile mode 0 (SW_64KB_D, mip 0 under 64 KiB: disputed). Stored chunk.
    let mut c1 = vec![0u8; 65536];
    let ch = mip_chain(Swizzle::Sw64KbD, 8, 4, 128, 128, 2).unwrap();
    let c_levels = vec![bc1_blocks(32, 32, 2), bc1_blocks(16, 16, 3)];
    for (m, l) in c_levels.iter().enumerate() {
        place(&mut c1, &ch, m, 0, l);
    }
    // D: BC1 64x64 in a BCPack BC1 chunk of 512 blocks.
    let (stream, _) = bcpack::encode::synthetic_bc1(7, 512, 64);
    let (w, q) = bcpack_tables();
    let blocks = bcpack::bc1::decode(&stream, 512, w, q).unwrap().blocks;
    let d_expected = detile(&blocks, Swizzle::Sw4KbS, 8, 16, 16).unwrap();
    // E: BC7 32x32 in a BCPack BC7 chunk (unsupported).
    // F: RGBA8 16x16 array of 2 slices, 1 mip each.
    let mut c4 = vec![0u8; 8192];
    let ch = mip_chain(Swizzle::Sw4KbS, 4, 1, 16, 16, 1).unwrap();
    let f_slices = vec![rgba_level(16, 16, 10), rgba_level(16, 16, 20)];
    place(&mut c4, &ch, 0, 0, &f_slices[0]);
    place(&mut c4, &ch, 0, 4096, &f_slices[1]);
    // G: BC1 256x256, 3 mips: tail + mip 1 in zlib, mip 0 in BCPack.
    let ch = mip_chain(Swizzle::Sw4KbS, 8, 4, 256, 256, 3).unwrap();
    assert_eq!(ch.first_tail_mip, Some(2));
    assert_eq!(ch.slice_size, 4096 + 8192 + 32768);
    let g_levels = vec![Vec::new(), bc1_blocks(32, 32, 5), bc1_blocks(16, 16, 6)];
    let mut c5 = vec![0u8; 12288];
    for m in 1..3 {
        place(&mut c5, &ch, m, ch.levels[m].offset, &g_levels[m]);
    }
    let (g_stream, _) = bcpack::encode::synthetic_bc1(11, 4096, 512);

    let chunks = vec![
        chunk(0x20, &c0),
        chunk(0x00, &c1),
        TestChunk {
            codec: 0x04,
            stored: stream,
            vlen: 4096,
        },
        TestChunk {
            codec: 0x1C,
            stored: vec![0x5A; 300],
            vlen: 4096,
        },
        chunk(0x20, &c4),
        chunk(0x20, &c5),
        TestChunk {
            codec: 0x04,
            stored: g_stream,
            vlen: 32768,
        },
    ];
    let textures = vec![
        (0x10, desc(64, 64, 0, 71, 1, 2, TILE1, 0)),
        (0x20, desc(16, 16, 1, 28, 1, 2, TILE1, 1)),
        (0x30, desc(128, 128, 1, 72, 1, 2, 0, 2)),
        (0x40, desc(64, 64, 0, 71, 1, 2, TILE1, 18)),
        (0x50, desc(32, 32, 0, 98, 1, 2, TILE1, 19)),
        (0x60, desc(16, 16, 0, 28, 2, 10, TILE1, 20)),
        (0x70, desc(256, 256, 2, 71, 1, 2, TILE1, 22)),
    ];
    Fixture {
        bytes: build(&chunks, &textures),
        a_blocks,
        b_levels,
        c_levels,
        d_expected,
        f_slices,
        g_levels,
    }
}

fn opts(mips: MipSelection) -> DecodeOptions {
    DecodeOptions {
        mips,
        ..DecodeOptions::default()
    }
}

#[test]
fn parses_synthetic_rds5() {
    let fx = fixture();
    let s = TextureStore::parse(&fx.bytes).unwrap();
    assert_eq!(s.kind, StoreKind::Rds5);
    assert_eq!(s.index_layout, IndexLayout::Agreed);
    assert!(!s.provisional, "{:?}", s.notes);
    assert_eq!(s.chunks().len(), 7);
    assert_eq!(s.chunks()[2].codec, Codec::BcPack(1));
    assert_eq!(s.chunks()[1].virtual_offset, 8192);
    let t = s.textures();
    assert_eq!(t.len(), 7);
    assert_eq!((t[0].width, t[0].height, t[0].mip_count), (64, 64, 1));
    assert_eq!(t[0].format, DxgiFormat::BC1_UNORM);
    assert_eq!(t[0].swizzle, Some(Swizzle::Sw4KbS));
    assert_eq!(t[0].mips[0].codecs, vec![Codec::Zlib]);
    assert!(!t[0].provisional);
    assert_eq!(t[1].mip_count, 2);
    assert_eq!(t[2].swizzle, Some(Swizzle::Sw64KbD));
    assert!(t[2].provisional, "tile mode 0 under 64 KiB is disputed");
    assert_eq!(t[3].mips[0].codecs, vec![Codec::BcPack(1)]);
    assert_eq!(t[5].kind, ResourceKind::Texture2DArray);
    assert_eq!(t[5].array_size, 2);
    assert_eq!(t[6].mip_count, 3);
    assert_eq!(t[6].mips[0].codecs, vec![Codec::BcPack(1)]);
    assert_eq!(t[6].mips[1].codecs, vec![Codec::Zlib]);
    assert_eq!(t[6].first_plain_mip(), Some(1));
    assert_eq!(s.find(0x50), Some(4));
    assert_eq!(s.find(0x55), None);
}

#[test]
fn decodes_zlib_stored_tail_and_arrays() {
    let fx = fixture();
    let s = TextureStore::parse(&fx.bytes).unwrap();
    // A: zlib, single level.
    let raw = s
        .decode_raw(&fx.bytes, 0, &opts(MipSelection::All))
        .unwrap();
    assert_eq!(raw.mips[0], fx.a_blocks);
    assert!(!raw.provisional);
    let img = s.decode(&fx.bytes, 0, MipSelection::All).unwrap();
    assert_eq!(
        img.mips[0],
        decode_bcn(DxgiFormat::BC1_UNORM, 64, 64, &fx.a_blocks).unwrap()
    );
    assert!(!img.provisional);
    // B: both levels from the packed mip tail.
    let img = s.decode(&fx.bytes, 1, MipSelection::All).unwrap();
    assert_eq!(img.mips, fx.b_levels);
    let img = s.decode(&fx.bytes, 1, MipSelection::Level(1)).unwrap();
    assert_eq!((img.width, img.height), (8, 8));
    assert_eq!(img.mips, vec![fx.b_levels[1].clone()]);
    // C: stored chunk, SW_64KB_D, sRGB, provisional (disputed tile rule).
    let raw = s
        .decode_raw(&fx.bytes, 2, &opts(MipSelection::All))
        .unwrap();
    assert_eq!(raw.mips, fx.c_levels);
    assert!(raw.provisional);
    let img = s.decode(&fx.bytes, 2, MipSelection::All).unwrap();
    assert_eq!(img.format, ImageFormat::Rgba8Srgb);
    assert!(img.provisional);
    // F: array slices.
    for (i, sl) in fx.f_slices.iter().enumerate() {
        let o = DecodeOptions {
            slice: i as u32,
            ..opts(MipSelection::All)
        };
        assert_eq!(&s.decode_with(&fx.bytes, 5, &o).unwrap().mips[0], sl);
    }
    let o = DecodeOptions {
        slice: 2,
        ..opts(MipSelection::All)
    };
    assert!(matches!(
        s.decode_with(&fx.bytes, 5, &o),
        Err(Error::SliceOutOfRange { .. })
    ));
    assert!(matches!(
        s.decode(&fx.bytes, 1, MipSelection::Level(2)),
        Err(Error::MipOutOfRange { .. })
    ));
    assert!(matches!(
        s.decode(&fx.bytes, 99, MipSelection::All),
        Err(Error::IndexOutOfRange { .. })
    ));
}

#[test]
fn bcpack_is_provisional_and_unsupported_codecs_are_clear() {
    let fx = fixture();
    let s = TextureStore::parse(&fx.bytes).unwrap();
    let raw = s
        .decode_raw(&fx.bytes, 3, &opts(MipSelection::All))
        .unwrap();
    assert_eq!(raw.mips[0], fx.d_expected);
    assert!(raw.provisional);
    assert!(raw.notes.iter().any(|n| n.contains("BCPack")));
    let img = s.decode(&fx.bytes, 3, MipSelection::All).unwrap();
    assert!(img.provisional);
    // BCPack refused on request.
    let o = DecodeOptions {
        allow_bcpack: false,
        ..opts(MipSelection::All)
    };
    assert!(matches!(
        s.decode_with(&fx.bytes, 3, &o),
        Err(Error::Unsupported { .. })
    ));
    // BCPack BC7 is not decodable yet.
    match s.decode(&fx.bytes, 4, MipSelection::All) {
        Err(Error::Unsupported { codec, format }) => {
            assert!(codec.contains("BCPack BC7"), "{codec}");
            assert!(format.contains("BC7_UNORM"), "{format}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn prefer_plain_skips_bcpack_mips() {
    let fx = fixture();
    let s = TextureStore::parse(&fx.bytes).unwrap();
    let raw = s
        .decode_raw(&fx.bytes, 6, &opts(MipSelection::PreferPlain))
        .unwrap();
    assert_eq!(raw.first_mip, 1);
    assert_eq!((raw.width, raw.height), (128, 128));
    assert_eq!(raw.mips, fx.g_levels[1..].to_vec());
    assert!(!raw.provisional);
    let img = s.decode(&fx.bytes, 6, MipSelection::All).unwrap();
    assert_eq!(img.mips.len(), 3);
    assert!(img.provisional);
    let img = s.decode(&fx.bytes, 6, MipSelection::FromLevel(2)).unwrap();
    assert_eq!((img.width, img.mips.len()), (64, 1));
    assert!(!img.provisional);
}

#[test]
fn tile_rule_option_changes_small_mode0_textures() {
    let fx = fixture();
    let s = TextureStore::parse_with(
        &fx.bytes,
        ParseOptions {
            tile_rule: TileRule::Mip0Size,
        },
    )
    .unwrap();
    assert_eq!(s.textures()[2].swizzle, Some(Swizzle::Sw4KbS));
    assert!(s.textures()[2].provisional);
    // Data was written as SW_64KB_D, so the other reading gives different blocks.
    let raw = s.decode_raw(&fx.bytes, 2, &opts(MipSelection::Level(0)));
    assert!(raw.map(|r| r.mips[0] != fx.c_levels[0]).unwrap_or(true));
}

#[test]
fn detects_index_layout_when_bits_24_25_are_set() {
    let fx = fixture();
    let mut bytes = fx.bytes.clone();
    // Bit 24 of entry 0: +4 GiB under the 26-bit reading (past the file), a flag under 24 bits.
    bytes[0x18 + 3] |= 0x01;
    let s = TextureStore::parse(&bytes).unwrap();
    assert_eq!(s.index_layout, IndexLayout::NarrowOffset);
    assert!(s.provisional);
    assert!(s.textures()[0].provisional);
    let raw = s.decode_raw(&bytes, 0, &opts(MipSelection::All)).unwrap();
    assert_eq!(raw.mips[0], fx.a_blocks);
}

#[test]
fn finds_misplaced_etpf() {
    let fx = fixture();
    let mut bytes = fx.bytes.clone();
    bytes[0x10..0x14].copy_from_slice(&0u32.to_le_bytes());
    let s = TextureStore::parse(&bytes).unwrap();
    assert!(s.provisional);
    assert_eq!(s.textures().len(), 7);
}

#[test]
fn bare_etpf_and_mip_spacing_rule() {
    // 1024x1024 BC1 SW_4KB_S: field 0 says 1 mip, the next texture sits after 3 mips.
    let ch3 = mip_chain(Swizzle::Sw4KbS, 8, 4, 1024, 1024, 3).unwrap();
    assert_eq!(ch3.slice_size, 672 << 10);
    let next = (ch3.slice_size >> 12) as u32;
    let table = etpf(&[
        (1, desc(1024, 1024, 0, 71, 1, 2, 1, 0)),
        (2, desc(64, 64, 0, 71, 1, 2, 1, next)),
    ]);
    let s = TextureStore::parse(&table).unwrap();
    assert_eq!(s.kind, StoreKind::EtpfOnly);
    let t = &s.textures()[0];
    assert_eq!((t.mip_field, t.mip_count), (0, 3));
    assert!(t.provisional);
    assert!(t.notes.iter().any(|n| n.contains("corrected")));
    assert_eq!(s.textures()[1].mip_count, 1);
    assert!(!s.textures()[1].provisional);
    assert!(matches!(
        s.decode(&table, 0, MipSelection::All),
        Err(Error::NoData)
    ));
}

#[test]
fn cube_layout_matches_documented_examples() {
    // GTA6-Asset-Info §5: 512x512 BC1, 7 mips, tile mode 12 -> full copy at page 104.
    let c = mip_chain(Swizzle::Sw4KbS, 8, 4, 512, 512, 7).unwrap();
    assert_eq!(cube_full_copy_start(&c, 6, 4).unwrap(), 104 * 4096);
    // 128x128 BC7, tile mode 11 -> full copy at page 18.
    let c = mip_chain(Swizzle::Sw4KbS, 16, 4, 128, 128, 8).unwrap();
    assert_eq!(cube_full_copy_start(&c, 6, 3).unwrap(), 18 * 4096);
    assert!(cube_full_copy_start(&c, 6, 9).is_err());
    // Descriptor: field 4 + copies 4 - 1 = 7 mips.
    let s = TextureStore::parse(&etpf(&[(1, desc(512, 512, 4, 71, 6, 4, 12, 0))])).unwrap();
    let t = &s.textures()[0];
    assert_eq!(
        (t.kind, t.mip_count, t.array_size),
        (ResourceKind::Cube, 7, 6)
    );
    assert!(t.provisional);
}

#[test]
fn unsupported_layouts_and_formats_are_reported() {
    let s = TextureStore::parse(&etpf(&[
        (1, desc(32, 32, 0, 71, 4, 6, 1, 0)),  // 3D
        (2, desc(32, 32, 0, 126, 1, 2, 1, 1)), // unknown format
        (3, desc(32, 32, 0, 71, 1, 2, 7, 2)),  // unknown tile mode
    ]))
    .unwrap();
    for t in s.textures() {
        assert!(t.layout_error().is_some(), "{t:?}");
    }
}

#[test]
fn rejects_malformed_headers() {
    assert!(matches!(TextureStore::parse(&[]), Err(Error::NotAStore(_))));
    assert!(matches!(
        TextureStore::parse(b"XXXXXXXX"),
        Err(Error::NotAStore(_))
    ));
    assert!(matches!(
        TextureStore::parse(b"RDS5"),
        Err(Error::Truncated { .. })
    ));
    let mut h = b"RDS5".to_vec();
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h.resize(0x40, 0);
    assert!(matches!(
        TextureStore::parse(&h),
        Err(Error::Malformed { .. })
    ));
    // Valid table, no ETPF anywhere.
    let mut h = b"RDS5".to_vec();
    h.extend_from_slice(&1u32.to_le_bytes());
    h.resize(0x200, 0);
    assert!(matches!(
        TextureStore::parse(&h),
        Err(Error::Malformed { .. })
    ));
    // ETPF claiming far more descriptors than present.
    let mut t = etpf(&[(1, desc(4, 4, 0, 71, 1, 2, 1, 0))]);
    t[8..12].copy_from_slice(&0x1000_0000u32.to_le_bytes());
    assert!(matches!(
        TextureStore::parse(&t),
        Err(Error::Malformed { .. })
    ));
    assert!(TextureStore::parse(b"ETPF\x7a\x02").is_err());
}

#[test]
fn truncated_and_corrupt_chunks_fail_cleanly() {
    let fx = fixture();
    let s = TextureStore::parse(&fx.bytes).unwrap();
    // Truncated file: the last chunks are gone.
    let cut = &fx.bytes[..s.chunks()[4].file_offset as usize + 10];
    let e = s.decode(cut, 6, MipSelection::All).unwrap_err();
    assert!(matches!(e, Error::Chunk { .. }), "{e}");
    assert!(e.to_string().contains("file offset"));
    // Corrupt zlib payload.
    let mut bad = fx.bytes.clone();
    let o = s.chunks()[0].file_offset as usize;
    for b in &mut bad[o + 2..o + 40] {
        *b ^= 0xFF;
    }
    assert!(s.decode(&bad, 0, MipSelection::All).is_err());
    // Corrupt BCPack stream.
    let mut bad = fx.bytes.clone();
    let o = s.chunks()[2].file_offset as usize;
    for b in &mut bad[o..o + 64] {
        *b = 0xFF;
    }
    assert!(s.decode(&bad, 3, MipSelection::All).is_err());
}

#[test]
fn mutated_stores_never_panic() {
    let fx = fixture();
    let base = fx.bytes;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let hot = 0x300; // header, chunk table and ETPF
    for _ in 0..400 {
        let mut b = base.clone();
        for _ in 0..1 + rnd() % 6 {
            let r = rnd();
            let i = if r & 1 == 0 {
                (r >> 8) as usize % hot
            } else {
                (r >> 8) as usize % b.len()
            };
            b[i] ^= (r >> 40) as u8 | 1;
        }
        if rnd() % 8 == 0 {
            let l = rnd() as usize % b.len();
            b.truncate(l);
        }
        if let Ok(s) = TextureStore::parse(&b) {
            for i in 0..s.textures().len().min(8) {
                for sel in [
                    MipSelection::All,
                    MipSelection::PreferPlain,
                    MipSelection::Level(0),
                ] {
                    let _ = s.decode(&b, i, sel);
                }
            }
        }
    }
}
