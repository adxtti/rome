//! Synthetic AWC bank builder for tests. Produces the layout described in
//! Albotterin/AWC_from_RPF9_Extractor docs/FORMAT.md; no game data involved.

#![allow(dead_code)]

pub mod opus_fixture;

use u_rome_audio::opus_packet_samples;

pub const UNIT: usize = 0x800;

/// A chunk to place in the bank: (tag, payload).
pub type ChunkSpec = (u8, Vec<u8>);

/// Assemble a bank from stream table entries `(id, chunks)`.
pub fn assemble(entries: &[(u32, Vec<ChunkSpec>)], flags: u16, with_indices: bool) -> Vec<u8> {
    let n = entries.len();
    let total: usize = entries.iter().map(|e| e.1.len()).sum();
    let header = 0x10 + if with_indices { 2 * n } else { 0 } + 4 * n + 8 * total;
    let mut out = Vec::new();
    out.extend_from_slice(b"ADAT");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(header as u32).to_le_bytes());
    if with_indices {
        let mut start = 0u16;
        for (_, chunks) in entries {
            out.extend_from_slice(&start.to_le_bytes());
            start += chunks.len() as u16;
        }
    }
    for (id, chunks) in entries {
        let v = (id & 0x1FFF_FFFF) | ((chunks.len() as u32) << 29);
        out.extend_from_slice(&v.to_le_bytes());
    }
    // Lay out payloads after the header, 16-byte aligned.
    let mut pos = header.div_ceil(16) * 16;
    let mut placed = Vec::new();
    for (_, chunks) in entries {
        for (tag, payload) in chunks {
            placed.push((*tag, pos, payload));
            pos = (pos + payload.len()).div_ceil(16) * 16;
        }
    }
    for (tag, off, payload) in &placed {
        let v = (*off as u64) | ((payload.len() as u64) << 28) | (u64::from(*tag) << 56);
        out.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(out.len(), header);
    for (_, off, payload) in &placed {
        out.resize(*off, 0);
        out.extend_from_slice(payload);
    }
    out
}

/// One channel stream of a music bank.
#[derive(Clone)]
pub struct Channel {
    pub id: u32,
    pub packets: Vec<Vec<u8>>,
    /// Events stored in the stream table entry with this channel's id.
    pub events: Vec<[u32; 4]>,
}

#[derive(Clone)]
pub struct MusicBank {
    pub channels: Vec<Channel>,
    pub packets_per_unit: usize,
    pub units_per_block: usize,
    pub with_indices: bool,
    pub flags: u16,
    pub codec: u8,
    pub sample_rate: u16,
    /// Events on the music (id 0) entry.
    pub music_events: Vec<[u32; 4]>,
    /// Override the header sample count (interleaved) for every channel.
    pub raw_samples: Option<u32>,
}

impl Default for MusicBank {
    fn default() -> Self {
        MusicBank {
            channels: Vec::new(),
            packets_per_unit: 6,
            units_per_block: 3,
            with_indices: true,
            flags: 0xFF01,
            codec: 0x0B,
            sample_rate: 48_000,
            music_events: Vec::new(),
            raw_samples: None,
        }
    }
}

pub struct Built {
    pub bytes: Vec<u8>,
    pub block_size: usize,
    pub block_count: usize,
    /// block -> channel -> seek table written.
    pub seek: Vec<Vec<Vec<u32>>>,
    /// block -> channel -> units written.
    pub units: Vec<Vec<u32>>,
    /// 0xA3 table written.
    pub block_table: Vec<u32>,
}

fn interleave_factor(packet: &[u8]) -> u32 {
    if packet.first().is_some_and(|t| t & 4 != 0) {
        2
    } else {
        1
    }
}

/// Pack packets into 0x800-byte units. Returns (unit bytes, interleaved samples).
fn pack_units(packets: &[Vec<u8>], per_unit: usize) -> Vec<(Vec<u8>, u32)> {
    let mut units = Vec::new();
    let mut cur = Vec::new();
    let mut count = 0;
    let mut samples = 0u32;
    for p in packets {
        if count == per_unit || cur.len() + 2 + p.len() > UNIT {
            cur.resize(UNIT, 0);
            units.push((std::mem::take(&mut cur), samples));
            count = 0;
            samples = 0;
        }
        cur.extend_from_slice(&(p.len() as u16).to_le_bytes());
        cur.extend_from_slice(p);
        count += 1;
        samples += opus_packet_samples(p).unwrap_or(0) * interleave_factor(p);
    }
    if count > 0 {
        cur.resize(UNIT, 0);
        units.push((cur, samples));
    }
    units
}

impl MusicBank {
    pub fn build(&self) -> Built {
        let nch = self.channels.len();
        let upb = self.units_per_block;
        let units: Vec<Vec<(Vec<u8>, u32)>> = self
            .channels
            .iter()
            .map(|c| pack_units(&c.packets, self.packets_per_unit))
            .collect();
        let block_count = units
            .iter()
            .map(|u| u.len().div_ceil(upb))
            .max()
            .unwrap_or(0);
        // One header unit plus upb units per channel, plus one spare unit of padding.
        let block_size = UNIT * (2 + upb * nch);

        let mut data = Vec::new();
        let mut seek_all = Vec::new();
        let mut units_all = Vec::new();
        let mut block_table = Vec::new();
        let mut running = vec![0u32; nch];
        for b in 0..block_count {
            block_table.push(running.first().copied().unwrap_or(0));
            let start = data.len();
            let slices: Vec<&[(Vec<u8>, u32)]> = units
                .iter()
                .map(|u| {
                    let lo = (b * upb).min(u.len());
                    let hi = ((b + 1) * upb).min(u.len());
                    &u[lo..hi]
                })
                .collect();
            let mut seeks = Vec::new();
            for s in &slices {
                let block_samples: u32 = s.iter().map(|u| u.1).sum();
                data.extend_from_slice(&(-1i32).to_le_bytes());
                data.extend_from_slice(&(s.len() as i32).to_le_bytes());
                data.extend_from_slice(&0i32.to_le_bytes());
                data.extend_from_slice(&(block_samples as i32).to_le_bytes());
                data.extend_from_slice(&[0; 8]);
            }
            for (ch, s) in slices.iter().enumerate() {
                let mut table = Vec::new();
                for u in s.iter() {
                    table.push(running[ch]);
                    running[ch] += u.1;
                }
                for v in &table {
                    data.extend_from_slice(&v.to_le_bytes());
                }
                seeks.push(table);
            }
            let header_end = start + (data.len() - start).div_ceil(UNIT) * UNIT;
            data.resize(header_end, 0);
            for s in &slices {
                for u in s.iter() {
                    data.extend_from_slice(&u.0);
                }
            }
            if b + 1 < block_count {
                data.resize(start + block_size, 0);
            }
            units_all.push(slices.iter().map(|s| s.len() as u32).collect());
            seek_all.push(seeks);
        }

        let mut fmt = Vec::new();
        fmt.extend_from_slice(&(block_count as u32).to_le_bytes());
        fmt.extend_from_slice(&(block_size as u32).to_le_bytes());
        fmt.extend_from_slice(&(nch as u32).to_le_bytes());
        for (c, u) in self.channels.iter().zip(&units) {
            let samples = self
                .raw_samples
                .unwrap_or_else(|| u.iter().map(|x| x.1).sum());
            fmt.extend_from_slice(&c.id.to_le_bytes());
            fmt.extend_from_slice(&samples.to_le_bytes());
            fmt.extend_from_slice(&(-100i16).to_le_bytes());
            fmt.extend_from_slice(&self.sample_rate.to_le_bytes());
            fmt.push(self.codec);
            fmt.extend_from_slice(&[0x02, 0xFF, 0xFF]);
        }
        let mut table = Vec::new();
        for v in &block_table {
            table.extend_from_slice(&v.to_le_bytes());
        }

        let mut music = vec![(0x48, fmt), (0xA3, table)];
        if !self.music_events.is_empty() {
            music.push((0xBD, events_bytes(&self.music_events)));
        }
        music.push((0x55, data));
        let mut entries = vec![(0u32, music)];
        for c in &self.channels {
            let chunks = if c.events.is_empty() {
                Vec::new()
            } else {
                vec![(0xBD, events_bytes(&c.events))]
            };
            entries.push((c.id, chunks));
        }
        Built {
            bytes: assemble(&entries, self.flags, self.with_indices),
            block_size,
            block_count,
            seek: seek_all,
            units: units_all,
            block_table,
        }
    }
}

pub fn events_bytes(events: &[[u32; 4]]) -> Vec<u8> {
    events
        .iter()
        .flat_map(|e| e.iter().flat_map(|w| w.to_le_bytes()))
        .collect()
}

/// Packet with a TOC byte and deterministic filler.
pub fn packet(toc: u8, len: usize, seed: u32) -> Vec<u8> {
    let mut p = vec![toc];
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    for _ in 1..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        p.push(x as u8);
    }
    p
}

/// Single-stream PCM bank using vgmstream's 0xFA sfx header layout.
pub fn sfx_pcm_bank(id: u32, samples: &[i16], rate: u16, codec: u8) -> Vec<u8> {
    let mut hdr = vec![0u8; 0x14];
    hdr[0..4].copy_from_slice(&(samples.len() as u32).to_le_bytes());
    hdr[4..8].copy_from_slice(&(-1i32).to_le_bytes());
    hdr[8..10].copy_from_slice(&rate.to_le_bytes());
    hdr[10..12].copy_from_slice(&(-50i16).to_le_bytes());
    hdr[0x13] = codec;
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    assemble(&[(id, vec![(0xFA, hdr), (0x55, data)])], 0xFF01, true)
}
