//! Synthetic meta resource builder for tests. No game data is involved: every
//! byte is produced here from the documented layout.

#![cfg(test)]

use u_rome_core::Hash32;

use crate::meta::make_pool_ref;

pub const SYS: u64 = 0x5000_0000;
pub const GFX: u64 = 0x6000_0000;

#[derive(Clone, Copy)]
pub struct M {
    pub name: u32,
    pub offset: u32,
    pub ty: u8,
    pub sub: u8,
    pub reference: u16,
    pub extra: u32,
}

pub fn m(name: Hash32, offset: u32, ty: u8) -> M {
    M {
        name: name.0,
        offset,
        ty,
        sub: 0,
        reference: 0,
        extra: 0,
    }
}

pub fn m_ex(name: Hash32, offset: u32, ty: u8, reference: u16, extra: u32) -> M {
    M {
        name: name.0,
        offset,
        ty,
        sub: 0,
        reference,
        extra,
    }
}

pub struct S {
    pub name: Hash32,
    pub size: u32,
    pub members: Vec<M>,
}

pub struct B {
    pub type_hash: Hash32,
    pub data: Vec<u8>,
    pub graphics: bool,
}

#[derive(Default)]
pub struct Builder {
    pub structs: Vec<S>,
    pub blocks: Vec<B>,
    pub root: u32,
}

/// Little-endian object buffer with typed writers.
pub struct Obj(pub Vec<u8>);

impl Obj {
    pub fn new(size: usize) -> Self {
        Obj(vec![0; size])
    }
    fn put(&mut self, off: usize, b: &[u8]) {
        if self.0.len() < off + b.len() {
            self.0.resize(off + b.len(), 0);
        }
        self.0[off..off + b.len()].copy_from_slice(b);
    }
    pub fn u8(&mut self, off: usize, v: u8) -> &mut Self {
        self.put(off, &[v]);
        self
    }
    pub fn u16(&mut self, off: usize, v: u16) -> &mut Self {
        self.put(off, &v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, off: usize, v: u32) -> &mut Self {
        self.put(off, &v.to_le_bytes());
        self
    }
    pub fn i32(&mut self, off: usize, v: i32) -> &mut Self {
        self.put(off, &v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, off: usize, v: u64) -> &mut Self {
        self.put(off, &v.to_le_bytes());
        self
    }
    pub fn f32(&mut self, off: usize, v: f32) -> &mut Self {
        self.put(off, &v.to_le_bytes());
        self
    }
    pub fn fs(&mut self, off: usize, v: &[f32]) -> &mut Self {
        for (i, f) in v.iter().enumerate() {
            self.f32(off + i * 4, *f);
        }
        self
    }
    /// 0x52 array: u64 ref, u16 count, u16 capacity.
    pub fn arr52(&mut self, off: usize, r: u64, count: u16) -> &mut Self {
        self.u64(off, r).u16(off + 8, count).u16(off + 10, count)
    }
    /// 0x5c array: u32 count, u32 capacity, u64 ref.
    pub fn arr5c(&mut self, off: usize, r: u64, count: u32) -> &mut Self {
        self.u32(off, count).u32(off + 4, count).u64(off + 8, r)
    }
    pub fn bytes(&mut self, off: usize, b: &[u8]) -> &mut Self {
        self.put(off, b);
        self
    }
}

fn align16(v: usize) -> usize {
    (v + 15) & !15
}

impl Builder {
    pub fn add_struct(&mut self, name: Hash32, size: u32, members: Vec<M>) {
        self.structs.push(S {
            name,
            size,
            members,
        });
    }

    /// Add a block, returning its 1-based index.
    pub fn add_block(&mut self, type_hash: Hash32, data: Vec<u8>, graphics: bool) -> u16 {
        self.blocks.push(B {
            type_hash,
            data,
            graphics,
        });
        self.blocks.len() as u16
    }

    pub fn pool(index: u16, offset: usize) -> u64 {
        make_pool_ref(index, offset)
    }

    /// Build (system page, graphics page).
    pub fn build_pages(&self) -> (Vec<u8>, Vec<u8>) {
        let mut sys = vec![0u8; 0x50];
        sys[0x10..0x14].copy_from_slice(b"0DRP");
        sys[0x14] = 0x7a;
        sys[0x20..0x24].copy_from_slice(&self.root.to_le_bytes());

        let struct_table = align16(sys.len());
        let member_base = struct_table + self.structs.len() * 32;
        let total_members: usize = self.structs.iter().map(|s| s.members.len()).sum();
        let block_table = align16(member_base + total_members * 16);
        let mut data_start = align16(block_table + self.blocks.len() * 16);
        sys.resize(data_start, 0);

        let mut mi = member_base;
        let mut srec = Vec::new();
        let mut mrec = Vec::new();
        for s in &self.structs {
            let mut r = Obj::new(32);
            r.u32(0, s.name.0)
                .u32(4, 0x1234)
                .u32(8, 0x300)
                .u64(
                    16,
                    if s.members.is_empty() {
                        0
                    } else {
                        SYS | mi as u64
                    },
                )
                .u32(24, s.size)
                .u16(30, s.members.len() as u16);
            srec.extend_from_slice(&r.0);
            for m in &s.members {
                let mut r = Obj::new(16);
                r.u32(0, m.name)
                    .u32(4, m.offset)
                    .u8(8, m.ty)
                    .u8(9, m.sub)
                    .u16(10, m.reference)
                    .u32(12, m.extra);
                mrec.extend_from_slice(&r.0);
            }
            mi += s.members.len() * 16;
        }
        sys[struct_table..struct_table + srec.len()].copy_from_slice(&srec);
        sys[member_base..member_base + mrec.len()].copy_from_slice(&mrec);

        let mut gfx = Vec::new();
        let mut brec = Vec::new();
        for b in &self.blocks {
            let ptr = if b.graphics {
                let off = align16(gfx.len());
                gfx.resize(off, 0);
                gfx.extend_from_slice(&b.data);
                GFX | off as u64
            } else {
                sys.resize(data_start, 0);
                sys.extend_from_slice(&b.data);
                let p = SYS | data_start as u64;
                data_start = align16(sys.len());
                p
            };
            let mut r = Obj::new(16);
            r.u32(0, b.type_hash.0)
                .u32(4, b.data.len() as u32)
                .u64(8, ptr);
            brec.extend_from_slice(&r.0);
        }
        sys.resize(align16(sys.len()), 0);
        sys[block_table..block_table + brec.len()].copy_from_slice(&brec);

        let w =
            |sys: &mut Vec<u8>, off: usize, v: &[u8]| sys[off..off + v.len()].copy_from_slice(v);
        w(&mut sys, 0x28, &(SYS | struct_table as u64).to_le_bytes());
        w(&mut sys, 0x30, &0u64.to_le_bytes());
        w(&mut sys, 0x38, &(SYS | block_table as u64).to_le_bytes());
        w(&mut sys, 0x48, &(self.structs.len() as u16).to_le_bytes());
        w(&mut sys, 0x4A, &0u16.to_le_bytes());
        w(&mut sys, 0x4C, &(self.blocks.len() as u16).to_le_bytes());
        (sys, gfx)
    }

    /// System page followed directly by the graphics page.
    pub fn build(&self) -> Vec<u8> {
        let (mut sys, gfx) = self.build_pages();
        sys.extend_from_slice(&gfx);
        sys
    }
}

/// Deterministic xorshift PRNG for fuzz-style tests.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}
