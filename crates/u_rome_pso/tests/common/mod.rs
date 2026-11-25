//! Builds synthetic PSIN documents in memory. No game data is used: every
//! byte is produced here from the layout documented in the crate.

#![allow(dead_code)]

pub struct M {
    pub name: u32,
    pub offset: u32,
    pub ty: u8,
    pub sub: u8,
    pub reference: u16,
    pub extra: u32,
}

pub fn m(name: u32, offset: u32, ty: u8) -> M {
    M {
        name,
        offset,
        ty,
        sub: 0,
        reference: 0,
        extra: 0,
    }
}

impl M {
    pub fn extra(mut self, extra: u32) -> M {
        self.extra = extra;
        self
    }
    pub fn reference(mut self, reference: u16) -> M {
        self.reference = reference;
        self
    }
    pub fn sub(mut self, sub: u8) -> M {
        self.sub = sub;
        self
    }
}

pub struct S {
    pub name: u32,
    pub size: u32,
    pub members: Vec<M>,
}

pub struct E {
    pub name: u32,
    pub values: Vec<(i64, u64)>,
}

#[derive(Default)]
pub struct DocBuilder {
    pub structs: Vec<S>,
    pub enums: Vec<E>,
    pub blocks: Vec<(u32, Vec<u8>)>,
    pub root: u32,
    /// Extra sections inserted before `CHKS`.
    pub extra_sections: Vec<([u8; 4], Vec<u8>)>,
    pub lrsc_block_count: Option<u16>,
    pub lrsc_enum_count: Option<u16>,
    pub chks_size_delta: i64,
}

pub fn pool_ref(block: u32, offset: u32) -> u64 {
    (u64::from(offset) << 20) | u64::from(block)
}

pub fn section(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(body);
}

fn pad16(out: &mut Vec<u8>, fill: u8) {
    while !out.len().is_multiple_of(16) {
        out.push(fill);
    }
}

impl DocBuilder {
    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"PSIN");
        out.extend_from_slice(&[0; 4]);
        for (ty, data) in &self.blocks {
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&0xAAAA_AAAA_AAAA_AAAAu64.to_le_bytes());
        }
        pad16(&mut out, 0x70);
        for (_, data) in &self.blocks {
            out.extend_from_slice(data);
            pad16(&mut out, 0);
        }
        let psin_size = out.len() as u32;
        out[4..8].copy_from_slice(&psin_size.to_be_bytes());

        let mut lsch = Vec::new();
        for s in &self.structs {
            lsch.extend_from_slice(&s.name.to_le_bytes());
            lsch.extend_from_slice(&0x5151_0000u32.to_le_bytes());
            lsch.extend_from_slice(&0x200u32.to_le_bytes());
            lsch.extend_from_slice(&0u32.to_le_bytes());
            lsch.extend_from_slice(&0xAAAA_AAAA_AAAA_AAAAu64.to_le_bytes());
            lsch.extend_from_slice(&s.size.to_le_bytes());
            lsch.extend_from_slice(&0u16.to_le_bytes());
            lsch.extend_from_slice(&(s.members.len() as u16).to_le_bytes());
        }
        for s in &self.structs {
            for m in &s.members {
                lsch.extend_from_slice(&m.name.to_le_bytes());
                lsch.extend_from_slice(&m.offset.to_le_bytes());
                lsch.push(m.ty);
                lsch.push(m.sub);
                lsch.extend_from_slice(&m.reference.to_le_bytes());
                lsch.extend_from_slice(&m.extra.to_le_bytes());
            }
        }
        section(&mut out, b"LSCH", &lsch);

        let mut lenm = Vec::new();
        for e in &self.enums {
            lenm.extend_from_slice(&e.name.to_le_bytes());
            lenm.extend_from_slice(&0u32.to_le_bytes());
            lenm.extend_from_slice(&0xEEEE_EEEE_EEEE_EEEEu64.to_le_bytes());
            lenm.extend_from_slice(&(e.values.len() as u32).to_le_bytes());
            lenm.extend_from_slice(&0u32.to_le_bytes());
        }
        for e in &self.enums {
            for (v, n) in &e.values {
                lenm.extend_from_slice(&v.to_le_bytes());
                lenm.extend_from_slice(&n.to_le_bytes());
            }
        }
        section(&mut out, b"LENM", &lenm);

        let mut lsns = Vec::new();
        lsns.extend_from_slice(&1u32.to_le_bytes());
        lsns.extend_from_slice(&0x101u32.to_le_bytes());
        lsns.extend_from_slice(&16u32.to_le_bytes());
        lsns.extend((0..16u8).map(|i| i.wrapping_mul(97).wrapping_add(13)));
        section(&mut out, b"LSNS", &lsns);
        section(&mut out, b"LGID", &[]);

        let mut lrsc = vec![0u8; 80];
        lrsc[0..4].copy_from_slice(b"0DRP");
        lrsc[4..8].copy_from_slice(&0x7au32.to_le_bytes());
        lrsc[0x10..0x14].copy_from_slice(&self.root.to_le_bytes());
        for i in 0..4 {
            let o = 0x18 + i * 8;
            lrsc[o..o + 8].copy_from_slice(&0xBBBB_BBBB_BBBB_BBBBu64.to_le_bytes());
        }
        lrsc[0x38..0x3a].copy_from_slice(&(self.structs.len() as u16).to_le_bytes());
        let enum_count = self.lrsc_enum_count.unwrap_or(self.enums.len() as u16);
        lrsc[0x3a..0x3c].copy_from_slice(&enum_count.to_le_bytes());
        let block_count = self.lrsc_block_count.unwrap_or(self.blocks.len() as u16);
        lrsc[0x3c..0x3e].copy_from_slice(&block_count.to_le_bytes());
        section(&mut out, b"LRSC", &lrsc);

        for (tag, body) in &self.extra_sections {
            section(&mut out, tag, body);
        }

        let total = (out.len() as i64 + 20 + self.chks_size_delta) as u32;
        let mut chks = Vec::new();
        chks.extend_from_slice(&total.to_be_bytes());
        chks.extend_from_slice(&0x1234_5678u32.to_be_bytes());
        chks.extend_from_slice(b"zppp");
        section(&mut out, b"CHKS", &chks);
        out
    }
}

/// Little-endian byte writer for block payloads.
#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i32(mut self, v: i32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn f32(mut self, v: f32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    pub fn pad_to(mut self, len: usize) -> Self {
        self.0.resize(len, 0);
        self
    }
    pub fn done(self) -> Vec<u8> {
        self.0
    }
}
