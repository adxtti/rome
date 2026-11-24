//! Ogg Opus output re-parsed with the independent `ogg` crate.

use std::io::Cursor;

use ogg::{OggReadError, PacketReader};
use u_rome_audio::{OggOpusOptions, ogg_crc32, write_ogg_opus, write_ogg_opus_with};

/// Synthetic packet: TOC byte followed by `len - 1` filler bytes.
fn packet(toc: u8, len: usize, seed: u8) -> Vec<u8> {
    let mut p = vec![toc];
    p.extend((1..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)));
    p
}

fn read_all(data: &[u8]) -> Vec<ogg::Packet> {
    let mut r = PacketReader::new(Cursor::new(data.to_vec()));
    let mut out = Vec::new();
    while let Some(p) = r.read_packet().expect("valid ogg") {
        out.push(p);
    }
    out
}

/// Raw page headers: (flags, granule, sequence, segment count).
fn pages(data: &[u8]) -> Vec<(u8, u64, u32, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        assert_eq!(&data[pos..pos + 4], b"OggS");
        let flags = data[pos + 5];
        let granule = u64::from_le_bytes(data[pos + 6..pos + 14].try_into().unwrap());
        let seq = u32::from_le_bytes(data[pos + 18..pos + 22].try_into().unwrap());
        let nseg = data[pos + 26] as usize;
        let body: usize = data[pos + 27..pos + 27 + nseg]
            .iter()
            .map(|&s| s as usize)
            .sum();
        // Recompute CRC independently of the writer.
        let end = pos + 27 + nseg + body;
        let mut page = data[pos..end].to_vec();
        let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
        page[22..26].fill(0);
        assert_eq!(ogg_crc32(&page), stored);
        out.push((flags, granule, seq, nseg));
        pos = end;
    }
    out
}

#[test]
fn headers_packets_and_granules() {
    // 100 packets of 20 ms CELT FB stereo (0xFC), 320 bytes as in GTA VI.
    let packets: Vec<Vec<u8>> = (0..100).map(|i| packet(0xFC, 320, i as u8)).collect();
    let data = write_ogg_opus(&packets, 2, 48_000, 312);

    let read = read_all(&data);
    assert_eq!(read.len(), 102);

    let head = &read[0].data;
    assert_eq!(&head[..8], b"OpusHead");
    assert_eq!(head.len(), 19);
    assert_eq!(head[8], 1, "version");
    assert_eq!(head[9], 2, "channels");
    assert_eq!(u16::from_le_bytes([head[10], head[11]]), 312, "pre-skip");
    assert_eq!(u32::from_le_bytes(head[12..16].try_into().unwrap()), 48_000);
    assert_eq!(head[18], 0, "mapping family");
    assert!(read[0].first_in_stream());
    assert!(read[0].last_in_page());

    assert_eq!(&read[1].data[..8], b"OpusTags");
    assert!(read[1].last_in_page());
    assert_eq!(read[1].absgp_page(), 0);

    for (i, p) in read[2..].iter().enumerate() {
        assert_eq!(p.data, packets[i], "packet {i}");
    }
    assert!(read.last().unwrap().last_in_stream());

    // 40 packets per page: granules 40*960, 80*960, 100*960.
    let pg = pages(&data);
    assert_eq!(pg.len(), 5);
    assert_eq!(pg[0].0, 0x02, "BOS");
    assert_eq!(pg[2].1, 40 * 960);
    assert_eq!(pg[3].1, 80 * 960);
    assert_eq!(pg[4].1, 100 * 960);
    assert_eq!(pg[4].0 & 0x04, 0x04, "EOS on last page");
    for (i, p) in pg.iter().enumerate() {
        assert_eq!(p.2 as usize, i, "sequence numbers");
        assert!(p.3 <= 255);
    }
    // 320 bytes lace as 255 + 65.
    assert_eq!(pg[2].3, 80);
}

#[test]
fn mixed_durations_and_end_trim() {
    // 2.5 ms mono CELT, 60 ms SILK, code 3 with 3 x 20 ms frames.
    let packets = vec![
        packet(0x80, 10, 1),       // config 16, 120 samples
        packet(0x18, 30, 2),       // config 3, 2880 samples
        vec![0xFB, 0x03, 0, 0, 0], // config 31 code 3, 3 frames = 2880
        packet(0xF9, 40, 3),       // config 31 code 1, 2 frames = 1920
    ];
    let total: u64 = 120 + 2880 + 2880 + 1920;
    let opts = OggOpusOptions {
        channels: 1,
        input_sample_rate: 44_100,
        pre_skip: 100,
        total_samples: Some(total - 500 - 100),
        comments: vec![("AWC_STREAM_ID".into(), "0x12345678".into())],
        packets_per_page: 2,
        ..OggOpusOptions::default()
    };
    let data = write_ogg_opus_with(&packets, &opts);
    let read = read_all(&data);
    assert_eq!(read.len(), 6);
    assert_eq!(read[0].data[9], 1);
    let tags = &read[1].data;
    let needle = b"AWC_STREAM_ID=0x12345678";
    assert!(tags.windows(needle.len()).any(|w| w == needle));

    let pg = pages(&data);
    assert_eq!(pg.len(), 4);
    assert_eq!(pg[2].1, 120 + 2880);
    // Last page trimmed: pre-skip + declared length.
    assert_eq!(pg[3].1, total - 500);
}

#[test]
fn huge_packet_spans_pages() {
    // 70,000 bytes need 275 lacing values: must continue onto a second page.
    let big = packet(0xFC, 70_000, 9);
    let small = packet(0xFC, 100, 5);
    let data = write_ogg_opus(&[big.clone(), small.clone()], 2, 48_000, 0);
    let read = read_all(&data);
    assert_eq!(read.len(), 4);
    assert_eq!(read[2].data, big);
    assert_eq!(read[3].data, small);

    let pg = pages(&data);
    assert_eq!(pg.len(), 4);
    assert_eq!(
        pg[2].1,
        u64::MAX,
        "no packet completes on the first big page"
    );
    assert_eq!(pg[3].0 & 0x01, 0x01, "continuation flag");
    assert_eq!(pg[3].1, 1920);
}

#[test]
fn empty_stream_and_bad_channels() {
    let none: [Vec<u8>; 0] = [];
    let data = write_ogg_opus(&none, 7, 48_000, 312);
    let read = read_all(&data);
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].data[9], 2, "invalid channel count clamps to 2");
    assert!(read[1].last_in_stream());
}

#[test]
fn corrupted_crc_is_rejected_by_ogg_crate() {
    let packets = vec![packet(0xFC, 320, 0)];
    let mut data = write_ogg_opus(&packets, 2, 48_000, 312);
    let last = data.len() - 1;
    data[last] ^= 0xFF;
    let mut r = PacketReader::new(Cursor::new(data));
    let mut err = None;
    loop {
        match r.read_packet() {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(matches!(err, Some(OggReadError::HashMismatch(_, _))));
}
