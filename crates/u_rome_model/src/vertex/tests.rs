use super::*;
use crate::testutil::{BlobBuilder, lcg};

const BASE: [f32; 3] = [-10.0, -5.0, -2.0];
const STEP: [f32; 3] = [20.0 / 16_777_215.0, 10.0 / 16_777_215.0, 4.0 / 65_535.0];

fn quantize(p: [f32; 3]) -> u64 {
    let q = |i: usize, max: f32| ((p[i] - BASE[i]) / STEP[i]).round().clamp(0.0, max) as u32;
    quantize_position_word(
        q(0, 16_777_215.0),
        q(1, 16_777_215.0),
        q(2, 65_535.0) as u16,
    )
}

fn sample_positions() -> Vec<[f32; 3]> {
    vec![
        [-10.0, -5.0, -2.0],
        [10.0, 5.0, 2.0],
        [0.123_456, -3.210_987, 1.5],
        [7.777, 0.000_1, -1.999],
    ]
}

fn oct_encode(n: [f32; 3]) -> u32 {
    let n = normalize(n);
    let s = n[0].abs() + n[1].abs() + n[2].abs();
    let (mut x, mut y) = (n[0] / s, n[1] / s);
    if n[2] < 0.0 {
        let (ox, oy) = (x, y);
        x = (1.0 - oy.abs()) * sign_nonzero(ox);
        y = (1.0 - ox.abs()) * sign_nonzero(oy);
    }
    let q = |v: f32| (((v + 1.0) / 2.0) * 1023.0).round().clamp(0.0, 1023.0) as u32;
    q(x) | (q(y) << 10) | (0xABC << 20) // junk in the unknown top bits
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn base_blob() -> BlobBuilder {
    let words: Vec<u64> = sample_positions().into_iter().map(quantize).collect();
    BlobBuilder::quantized(BASE, STEP, 2, &words)
}

#[test]
fn bit_layout_matches_published_formula() {
    let q = quantize_position_word(0x12_3456, 0xAB_CDEF, 0x7FFF);
    assert_eq!(q & 0xFF, 0x56);
    assert_eq!((q >> 8) & 0xFF, 0xEF);
    assert_eq!((q >> 16) & 0xFFFF, 0x1234);
    assert_eq!((q >> 32) & 0xFFFF, 0xABCD);
    assert_eq!(q >> 48, 0x7FFF);
    let p = dequantize_position(q, [0.0; 3], [1.0; 3]);
    assert_eq!(p, [0x12_3456 as f32, 0xAB_CDEF as f32, 0x7FFF as f32]);
}

#[test]
fn quantized_positions_within_step() {
    let blob = base_blob().stream(1, format::RGBA8, vec![0; 16]).build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert_eq!(vd.vertex_count, 4);
    assert!(!vd.provisional, "{:?}", vd.warnings);
    for (got, want) in vd.positions.iter().zip(sample_positions()) {
        for i in 0..3 {
            assert!(
                (got[i] - want[i]).abs() <= STEP[i] * 0.51 + 1e-6,
                "{got:?} vs {want:?}"
            );
        }
    }
    // x and y really use 24 bits: the error is far below a 16-bit step.
    let p = vd.positions[2];
    assert!((p[0] - 0.123_456).abs() < 20.0 / 65_535.0 / 16.0);
}

#[test]
fn vertex_count_from_smallest_following_stream() {
    // Streams listed out of slot order; padding after each stream.
    let mut b = base_blob().stream(11, format::RGB10A2, vec![0; 16]).stream(
        2,
        format::OCT_NORMAL,
        vec![0; 16],
    );
    b.padding = 4;
    let vd = decode_vertex_buffer(&b.build()).unwrap();
    assert_eq!(vd.vertex_count, 4);
    assert_eq!(vd.normals.len(), 4);
    assert_eq!(vd.tangents.len(), 4);
}

#[test]
fn axis_modes() {
    // A point whose slot-space coordinates are (s0, s1, s2).
    let slot = [1.0f32, 2.0, 0.5];
    let word = quantize(slot);
    let slot_dq = dequantize_position(word, BASE, STEP);
    for (mode, model) in [
        (2u32, [slot_dq[0], slot_dq[1], slot_dq[2]]),
        // mode 1: slots hold (z, x, y)
        (1, [slot_dq[1], slot_dq[2], slot_dq[0]]),
        // mode 0: slots hold (z, y, x)
        (0, [slot_dq[2], slot_dq[1], slot_dq[0]]),
    ] {
        let blob = BlobBuilder::quantized(BASE, STEP, mode, &[word])
            .stream(1, format::RGBA8, vec![0; 4])
            .build();
        let vd = decode_vertex_buffer(&blob).unwrap();
        assert_eq!(vd.positions[0], model, "mode {mode}");
        assert!(!vd.provisional);
    }
    // Unknown mode: slot order kept, flagged.
    let blob = BlobBuilder::quantized(BASE, STEP, 7, &[word])
        .stream(1, format::RGBA8, vec![0; 4])
        .build();
    let mut vd = decode_vertex_buffer(&blob).unwrap();
    assert!(vd.provisional);
    assert_eq!(vd.axis_order, None);
    assert_eq!(vd.positions[0], slot_dq);
    vd.apply_axis_order([2, 0, 1]);
    assert_eq!(vd.positions[0], [slot_dq[1], slot_dq[2], slot_dq[0]]);
}

#[test]
fn float_positions_variant() {
    let pts = vec![[1.0, 2.0, 3.0], [-4.5, 0.25, 1e3]];
    let normals: Vec<u8> = [[0.0f32, 0.0, 2.0], [1.0, 0.0, 0.0]]
        .iter()
        .flat_map(|n| n.iter().flat_map(|c| c.to_le_bytes()))
        .collect();
    let blob = BlobBuilder::float(&pts)
        .stream(2, format::F32X3, normals)
        .build();
    assert_eq!(u32::from_le_bytes(blob[0..4].try_into().unwrap()), 0x3063);
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert_eq!(vd.position_encoding, Some(PositionEncoding::Float));
    assert_eq!(vd.positions, pts);
    assert_eq!(vd.normals, vec![[0.0, 0.0, 1.0], [1.0, 0.0, 0.0]]);
}

#[test]
fn octahedral_normals() {
    let mut seed = 3;
    let mut normals = Vec::new();
    for _ in 0..4 {
        let r = |s: &mut u64| lcg(s) as f32 / u32::MAX as f32 * 2.0 - 1.0;
        normals.push(normalize([r(&mut seed), r(&mut seed), r(&mut seed)]));
    }
    // Include both hemispheres explicitly.
    normals[0] = [0.0, 0.0, -1.0];
    normals[1] = normalize([0.3, -0.4, -0.8]);
    let bytes: Vec<u8> = normals
        .iter()
        .flat_map(|n| oct_encode(*n).to_le_bytes())
        .collect();
    let vd =
        decode_vertex_buffer(&base_blob().stream(2, format::OCT_NORMAL, bytes).build()).unwrap();
    for (got, want) in vd.normals.iter().zip(&normals) {
        assert!(dot(*got, *want) > 0.999, "{got:?} vs {want:?}");
        assert!((dot(*got, *got) - 1.0).abs() < 1e-5);
    }
}

#[test]
fn colours_uvs_and_tangents() {
    let colours: Vec<u8> = (0..16).collect();
    let uv_s16: Vec<u8> = [(8192i16, -8192i16), (4096, 0), (-16384, 32767), (1, -1)]
        .iter()
        .flat_map(|(u, v)| [u.to_le_bytes(), v.to_le_bytes()].concat())
        .collect();
    let uv_f32: Vec<u8> = [[0.5f32, 0.25], [1.0, -2.0], [3.0, 4.0], [0.0, 0.0]]
        .iter()
        .flat_map(|uv| uv.iter().flat_map(|c| c.to_le_bytes()))
        .collect();
    let tq = |v: f32| (((v + 1.0) / 2.0) * 1023.0).round() as u32;
    let tangent = |t: [f32; 3], neg: bool| {
        tq(t[0]) | tq(t[1]) << 10 | tq(t[2]) << 20 | if neg { 3 << 30 } else { 0 }
    };
    let tangents: Vec<u8> = [
        tangent([1.0, 0.0, 0.0], false),
        tangent([0.0, 1.0, 0.0], true),
        tangent([0.0, 0.0, -1.0], false),
        tangent([0.6, 0.8, 0.0], true),
    ]
    .iter()
    .flat_map(|w| w.to_le_bytes())
    .collect();
    let blob = base_blob()
        .stream(1, format::RGBA8, colours)
        .stream(3, format::S16X2, uv_s16.clone())
        .stream(4, format::EIGHT_BYTE, uv_f32)
        .stream(14, format::S16X2, uv_s16.clone())
        .stream(15, format::S16X2, uv_s16)
        .stream(11, format::RGB10A2, tangents)
        .stream(12, format::RGBA8, vec![9; 16])
        .stream(17, format::EIGHT_BYTE, vec![0; 32])
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert_eq!(vd.colors[1], [4, 5, 6, 7]);
    assert_eq!(vd.colors_slot12[3], [9; 4]);
    assert_eq!(vd.uv_sets[0][0], [1.0, -1.0]);
    assert_eq!(vd.uv_sets[0][1], [0.5, 0.0]);
    assert_eq!(vd.uv_sets[0][2], [-2.0, 32767.0 / 8192.0]);
    assert_eq!(vd.uv_sets[1][1], [1.0, -2.0]);
    assert_eq!(vd.uv_sets[2], vd.uv_sets[0]);
    assert_eq!(vd.uv_sets[3], vd.uv_sets[0]);
    let want = [
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, -1.0],
        [0.0, 0.0, -1.0, 1.0],
        [0.6, 0.8, 0.0, -1.0],
    ];
    for (g, w) in vd.tangents.iter().zip(want) {
        for i in 0..4 {
            assert!((g[i] - w[i]).abs() < 3e-3, "{g:?} vs {w:?}");
        }
    }
    // Slot 17 is unknown: listed but not decoded.
    let s17 = vd.streams.iter().find(|s| s.slot == 17).unwrap();
    assert!(!s17.decoded);
}

#[test]
fn skin_single_joint_u8_and_u16() {
    let blob = base_blob()
        .stream(8, format::JOINT_SINGLE, vec![3, 7, 255, 0])
        .stream(1, format::RGBA8, vec![0; 16])
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert_eq!(vd.joint_format, Some(JointFormat::SingleU8));
    let (j, w) = vd.skin();
    assert_eq!(j[2], [255, 0, 0, 0]);
    assert_eq!(w[2], [1.0, 0.0, 0.0, 0.0]);

    let u16s: Vec<u8> = [3u16, 290, 383, 0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let blob = base_blob()
        .stream(8, format::JOINT_SINGLE, u16s)
        .stream(1, format::RGBA8, vec![0; 16])
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert_eq!(vd.joint_format, Some(JointFormat::SingleU16));
    assert_eq!(vd.skin().0[1], [290, 0, 0, 0]);
}

#[test]
fn skin_u8x4_and_u16x4_with_weights() {
    let weights = vec![255, 0, 0, 0, 128, 127, 0, 0, 85, 85, 85, 0, 0, 0, 0, 0];
    let blob = base_blob()
        .stream(6, format::RGBA8, weights.clone())
        .stream(8, format::U8X4, (0..16).collect())
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    let (j, w) = vd.skin();
    assert_eq!(j[1], [4, 5, 0, 0]);
    assert!((w[1][0] - 128.0 / 255.0).abs() < 1e-6);
    assert!((w[1].iter().sum::<f32>() - 1.0).abs() < 1e-6);
    assert_eq!(w[3], [1.0, 0.0, 0.0, 0.0]); // zero weights become rigid

    let joints: Vec<u8> = (0u16..16)
        .map(|i| i * 100)
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let blob = base_blob()
        .stream(6, format::RGBA8, weights)
        .stream(8, format::U16X4, joints)
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert_eq!(vd.joint_format, Some(JointFormat::U16x4));
    assert_eq!(vd.skin().0[2], [800, 900, 1000, 0]);
}

#[test]
fn ped_eight_influences_keep_heaviest_four() {
    let w1 = vec![10, 20, 30, 40, 0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0];
    let w2 = vec![50, 60, 5, 40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let blob = base_blob()
        .stream(6, format::RGBA8, w1)
        .stream(7, format::RGBA8, w2)
        .stream(
            8,
            format::U8X4,
            vec![1, 2, 3, 4, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .stream(
            9,
            format::U8X4,
            vec![5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    let (j, w) = vd.skin();
    assert_eq!(j[0], [6, 5, 4, 8]);
    let sum = 60.0 + 50.0 + 40.0 + 40.0;
    assert!((w[0][0] - 60.0 / sum).abs() < 1e-6);
    assert_eq!(j[1], [9, 0, 0, 0]);
    assert_eq!(w[1], [1.0, 0.0, 0.0, 0.0]);
}

#[test]
fn head_skin_is_provisional() {
    let blob = base_blob()
        .stream(8, format::HEAD_SKIN, (0..64).collect())
        .stream(6, format::RGBA8, [255, 0, 0, 0].repeat(4))
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert!(vd.provisional);
    assert_eq!(vd.joint_format, Some(JointFormat::HeadSkin));
    assert_eq!(vd.joints[0], [0x0100, 0x0504, 0x0908, 0x0D0C]);
}

#[test]
fn bad_streams_are_skipped_with_warnings() {
    // Unknown format on a known slot, and a stream too short for V.
    let blob = base_blob()
        .stream(2, 0x99, vec![0; 16])
        .stream(3, format::S16X2, vec![0; 8])
        .build();
    let vd = decode_vertex_buffer(&blob).unwrap();
    assert!(vd.provisional);
    assert!(vd.normals.is_empty());
    assert!(vd.uv_sets[0].is_empty());
    assert_eq!(vd.warnings.len(), 2, "{:?}", vd.warnings);
}

#[test]
fn malformed_inputs() {
    assert!(matches!(
        decode_vertex_buffer(&[]),
        Err(ModelError::Bytes { .. })
    ));
    assert!(matches!(
        decode_vertex_buffer(&[1, 2, 3, 4, 5, 6, 7, 8]),
        Err(ModelError::BadVertexMagic { found: 0x0403_0201 })
    ));
    // Header truncated.
    let good = base_blob().stream(1, format::RGBA8, vec![0; 16]).build();
    for cut in 0..0x80 {
        assert!(decode_vertex_buffer(&good[..cut]).is_err(), "cut {cut}");
    }
    // Truncation inside the streams never panics.
    for cut in 0x80..good.len() {
        let _ = decode_vertex_buffer(&good[..cut]);
    }
    // Non-finite step.
    let mut bad = good.clone();
    bad[0x70..0x74].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(decode_vertex_buffer(&bad).is_err());
    // A slot pointing past the end is ignored with a warning.
    let mut bad = good.clone();
    bad[8..12].copy_from_slice(&0xFFFF_FF21u32.to_le_bytes());
    let vd = decode_vertex_buffer(&bad).unwrap();
    assert!(vd.provisional);
}

#[test]
fn random_bytes_never_panic() {
    let mut seed = 1234;
    for _ in 0..3000 {
        let len = 0x80 + (lcg(&mut seed) % 256) as usize;
        let mut b: Vec<u8> = (0..len).map(|_| lcg(&mut seed) as u8).collect();
        b[0..4].copy_from_slice(&VERTEX_MAGIC.to_le_bytes());
        // Point some slots at plausible offsets with random formats.
        for slot in 1..STREAM_SLOTS {
            if lcg(&mut seed).is_multiple_of(3) {
                let off = (0x80 + lcg(&mut seed) as usize % (len - 0x7F)) / 2;
                let v = ((off as u32) << 8) | (lcg(&mut seed) & 0xFF);
                b[slot * 4..slot * 4 + 4].copy_from_slice(&v.to_le_bytes());
            } else {
                b[slot * 4..slot * 4 + 4].fill(0);
            }
        }
        if let Ok(vd) = decode_vertex_buffer(&b) {
            let _ = vd.skin();
        }
    }
}
