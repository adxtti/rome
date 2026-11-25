//! AWC bank parsing, packet extraction, Ogg export and decoding on synthetic banks.

mod common;

use std::io::Cursor;

use common::{Channel, MusicBank, assemble, opus_fixture, packet, sfx_pcm_bank};
use u_rome_audio::{
    AwcBank, AwcError, Codec, DEFAULT_PRE_SKIP, StreamKind, decode_opus, decode_pcm, event_name,
    write_wav,
};
use u_rome_core::{Hash32, joaat};

const BEAT: u32 = 0xE89A_E78C;
const TRACKID: u32 = 0xA6D9_3246;

fn three_channel_bank() -> (MusicBank, common::Built) {
    let stereo = |id: u32, seed: u32| Channel {
        id,
        packets: (0..40).map(|i| packet(0xFC, 320, seed + i)).collect(),
        events: Vec::new(),
    };
    let mut ch1 = stereo(0x9D66_FE4C, 1000);
    ch1.events = vec![[TRACKID, 7.0f32.to_bits(), 0, 0]];
    let mono = Channel {
        id: 0xE032_C57E, // top bits set: table id is the low 29 bits
        packets: (0..25).map(|i| packet(0xF8, 200, 5000 + i)).collect(),
        events: vec![[BEAT, 1.5f32.to_bits(), 96_000, 0]],
    };
    let spec = MusicBank {
        channels: vec![stereo(0x7A38_37EF, 0), ch1, mono],
        music_events: vec![[BEAT, 0.0f32.to_bits(), 48_000, 0]],
        ..MusicBank::default()
    };
    let built = spec.build();
    (spec, built)
}

#[test]
fn multi_channel_multi_block() {
    let (spec, built) = three_channel_bank();
    assert!(built.block_count >= 3, "test needs several blocks");
    let bank = AwcBank::parse(built.bytes.clone()).unwrap();
    assert!(!bank.provisional(), "{:?}", bank.notes());
    assert_eq!(bank.version(), 1);
    assert_eq!(bank.entries().len(), 4);

    let streams = bank.streams();
    assert_eq!(streams.len(), 3);
    for (i, (s, c)) in streams.iter().zip(&spec.channels).enumerate() {
        assert_eq!(s.id, c.id);
        assert_eq!(s.codec, Codec::Opus);
        assert_eq!(s.codec_id, 0x0B);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.headroom, -100);
        assert_eq!(
            s.kind,
            StreamKind::Music {
                entry: 0,
                channel: i
            }
        );
        assert!(!s.provisional, "stream {i}: {:?}", s.notes);
        assert_eq!(s.sample_count, c.packets.len() as u64 * 960);
        assert_eq!(bank.opus_packets(i).unwrap(), c.packets, "stream {i}");
    }
    assert_eq!(streams[0].channels, 2);
    assert_eq!(streams[2].channels, 1);
    assert_eq!(streams[0].raw_sample_count, 40 * 960 * 2);
    assert_eq!(streams[2].raw_sample_count, 25 * 960);
    assert!((streams[0].duration_secs() - 0.8).abs() < 1e-9);

    // Events: music entry events apply to all channels, plus the channel's own.
    assert_eq!(streams[0].events.len(), 1);
    assert_eq!(streams[0].events[0].name, Some("beat"));
    assert_eq!(streams[0].events[0].time, 48_000);
    assert!((streams[0].events[0].time_secs() - 1.0).abs() < 1e-9);
    assert_eq!(streams[1].events.len(), 2);
    assert_eq!(streams[1].events[1].kind, Hash32(TRACKID));
    assert_eq!(streams[1].events[1].name, Some("trackid"));
    assert_eq!(streams[1].events[1].value, 7.0);
    assert_eq!(streams[2].events.len(), 2);
    assert_eq!(streams[2].events[1].value, 1.5);
    assert!(streams[2].events[1].provisional);

    // Blocks and seek tables.
    let blocks = bank.blocks(0).unwrap();
    assert_eq!(blocks.len(), built.block_count);
    for (b, block) in blocks.iter().enumerate() {
        assert_eq!(block.index, b);
        for (ch, bc) in block.channels.iter().enumerate() {
            assert_eq!(bc.units, built.units[b][ch], "block {b} ch {ch}");
            assert_eq!(bc.seek, built.seek[b][ch], "block {b} ch {ch}");
            assert_eq!(bc.start_entry, -1);
            assert_eq!(bc.skip_samples, 0);
        }
        if b + 1 < blocks.len() {
            assert_eq!(block.len, built.block_size);
        }
    }
    assert!(
        blocks.last().unwrap().len < built.block_size,
        "short last block"
    );
    assert_eq!(bank.block_sample_table(1).unwrap(), &built.block_table[..]);
    assert_eq!(bank.blocks(2).unwrap(), blocks);
}

#[test]
fn table_without_chunk_indices() {
    let (mut spec, _) = three_channel_bank();
    spec.with_indices = false;
    spec.flags = 0xFF00;
    let bank = AwcBank::parse(spec.build().bytes).unwrap();
    assert!(!bank.provisional(), "{:?}", bank.notes());
    assert_eq!(bank.streams().len(), 3);
    assert_eq!(bank.opus_packets(1).unwrap(), spec.channels[1].packets);
}

#[test]
fn flag_disagreeing_with_layout_falls_back() {
    // Flag bit 0 says "indices present" but the table has none.
    let (mut spec, _) = three_channel_bank();
    spec.with_indices = false;
    spec.flags = 0xFF01;
    let bank = AwcBank::parse(spec.build().bytes).unwrap();
    assert!(bank.provisional());
    assert!(bank.notes()[0].contains("chunk indices absent"));
    assert!(bank.streams().iter().all(|s| s.provisional));
    assert_eq!(bank.opus_packets(0).unwrap(), spec.channels[0].packets);
}

#[test]
fn corrupt_stream_table_uses_scan_fallback() {
    let (_, built) = three_channel_bank();
    let mut bytes = built.bytes;
    // Stream info of entry 1 (after 4 u16 indices and entry 0): claim 7 chunks.
    let at = 0x10 + 8 + 4;
    let v = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    bytes[at..at + 4].copy_from_slice(&(v | 0xE000_0000).to_le_bytes());
    let bank = AwcBank::parse(bytes).unwrap();
    assert!(bank.provisional());
    assert!(
        bank.notes().iter().any(|n| n.contains("scanning")),
        "{:?}",
        bank.notes()
    );
    assert_eq!(bank.streams().len(), 3);
    assert_eq!(bank.opus_packets(2).unwrap().len(), 25);
}

#[test]
fn ogg_export_from_bank() {
    let (mut spec, _) = three_channel_bank();
    // Declare 30 packets' worth of audio (interleaved): the Ogg end is trimmed.
    spec.raw_samples = Some(30 * 960 * 2);
    let bank = AwcBank::parse(spec.build().bytes).unwrap();
    let ogg = bank.ogg_opus(0).unwrap();
    let mut r = ogg::PacketReader::new(Cursor::new(ogg));
    let mut packets = Vec::new();
    let mut last_granule = 0;
    while let Some(p) = r.read_packet().unwrap() {
        last_granule = p.absgp_page();
        packets.push(p.data);
    }
    assert_eq!(&packets[0][..8], b"OpusHead");
    assert_eq!(packets[0][9], 2);
    let tags = &packets[1];
    let needle = b"AWC_STREAM_ID=0x7A3837EF";
    assert!(tags.windows(needle.len()).any(|w| w == needle));
    assert_eq!(&packets[2..], &spec.channels[0].packets[..]);
    assert_eq!(last_granule, 30 * 960 + u64::from(DEFAULT_PRE_SKIP));
}

/// Zero-crossing frequency estimate of one channel.
fn frequency(samples: &[i16], channels: usize, ch: usize, rate: f64) -> f64 {
    let s: Vec<i16> = samples.iter().skip(ch).step_by(channels).copied().collect();
    let crossings = s.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
    crossings as f64 / 2.0 / (s.len() as f64 / rate)
}

#[test]
fn decode_real_opus_end_to_end() {
    let packets = opus_fixture::packets();
    let spec = MusicBank {
        channels: vec![Channel {
            id: joaat("test_sine").0,
            packets: packets.clone(),
            events: Vec::new(),
        }],
        units_per_block: 1,
        ..MusicBank::default()
    };
    let bank = AwcBank::parse(spec.build().bytes).unwrap();
    assert_eq!(bank.streams()[0].channels, 2);
    assert_eq!(bank.streams()[0].sample_count, 5 * 960);

    let pcm = decode_pcm(&bank, 0).unwrap();
    assert_eq!(pcm.channels, 2);
    assert_eq!(pcm.sample_rate, 48_000);
    assert_eq!(pcm.frames(), 5 * 960 - usize::from(DEFAULT_PRE_SKIP));
    let peak = pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(peak > 2000, "decoded audio should be loud, peak {peak}");
    let left = frequency(&pcm.samples, 2, 0, 48_000.0);
    let right = frequency(&pcm.samples, 2, 1, 48_000.0);
    assert!((left - 440.0).abs() < 25.0, "left {left} Hz");
    assert!((right - 660.0).abs() < 25.0, "right {right} Hz");

    // Encoder pre-skip of this fixture is 120.
    let direct = decode_opus(&packets, 2, opus_fixture::PRE_SKIP).unwrap();
    assert_eq!(direct.frames(), 5 * 960 - 120);

    let wav = write_wav(&pcm);
    assert_eq!(wav.len(), 44 + pcm.samples.len() * 2);
    assert_eq!(&wav[44..46], &pcm.samples[0].to_le_bytes());
}

#[test]
fn decode_silence_frames() {
    // TOC 0xFC (CELT FB 20 ms stereo) + 0xFF 0xFE: the common Opus silence frame.
    let silence = vec![vec![0xFCu8, 0xFF, 0xFE]; 3];
    let pcm = decode_opus(&silence, 2, 0).unwrap();
    assert_eq!(pcm.frames(), 3 * 960);
    assert!(pcm.samples.iter().all(|&s| s == 0));
}

#[test]
fn undecodable_packets_fail_cleanly() {
    // Valid TOC, garbage payload: decoder may or may not reject; either way no panic.
    let junk: Vec<Vec<u8>> = (0..20).map(|i| packet(0xFC, 320, i)).collect();
    match decode_opus(&junk, 2, 312) {
        Ok(pcm) => assert_eq!(pcm.frames(), 20 * 960 - 312),
        Err(e) => assert!(matches!(e, AwcError::Decode { .. })),
    }
    // Code 1 packets with an even payload are malformed per RFC 6716 3.2.3.
    let bad = vec![vec![0xFDu8, 1, 2]; 4];
    match decode_opus(&bad, 2, 0) {
        Ok(pcm) => assert_eq!(pcm.frames(), 4 * 1920),
        Err(e) => assert!(matches!(e, AwcError::Decode { .. }), "{e}"),
    }
}

#[test]
fn sfx_pcm_stream() {
    let samples: Vec<i16> = (0..1000).map(|i| ((i * 37) % 2000 - 1000) as i16).collect();
    let bank = AwcBank::parse(sfx_pcm_bank(0x0123_4567, &samples, 22_050, 0x01)).unwrap();
    let s = &bank.streams()[0];
    assert_eq!(s.id, 0x0123_4567);
    assert_eq!(s.codec, Codec::Pcm);
    assert_eq!(s.sample_rate, 22_050);
    assert_eq!(s.sample_count, 1000);
    assert_eq!(s.headroom, -50);
    assert_eq!(s.kind, StreamKind::Sfx { entry: 0 });
    assert!(s.provisional);
    let pcm = decode_pcm(&bank, 0).unwrap();
    assert_eq!(pcm.samples, samples);
    assert_eq!(pcm.channels, 1);
    assert!(matches!(
        bank.opus_packets(0),
        Err(AwcError::UnsupportedCodec { .. })
    ));
    assert!(matches!(
        bank.blocks(0),
        Err(AwcError::UnsupportedLayout { .. })
    ));
}

#[test]
fn unsupported_codecs_reported() {
    let (mut spec, _) = three_channel_bank();
    spec.codec = 0x05;
    let bank = AwcBank::parse(spec.build().bytes).unwrap();
    let s = &bank.streams()[0];
    assert_eq!(s.codec, Codec::Xma2);
    assert!(s.provisional);
    let err = bank.opus_packets(0).unwrap_err();
    assert!(matches!(
        err,
        AwcError::UnsupportedCodec {
            codec: Codec::Xma2,
            ..
        }
    ));
    assert!(err.to_string().contains("XMA2"));
    assert!(matches!(
        decode_pcm(&bank, 0),
        Err(AwcError::UnsupportedCodec { .. })
    ));

    let adpcm = AwcBank::parse(sfx_pcm_bank(1, &[0; 8], 32_000, 0x04)).unwrap();
    assert_eq!(adpcm.streams()[0].codec, Codec::ImaAdpcm);
    assert!(decode_pcm(&adpcm, 0).is_err());
    assert!(matches!(
        adpcm.opus_packets(5),
        Err(AwcError::StreamIndex { index: 5, count: 1 })
    ));
}

#[test]
fn encrypted_flag() {
    let (mut spec, _) = three_channel_bank();
    spec.flags = 0xFF09;
    let bank = AwcBank::parse(spec.build().bytes).unwrap();
    assert!(bank.is_encrypted());
    assert!(bank.streams()[0].provisional);
    assert!(matches!(bank.opus_packets(0), Err(AwcError::Encrypted)));
}

#[test]
fn event_names_match_format_doc() {
    // Hash values from FORMAT.md section 7.
    let table = [
        (0xE89A_E78C, "beat"),
        (0xA6D9_3246, "trackid"),
        (0x08DB_A0F8, "dj"),
        (0xDA8F_5F53, "intro"),
        (0x5D40_B28D, "outro"),
        (0x0C80_1D6C, "low"),
        (0x3A42_3FE8, "high"),
    ];
    for (hash, name) in table {
        assert_eq!(event_name(Hash32(hash)), Some(name));
    }
    assert_eq!(event_name(Hash32(0x1234_5678)), None);
}

#[test]
fn malformed_headers() {
    let err = |b: Vec<u8>| AwcBank::parse(b).unwrap_err();
    assert!(matches!(
        err(b"RIFF....".to_vec()),
        AwcError::BadMagic { .. }
    ));
    assert!(matches!(err(b"TADA".to_vec()), AwcError::BigEndian));
    assert!(matches!(err(b"AD".to_vec()), AwcError::Read { .. }));
    assert!(matches!(
        err(b"ADAT\x01\x00".to_vec()),
        AwcError::Read { .. }
    ));

    let header = |version: u16, count: u32, size: u32| {
        let mut b = b"ADAT".to_vec();
        b.extend_from_slice(&version.to_le_bytes());
        b.extend_from_slice(&0xFF01u16.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.resize(64, 0);
        b
    };
    assert!(matches!(
        err(header(7, 1, 0x20)),
        AwcError::UnsupportedVersion { version: 7, .. }
    ));
    assert!(matches!(
        err(header(1, 0, 0x20)),
        AwcError::InvalidHeader { offset: 8, .. }
    ));
    assert!(matches!(
        err(header(1, 1_000_000, 0x20)),
        AwcError::Limit { .. }
    ));
    assert!(matches!(
        err(header(1, 1, 0x1000)),
        AwcError::InvalidHeader { offset: 12, .. }
    ));
    // Valid table with no stream headers at all.
    let e = err(assemble(&[(1, vec![(0x55, vec![0; 16])])], 0xFF01, true));
    assert!(e.to_string().contains("no stream has"), "{e}");
}

#[test]
fn malformed_chunks() {
    // Music header claims 99 channels.
    let mut fmt = Vec::new();
    for v in [1u32, 0x80000, 99] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    let bytes = assemble(
        &[(0, vec![(0x48, fmt), (0x55, vec![0; 0x800])])],
        0xFF01,
        true,
    );
    assert!(matches!(
        AwcBank::parse(bytes).unwrap_err(),
        AwcError::Limit {
            what: "music channel count",
            value: 99,
            ..
        }
    ));

    // Truncated music header chunk: entry for channel 0 missing.
    let mut fmt = Vec::new();
    for v in [1u32, 0x80000, 1] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    let bytes = assemble(
        &[(0, vec![(0x48, fmt), (0x55, vec![0; 0x800])])],
        0xFF01,
        true,
    );
    let e = AwcBank::parse(bytes).unwrap_err();
    assert!(matches!(e, AwcError::Read { .. }));
    assert!(e.to_string().contains("channel 0 stream id"), "{e}");

    // Data chunk pointing past the end of the file.
    let (_, built) = three_channel_bank();
    let mut bytes = built.bytes;
    let len = bytes.len();
    bytes.truncate(len / 2);
    let bank = AwcBank::parse(bytes).unwrap();
    assert!(bank.provisional());
    assert!(bank.notes().iter().any(|n| n.contains("truncated")));
    // Earlier blocks still extract; later ones are dropped with a warning.
    let n = bank.opus_packets(0).map(|p| p.len()).unwrap_or(0);
    assert!(n < 40);
}

#[test]
fn bad_block_header() {
    let (_, built) = three_channel_bank();
    let bank = AwcBank::parse(built.bytes.clone()).unwrap();
    let data_off = bank.blocks(0).unwrap()[0].offset;

    // Block 0, channel 0 unit count -> 0x7FFFFFFF.
    let mut bytes = built.bytes.clone();
    bytes[data_off + 4..data_off + 8].copy_from_slice(&i32::MAX.to_le_bytes());
    let bank = AwcBank::parse(bytes).unwrap();
    let e = bank.opus_packets(0).unwrap_err();
    assert!(matches!(e, AwcError::Block { block: 0, .. }), "{e}");
    assert!(bank.streams()[0].provisional);

    // Block 1 corrupt: block 0 packets survive.
    let mut bytes = built.bytes.clone();
    let b1 = data_off + built.block_size;
    bytes[b1 + 4..b1 + 8].copy_from_slice(&500i32.to_le_bytes());
    let bank = AwcBank::parse(bytes).unwrap();
    let packets = bank.opus_packets(0).unwrap();
    assert_eq!(packets.len(), 18, "3 units x 6 packets from block 0");

    // Packet length running past its unit.
    let mut bytes = built.bytes;
    let unit0 = data_off + 0x800;
    bytes[unit0..unit0 + 2].copy_from_slice(&0x7FFFu16.to_le_bytes());
    let bank = AwcBank::parse(bytes).unwrap();
    assert_eq!(bank.opus_packets(0).unwrap().len(), 40 - 6);
}

#[test]
fn never_panics_on_corruption() {
    let (_, built) = three_channel_bank();
    let base = built.bytes;
    let exercise = |bytes: Vec<u8>| {
        if let Ok(bank) = AwcBank::parse(bytes) {
            for i in 0..bank.streams().len() {
                let _ = bank.opus_packets(i);
                let _ = bank.blocks(i);
                let _ = bank.block_sample_table(i);
            }
        }
    };
    // Every truncation inside the header region and coarse ones beyond.
    for len in (0..0x400).chain((0x400..base.len()).step_by(0x3F1)) {
        exercise(base[..len].to_vec());
    }
    // Deterministic random byte corruption.
    let mut x = 0x1234_5678u32;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    for _ in 0..1500 {
        let mut b = base.clone();
        for _ in 0..4 {
            let at = next() as usize % 0x1000.min(b.len());
            b[at] = next() as u8;
        }
        exercise(b);
    }
    // Garbage that starts with the magic.
    for _ in 0..500 {
        let mut b = b"ADAT".to_vec();
        b.extend((0..256).map(|_| next() as u8));
        exercise(b);
    }
}
