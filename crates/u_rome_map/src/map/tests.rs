use super::*;
use crate::meta::{MetaResource, Page};
use crate::synth::{Builder, Obj, Rng, m, m_ex};

const ANON: Hash32 = Hash32(0x100);

pub(crate) struct EntitySpec {
    pub model: &'static str,
    pub pos: [f32; 3],
    /// Stored (inverse) quaternion.
    pub stored_rot: [f32; 4],
    pub lod: u8,
    pub parent_index: i32,
    pub parent_instance: i32,
    pub num_children: u32,
    pub lod_dist: f32,
}

impl EntitySpec {
    pub fn new(model: &'static str, pos: [f32; 3], lod: u8) -> Self {
        Self {
            model,
            pos,
            stored_rot: [0.0, 0.0, 0.0, 1.0],
            lod,
            parent_index: -1,
            parent_instance: -1,
            num_children: 0,
            lod_dist: -1.0,
        }
    }
}

pub(crate) struct Fixture {
    pub name: &'static str,
    pub parent: &'static str,
    pub entities: Vec<EntitySpec>,
    pub with_batches: bool,
    pub mlo: bool,
}

fn entity_struct() -> Vec<crate::synth::M> {
    vec![
        m(fields::ROTATION, 0x10, 0x34),
        m(fields::GUID, 0x20, 0x08),
        m(fields::POSITION, 0x38, 0x32),
        m(fields::ARCHETYPE_NAME, 0x44, 0x4a),
        m(fields::FLAGS, 0x48, 0x15),
        m(fields::SCALE_XY, 0x54, 0x21),
        m(fields::SCALE_Z, 0x58, 0x21),
        m(fields::PARENT_INDEX, 0x5C, 0x14),
        m(fields::LOD_DIST, 0x60, 0x21),
        m(fields::CHILD_LOD_DIST, 0x64, 0x21),
        m(fields::NUM_CHILDREN, 0x68, 0x15),
        m(fields::PARENT_INSTANCE, 0x6C, 0x14),
        m(fields::LOD_LEVEL, 0x70, 0x60),
        m(fields::PARENT_DEP, 0x74, 0x14),
    ]
}

/// Prop instance bytes: rotation 90 degrees about Z, scale 2, translation t.
pub(crate) fn prop_instance(t: [f32; 3], tint: [u8; 4]) -> Vec<u8> {
    let mut o = Obj::new(64);
    o.fs(0, &[0.0, -2.0, 0.0, t[0]])
        .fs(16, &[2.0, 0.0, 0.0, t[1]])
        .fs(32, &[0.0, 0.0, 2.0, t[2]])
        .f32(0x30, 0.5)
        .bytes(0x34, &tint)
        .u16(0x3C, 7);
    o.0
}

pub(crate) fn build_fixture(fx: &Fixture) -> Builder {
    let mut b = Builder::default();
    b.add_struct(
        structs::CMAP_DATA,
        0x460,
        vec![
            m(fields::NAME, 0x08, 0x4a),
            m(fields::PARENT, 0x68, 0x4a),
            m(fields::FLAGS, 0x80, 0x15),
            m(fields::STREAMING_EXTENTS_MIN, 0x90, 0x33),
            m(fields::STREAMING_EXTENTS_MAX, 0xA0, 0x33),
            m(fields::ENTITIES_EXTENTS_MIN, 0xB0, 0x33),
            m(fields::ENTITIES_EXTENTS_MAX, 0xC0, 0x33),
            m(ANON, 0, 0x07),
            m_ex(fields::ENTITIES, 0xD0, 0x52, 7, 0),
            m_ex(
                fields::INSTANCED_DATA,
                0x140,
                0x05,
                0,
                structs::INSTANCED_MAP_DATA.0,
            ),
            m(fields::REGION_CODE, 0x180, 0x15),
        ],
    );
    b.add_struct(
        structs::INSTANCED_MAP_DATA,
        0x28,
        vec![
            m_ex(ANON, 0, 0x05, 0, structs::PROP_BATCH.0),
            m_ex(joaat("PropInstanceList"), 0x08, 0x52, 0, 0),
            m_ex(ANON, 0, 0x05, 0, structs::GRASS_BATCH.0),
            m_ex(joaat("GrassInstanceList"), 0x18, 0x52, 2, 0),
        ],
    );
    let entity_hash = if fx.mlo {
        structs::CMLO_INSTANCE_DEF
    } else {
        structs::CENTITY_DEF
    };
    let entity_size = if fx.mlo { 592 } else { 0x90 };
    b.add_struct(entity_hash, entity_size, entity_struct());
    b.add_struct(
        structs::PROP_BATCH,
        368,
        vec![
            m(fields::POSITION, 0x38, 0x32),
            m(fields::ARCHETYPE_NAME, 0x44, 0x4a),
            m(fields::LOD_DIST, 0x60, 0x21),
            m_ex(ANON, 0, 0x05, 0, structs::PROP_INSTANCE.0),
            m_ex(joaat("InstanceList"), 0x90, 0x52, 3, 0),
        ],
    );
    b.add_struct(structs::PROP_INSTANCE, 64, vec![]);
    b.add_struct(
        structs::GRASS_BATCH,
        416,
        vec![
            m(fields::ARCHETYPE_NAME, 0x44, 0x4a),
            m_ex(ANON, 0, 0x05, 0, structs::GRASS_INSTANCE.0),
            m_ex(joaat("InstanceList"), 0x178, 0x5c, 1, 0),
        ],
    );
    b.add_struct(structs::GRASS_INSTANCE, 16, vec![]);

    // Block 1: CMapData; 2: entities; 3: entity pointer pool; 4: prop batch;
    // 5: grass batch; 6: prop instances (gfx); 7: grass instances (gfx).
    let n = fx.entities.len();
    let mut md = Obj::new(0x460);
    md.u32(0x08, joaat(fx.name).0)
        .u32(
            0x68,
            if fx.parent.is_empty() {
                0
            } else {
                joaat(fx.parent).0
            },
        )
        .u32(0x80, 0x2)
        .fs(0x90, &[-100.0, -200.0, -10.0])
        .fs(0xA0, &[100.0, 200.0, 50.0])
        .fs(0xB0, &[-90.0, -190.0, -5.0])
        .fs(0xC0, &[90.0, 190.0, 40.0])
        .arr52(0xD0, Builder::pool(3, 0), n as u16)
        .u32(0x180, 0xABCD);
    if fx.with_batches {
        md.arr52(0x140 + 0x08, Builder::pool(4, 0), 1)
            .arr52(0x140 + 0x18, Builder::pool(5, 0), 1);
    }
    b.add_block(structs::CMAP_DATA, md.0, false);

    let mut ents = Obj::new(n * entity_size as usize);
    for (i, e) in fx.entities.iter().enumerate() {
        let o = i * entity_size as usize;
        ents.fs(o + 0x10, &e.stored_rot)
            .u64(o + 0x20, 0x1000 + i as u64)
            .fs(o + 0x38, &e.pos)
            .u32(o + 0x44, joaat(e.model).0)
            .u32(o + 0x48, 0x41)
            .f32(o + 0x54, 1.5)
            .f32(o + 0x58, 2.5)
            .i32(o + 0x5C, e.parent_index)
            .f32(o + 0x60, e.lod_dist)
            .f32(o + 0x64, 0.0)
            .u32(o + 0x68, e.num_children)
            .i32(o + 0x6C, e.parent_instance)
            .u8(o + 0x70, e.lod)
            .i32(o + 0x74, if e.parent_index < 0 { -1 } else { 0 });
    }
    b.add_block(entity_hash, ents.0, false);

    let mut pool = Obj::new(n.max(1) * 8);
    for i in 0..n {
        pool.u64(i * 8, Builder::pool(2, i * entity_size as usize));
    }
    b.add_block(Hash32(0x16), pool.0, false);

    let mut pb = Obj::new(368);
    pb.fs(0x38, &[10.0, 20.0, 1.0])
        .u32(0x44, joaat("prop_bush_01_batchlod").0)
        .f32(0x60, 300.0)
        .arr52(0x90, Builder::pool(6, 0), 2)
        .fs(0xA0, &[0.0, 0.0, 0.0])
        .fs(0xB0, &[20.0, 40.0, 2.0]);
    b.add_block(structs::PROP_BATCH, pb.0, false);

    let mut gb = Obj::new(416);
    gb.u32(0x44, joaat("veg_grass_sml_01").0)
        .fs(0x80, &[-10.0, -10.0, 0.0])
        .fs(0x90, &[10.0, 30.0, 4.0])
        .fs(0xA0, &[0.75, 1.25, 0.0])
        .f32(0xB8, 80.0)
        .arr5c(0x178, Builder::pool(7, 0), 2);
    b.add_block(structs::GRASS_BATCH, gb.0, false);

    let mut pi = prop_instance([1.0, 2.0, 3.0], [255, 128, 0, 255]);
    pi.extend(prop_instance([11.0, 12.0, 1.5], [1, 2, 3, 4]));
    b.add_block(structs::PROP_INSTANCE, pi, true);

    let mut gi = Obj::new(32);
    gi.u16(0, 0)
        .u16(2, 0)
        .u16(4, 0)
        .u8(6, 128)
        .u8(7, 128)
        .bytes(8, &[83, 78, 57])
        .u8(11, 0)
        .u8(12, 200);
    gi.u16(16, 65535)
        .u16(18, 32768)
        .u16(20, 65535)
        .u8(22, 255)
        .u8(23, 128)
        .bytes(24, &[1, 2, 3])
        .u8(27, 255)
        .u8(28, 10);
    b.add_block(structs::GRASS_INSTANCE, gi.0, true);
    b.root = 1;
    b
}

pub(crate) fn sample() -> Fixture {
    let mut lod = EntitySpec::new("bldg_01_lod", [5.0, 6.0, 7.0], 1);
    lod.num_children = 1;
    lod.lod_dist = 500.0;
    let mut hd = EntitySpec::new("bldg_01", [5.1, 6.0, 7.0], 0);
    hd.parent_index = 0;
    // Stored inverse of a 90 degree rotation about Z.
    let h = std::f32::consts::FRAC_1_SQRT_2;
    hd.stored_rot = [0.0, 0.0, -h, h];
    Fixture {
        name: "dt1_01",
        parent: "dt1_01_lod",
        entities: vec![lod, hd],
        with_batches: true,
        mlo: false,
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

#[test]
fn decodes_map_entities_and_batches() {
    let (sys, gfx) = build_fixture(&sample()).build_pages();
    let meta = MetaResource::parse_pages(&sys, &gfx).unwrap();
    assert!(!meta.provisional);
    assert_eq!(meta.root().unwrap().struct_hash, structs::CMAP_DATA);
    let map = MapData::from_meta(&meta).unwrap();
    assert_eq!(map.name, joaat("dt1_01"));
    assert_eq!(map.parent, joaat("dt1_01_lod"));
    assert_eq!(map.flags, 2);
    assert_eq!(map.region_code, Some(0xABCD));
    assert_eq!(map.extents.streaming_min, [-100.0, -200.0, -10.0]);
    assert_eq!(map.extents.entities_max, [90.0, 190.0, 40.0]);
    assert_eq!(map.entities.len(), 2);

    let lod = &map.entities[0];
    assert_eq!(lod.archetype, joaat("bldg_01_lod"));
    assert_eq!(lod.lod_level, LodLevel::Lod);
    assert_eq!(lod.num_children, 1);
    assert_eq!(lod.lod_dist, 500.0);
    assert_eq!(lod.parent_index, -1);
    assert_eq!(lod.guid, 0x1000);
    assert_eq!(lod.flags, 0x41);
    assert_eq!(lod.scale_xy, 1.5);
    assert_eq!(lod.scale_z, 2.5);
    assert_eq!(lod.kind, PlacementKind::Entity);

    let hd = &map.entities[1];
    assert_eq!(hd.lod_level, LodLevel::Hd);
    assert_eq!(hd.parent_index, 0);
    assert_eq!(hd.parent_instance, -1);
    assert_eq!(hd.position, [5.1, 6.0, 7.0]);
    assert_eq!(hd.parent_dep, 0);
    let h = std::f32::consts::FRAC_1_SQRT_2;
    assert!(close(hd.rotation[2], h) && close(hd.rotation[3], h));

    assert_eq!(map.prop_batches.len(), 1);
    let pb = &map.prop_batches[0];
    assert_eq!(pb.archetype, joaat("prop_bush_01_batchlod"));
    assert_eq!(pb.lod_dist, 300.0);
    assert_eq!(pb.bbox_max, [20.0, 40.0, 2.0]);
    assert_eq!(pb.instances.len(), 2);
    assert_eq!(pb.instances[0].translation(), [1.0, 2.0, 3.0]);
    assert_eq!(pb.instances[0].tint, [255, 128, 0, 255]);
    assert_eq!(pb.instances[0].unknown_3c, 7);
    let s = pb.instances[0].scale();
    assert!(close(s[0], 2.0) && close(s[1], 2.0) && close(s[2], 2.0));
    assert_eq!(pb.instances[1].tint, [1, 2, 3, 4]);

    assert_eq!(map.grass_batches.len(), 1);
    let gb = &map.grass_batches[0];
    assert_eq!(gb.archetype, joaat("veg_grass_sml_01"));
    assert_eq!(gb.lod_dist, 80.0);
    assert_eq!(gb.instances.len(), 2);
    assert_eq!(gb.instances[0].position, [-10.0, -10.0, 0.0]);
    assert_eq!(gb.instances[0].ao, 200);
    assert_eq!(gb.instances[0].color, [83, 78, 57]);
    assert!(close(gb.instances[0].scale, 0.75));
    assert!(close(gb.instances[1].scale, 1.25));
    // Batches carry inferred layouts.
    assert!(pb.provisional && gb.provisional && map.provisional);
    assert_eq!(map.lod_slot_count(), 3);
}

#[test]
fn grass_dequantisation() {
    let p = dequantise_grass_position(
        [-10.0, -10.0, 0.0],
        [10.0, 30.0, 4.0],
        [65535, 32768, 65535],
    );
    assert!(close(p[0], 10.0));
    assert!(close(p[1], -10.0 + 40.0 * 32768.0 / 65535.0));
    assert!(close(p[2], 4.0));
    let n = dequantise_grass_normal(128, 128);
    assert!(n[0].abs() < 0.01 && n[1].abs() < 0.01 && close(n[2], 1.0 - n[0] * n[0] / 2.0));
    let n = dequantise_grass_normal(255, 128);
    assert!(close(n[0], 1.0) && n[2].abs() < 0.1);
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    assert!(len <= 1.0001);
}

#[test]
fn quaternion_inversion() {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let stored = [0.0, 0.0, -h, h];
    let q = invert_quat(stored);
    assert!(close(q[2], h) && close(q[3], h));
    // Entity rotation of +90 degrees about Z maps +X to +Y.
    let r = quat_to_mat3(q);
    assert!(close(r[0][0], 0.0) && close(r[1][0], 1.0));
    // Non-unit input is inverted, not just conjugated.
    let q2 = invert_quat([0.0, 0.0, 0.0, 2.0]);
    assert!(close(q2[3], 0.5));
    // Degenerate input becomes identity.
    assert_eq!(invert_quat([0.0; 4]), [0.0, 0.0, 0.0, 1.0]);
    assert_eq!(invert_quat([f32::NAN; 4]), [0.0, 0.0, 0.0, 1.0]);
    let p = Placement {
        archetype: Hash32(1),
        position: [1.0, 2.0, 3.0],
        rotation: q,
        scale_xy: 2.0,
        scale_z: 3.0,
        flags: 0,
        lod_level: LodLevel::Hd,
        parent_index: -1,
        parent_instance: -1,
        num_children: 0,
        lod_dist: -1.0,
        child_lod_dist: 0.0,
        guid: 0,
        parent_dep: -1,
        kind: PlacementKind::Entity,
    };
    let m = p.transform();
    // Column 0 = rotated, scaled X axis = (0, 2, 0).
    assert!(close(m[0][0], 0.0) && close(m[0][1], 2.0));
    assert!(close(m[2][2], 3.0));
    assert_eq!(m[3], [1.0, 2.0, 3.0, 1.0]);
}

#[test]
fn mlo_instance_rotation_is_not_inverted() {
    let mut fx = sample();
    fx.mlo = true;
    let (sys, gfx) = build_fixture(&fx).build_pages();
    let meta = MetaResource::parse_pages(&sys, &gfx).unwrap();
    let map = MapData::from_meta(&meta).unwrap();
    let hd = &map.entities[1];
    assert_eq!(hd.kind, PlacementKind::MloInstance);
    let h = std::f32::consts::FRAC_1_SQRT_2;
    assert!(close(hd.rotation[2], -h));
}

#[test]
fn concatenated_buffer_infers_graphics_page() {
    let fx = sample();
    let b = build_fixture(&fx);
    let (sys, gfx) = b.build_pages();
    let exact = MapData::from_meta(&MetaResource::parse_pages(&sys, &gfx).unwrap()).unwrap();
    let meta = MetaResource::parse(&b.build()).unwrap();
    assert!(meta.provisional);
    assert_eq!(meta.page(Page::Graphics).len(), gfx.len());
    let inferred = MapData::from_meta(&meta).unwrap();
    assert_eq!(inferred.prop_batches, exact.prop_batches);
    assert_eq!(inferred.grass_batches, exact.grass_batches);
    assert!(
        inferred
            .warnings
            .iter()
            .any(|w| w.contains("graphics page"))
    );
}

#[test]
fn schema_without_members_uses_documented_offsets() {
    let fx = Fixture {
        with_batches: false,
        ..sample()
    };
    let mut b = build_fixture(&fx);
    // Strip the CEntityDef members: core fields fall back to documented offsets.
    for s in &mut b.structs {
        if s.name == structs::CENTITY_DEF {
            s.members.clear();
        }
    }
    let (sys, gfx) = b.build_pages();
    let map = MapData::from_meta(&MetaResource::parse_pages(&sys, &gfx).unwrap()).unwrap();
    assert!(map.provisional);
    assert_eq!(map.entities.len(), 2);
    let hd = &map.entities[1];
    assert_eq!(hd.archetype, joaat("bldg_01"));
    assert_eq!(hd.position, [5.1, 6.0, 7.0]);
    assert_eq!(hd.scale_z, 2.5);
    assert_eq!(hd.lod_level, LodLevel::Unknown(u32::MAX));
    assert_eq!(hd.parent_index, -1);
    assert!(map.warnings.iter().any(|w| w.contains("lodLevel")));
}

#[test]
fn archetype_defs() {
    let mut b = Builder::default();
    b.add_struct(
        structs::CBASE_ARCHETYPE_DEF,
        184,
        vec![m(fields::NAME, 0x74, 0x4a), m(fields::LOD_DIST, 0x38, 0x21)],
    );
    let mut o = Obj::new(184);
    o.f32(0x38, 150.0)
        .fs(0x40, &[-1.0, -2.0, -3.0])
        .fs(0x4C, &[1.0, 2.0, 3.0])
        .f32(0x64, 4.0)
        .u32(0x74, joaat("prop_chair_01").0)
        .u32(0x78, joaat("prop_chair_txd").0)
        .u32(0x80, joaat("prop_chair_phys").0);
    b.add_block(structs::CBASE_ARCHETYPE_DEF, o.0, false);
    let meta = MetaResource::parse(&b.build()).unwrap();
    let defs = ArchetypeDef::all_from_meta(&meta);
    assert_eq!(defs.len(), 1);
    let d = &defs[0];
    assert_eq!(d.name, archetype_hash("prop_chair_01"));
    assert_eq!(d.lod_dist, 150.0);
    assert_eq!(d.bb_max, [1.0, 2.0, 3.0]);
    assert_eq!(d.bs_radius, 4.0);
    assert_eq!(d.texture_dictionary, joaat("prop_chair_txd"));
    assert_eq!(d.physics_dictionary, joaat("prop_chair_phys"));
    assert!(d.provisional); // bbox etc. from documented offsets
}

#[test]
fn no_map_data_is_an_error() {
    let b = Builder::default();
    let meta = MetaResource::parse(&b.build()).unwrap();
    assert!(matches!(
        MapData::from_meta(&meta),
        Err(MapError::NoMapData)
    ));
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for i in 0..3000 {
        let len = (rng.next() % 2048) as usize;
        let mut buf: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if i % 2 == 0 && buf.len() >= 0x50 {
            buf[0x10..0x14].copy_from_slice(b"0DRP");
            buf[0x14] = 0x7a;
            // Point tables somewhere inside the buffer with small counts.
            let small = |r: &mut Rng| (r.next() % 64) as u16;
            buf[0x48..0x4A].copy_from_slice(&small(&mut rng).to_le_bytes());
            buf[0x4C..0x4E].copy_from_slice(&small(&mut rng).to_le_bytes());
        }
        if let Ok(meta) = MetaResource::parse(&buf) {
            let _ = MapData::from_meta(&meta);
            let _ = ArchetypeDef::all_from_meta(&meta);
        }
    }
}

#[test]
fn mutated_and_truncated_fixtures_never_panic() {
    let good = build_fixture(&sample()).build();
    let mut rng = Rng(12345);
    for _ in 0..3000 {
        let mut buf = good.clone();
        let flips = 1 + rng.next() % 16;
        for _ in 0..flips {
            let at = (rng.next() as usize) % buf.len();
            buf[at] = rng.next() as u8;
        }
        if let Ok(meta) = MetaResource::parse(&buf) {
            let _ = MapData::from_meta(&meta);
        }
    }
    for len in 0..good.len() {
        if let Ok(meta) = MetaResource::parse(&good[..len]) {
            let _ = MapData::from_meta(&meta);
        }
    }
}
