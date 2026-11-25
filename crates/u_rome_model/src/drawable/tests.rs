use super::*;
use crate::index::encode_index_buffer;
use crate::testutil::{BlobBuilder, lcg};
use crate::vertex::{format, quantize_position_word};

/// Builds a synthetic resource: a system page and a graphics page with
/// pointers in the 0x5xxxxxxx / 0x6xxxxxxx form.
#[derive(Default)]
struct Res {
    sys: Vec<u8>,
    gfx: Vec<u8>,
}

fn sp(off: usize) -> u64 {
    0x5000_0000 | off as u64
}
fn gp(off: usize) -> u64 {
    0x6000_0000 | off as u64
}

impl Res {
    fn alloc(&mut self, len: usize) -> usize {
        while !self.sys.len().is_multiple_of(16) {
            self.sys.push(0);
        }
        let off = self.sys.len();
        self.sys.resize(off + len, 0);
        off
    }
    fn gfx_put(&mut self, bytes: &[u8]) -> usize {
        while !self.gfx.len().is_multiple_of(16) {
            self.gfx.push(0);
        }
        let off = self.gfx.len();
        self.gfx.extend_from_slice(bytes);
        off
    }
    fn w16(&mut self, off: usize, v: u16) {
        self.sys[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn w32(&mut self, off: usize, v: u32) {
        self.sys[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn w64(&mut self, off: usize, v: u64) {
        self.sys[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn wf(&mut self, off: usize, v: &[f32]) {
        for (i, x) in v.iter().enumerate() {
            self.sys[off + i * 4..off + i * 4 + 4].copy_from_slice(&x.to_le_bytes());
        }
    }
    fn string(&mut self, s: &str) -> usize {
        let off = self.alloc(s.len() + 1);
        self.sys[off..off + s.len()].copy_from_slice(s.as_bytes());
        off
    }
    fn buf_desc(&mut self, gfx_off: usize, len: usize, flags: u16) -> usize {
        let d = self.alloc(0x20);
        self.w32(d + 8, len.div_ceil(4) as u32);
        self.w16(d + 0x10, flags);
        self.w64(d + 0x18, gp(gfx_off));
        d
    }
    /// Add a geometry record for (vertex blob, triangle list).
    fn geometry(&mut self, blob: &[u8], tris: &[u32]) -> usize {
        let vo = self.gfx_put(blob);
        let ib = encode_index_buffer(tris).unwrap();
        let io = self.gfx_put(&ib);
        let vd = self.buf_desc(vo, blob.len(), 0x1601);
        let id = self.buf_desc(io, ib.len(), 0x1380);
        let holder = self.alloc(8);
        self.w64(holder, sp(id));
        let g = self.alloc(0xA0);
        self.w64(g + 0x08, sp(vd));
        self.w64(g + 0x40, sp(holder));
        g
    }
    /// A model struct with the given geometries and shader indices;
    /// returns the pointer value stored in model arrays (struct - 8).
    fn model(&mut self, geoms: &[usize], shaders: &[u16], palette: &[u32]) -> usize {
        let base = self.alloc(0x48);
        let s = base + 8;
        let arr = self.alloc(geoms.len() * 8);
        for (i, g) in geoms.iter().enumerate() {
            self.w64(arr + i * 8, sp(*g));
        }
        let sh = self.alloc(shaders.len() * 2);
        for (i, v) in shaders.iter().enumerate() {
            self.w16(sh + i * 2, *v);
        }
        self.w64(s, sp(arr));
        self.w16(s + 8, geoms.len() as u16);
        self.w16(s + 10, geoms.len() as u16);
        self.w64(s + 0x18, sp(sh));
        if !palette.is_empty() {
            let p = self.alloc(palette.len() * 4);
            for (i, h) in palette.iter().enumerate() {
                self.w32(p + i * 4, *h);
            }
            self.w16(s + 0x0C, palette.len() as u16);
            self.w64(s + 0x20, sp(p));
        }
        base
    }
    fn lod(&mut self, drawable: usize, lod: usize, models: &[usize]) {
        let arr = self.alloc(models.len() * 8);
        for (i, m) in models.iter().enumerate() {
            self.w64(arr + i * 8, sp(*m));
        }
        let h = self.alloc(16);
        self.w64(h, sp(arr));
        self.w16(h + 8, models.len() as u16);
        self.w16(h + 10, models.len() as u16);
        self.w64(drawable + 0x50 + lod * 8, sp(h));
    }
    fn shaders(&mut self, drawable: usize, shaders: &[(u32, &[(u32, u64)])]) {
        let arr = self.alloc(shaders.len() * 8);
        for (i, (hash, texs)) in shaders.iter().enumerate() {
            let s = self.alloc(0x40);
            self.w32(s, *hash);
            self.sys[s + 4..s + 8].copy_from_slice(b"atem");
            let slots = self.alloc(texs.len() * 8);
            let table = self.alloc(8 + texs.len() * 8);
            self.sys[table + 1] = texs.len() as u8;
            self.sys[table + 4] = texs.len() as u8;
            for (k, (param, tex)) in texs.iter().enumerate() {
                let st = self.alloc(0x50);
                let r = self.alloc(0x20);
                self.w64(r + 8, *tex);
                self.w64(st + 0x30, sp(r));
                self.w64(slots + k * 8, sp(st));
                self.w32(table + 8 + k * 8, *param);
                self.w32(table + 12 + k * 8, (k * 4) as u32);
            }
            self.w64(s + 0x10, sp(slots));
            self.w64(s + 0x20, sp(table));
            self.w64(arr + i * 8, sp(s));
        }
        let g = self.alloc(0x20);
        self.w64(g + 0x10, sp(arr));
        self.w16(g + 0x18, shaders.len() as u16);
        self.w64(drawable + 0x10, sp(g));
    }
    /// Skeleton struct with header at +0x20; bones as (name, parent, translation).
    fn skeleton(&mut self, drawable: usize, bones: &[(&str, Option<u16>, [f32; 3])]) {
        let names: Vec<usize> = bones.iter().map(|b| self.string(b.0)).collect();
        let ba = self.alloc(bones.len() * 0x50);
        let ibm = self.alloc(bones.len() * 64);
        let par = self.alloc(bones.len() * 2);
        for (i, (name, parent, t)) in bones.iter().enumerate() {
            let r = ba + i * 0x50;
            self.wf(r, &[0.0, 0.0, 0.0, 1.0]);
            self.wf(r + 0x10, t);
            self.w32(r + 0x1C, joaat_lower(name).0);
            self.wf(r + 0x20, &[1.0, 1.0, 1.0]);
            self.w64(r + 0x30, sp(names[i]));
            self.w16(r + 0x44, 0xFFFF);
            self.w16(r + 0x46, parent.unwrap_or(0xFFFF));
            self.w16(r + 0x48, i as u16);
            // Row-vector inverse bind of a pure translation.
            self.wf(
                ibm + i * 64,
                &[
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, -t[0], -t[1],
                    -t[2], 1.0,
                ],
            );
            self.w16(par + i * 2, parent.unwrap_or(0xFFFF));
        }
        let skel = self.alloc(0x60);
        self.w64(skel + 0x20, sp(ba));
        self.w64(skel + 0x28, sp(ibm));
        self.w64(skel + 0x38, sp(par));
        self.w64(drawable + 0x18, sp(skel));
    }
    fn pages(&self) -> ResourcePages<'_> {
        ResourcePages {
            system: &self.sys,
            graphics: &self.gfx,
        }
    }
}

const BASE: [f32; 3] = [-1.0, -1.0, -1.0];
const STEP: [f32; 3] = [2.0 / 16_777_215.0, 2.0 / 16_777_215.0, 2.0 / 65_535.0];

/// A quad in the z = 0 plane facing +z, with normals and one-bone skin.
fn quad_blob(axis_mode: u32, joint: u8) -> (Vec<u8>, Vec<[f32; 3]>) {
    let pts = [
        [-0.5f32, -0.5, 0.0],
        [0.5, -0.5, 0.0],
        [0.5, 0.5, 0.0],
        [-0.5, 0.5, 0.0],
    ];
    let order = crate::vertex::axis_order_for_mode(axis_mode).unwrap_or([0, 1, 2]);
    let words: Vec<u64> = pts
        .iter()
        .map(|p| {
            // slot[i] = model[order[i]]
            let s = [p[order[0]], p[order[1]], p[order[2]]];
            let q =
                |i: usize, max: f32| ((s[i] - BASE[i]) / STEP[i]).round().clamp(0.0, max) as u32;
            quantize_position_word(
                q(0, 16_777_215.0),
                q(1, 16_777_215.0),
                q(2, 65_535.0) as u16,
            )
        })
        .collect();
    // Octahedral +z normal: x = y = 0 -> 511.5 rounds to 512.
    let n = 512u32 | (512 << 10);
    let blob = BlobBuilder::quantized(BASE, STEP, axis_mode, &words)
        .stream(2, format::OCT_NORMAL, n.to_le_bytes().repeat(4))
        .stream(3, format::S16X2, vec![0; 16])
        .stream(8, format::JOINT_SINGLE, vec![joint; 4])
        .build();
    (blob, pts.to_vec())
}

/// Drawable with two LODs (LOD1 is listed in the LOD group but placed
/// earlier in the file than LOD0 to make sure file order is not used),
/// one shader with a texture, a two-bone skeleton and a name.
fn build_drawable() -> Res {
    let mut r = Res::default();
    let d = r.alloc(0xB0);
    let (blob1, _) = quad_blob(2, 0);
    let g1 = r.geometry(&blob1, &[0, 1, 2]);
    let m1 = r.model(&[g1], &[0], &[]);
    let (blob0, _) = quad_blob(2, 1);
    // Second triangle wound clockwise: must be flipped to face +z.
    let g0 = r.geometry(&blob0, &[0, 1, 2, 0, 3, 2]);
    let m0 = r.model(&[g0], &[0], &[]);
    r.lod(d, 0, &[m0]);
    r.lod(d, 1, &[m1]);
    r.shaders(
        d,
        &[(
            joaat("standard").0,
            &[(joaat("diffusetex").0, 0x1122_3344_5566_7788)],
        )],
    );
    r.skeleton(
        d,
        &[
            ("root", None, [0.0; 3]),
            ("door_dside_f", Some(0), [1.0, 0.0, 0.0]),
        ],
    );
    let name = r.string("test_prop.#dr");
    r.w64(d + 0xA8, sp(name));
    r
}

#[test]
fn decodes_synthetic_drawable() {
    let r = build_drawable();
    let dm = decode_drawable_pages(r.pages()).unwrap();
    let m = &dm.model;
    assert_eq!(m.name.as_deref(), Some("test_prop"));
    assert_eq!(m.lods.len(), 2);
    assert_eq!(m.lods[0].meshes[0].indices.len(), 6);
    assert_eq!(m.lods[1].meshes[0].indices.len(), 3);
    let mesh = &m.lods[0].meshes[0];
    assert_eq!(mesh.indices, vec![0, 1, 2, 0, 2, 3]);
    assert!((mesh.positions[2][0] - 0.5).abs() < 1e-5);
    assert_eq!(mesh.material, Some(0));
    assert_eq!(mesh.joints[0], [1, 0, 0, 0]);
    assert_eq!(m.materials.len(), 1);
    assert_eq!(m.materials[0].shader, joaat("standard"));
    assert_eq!(
        m.materials[0].textures,
        vec![(joaat("diffusetex"), 0x1122_3344_5566_7788)]
    );
    let skel = m.skeleton.as_ref().unwrap();
    assert_eq!(skel.bones.len(), 2);
    assert_eq!(skel.bones[1].name.as_deref(), Some("door_dside_f"));
    assert_eq!(skel.bones[1].parent, Some(0));
    assert_eq!(skel.bones[1].inverse_bind[3], [-1.0, 0.0, 0.0, 1.0]);
    assert!(dm.provisional);
    // The decoded model exports.
    let glb = crate::glb::export_glb(m, 0, &[]).unwrap();
    assert_eq!(&glb[0..4], b"glTF");
}

#[test]
fn concatenated_bytes_infer_the_graphics_start() {
    let r = build_drawable();
    let mut bytes = r.sys.clone();
    bytes.resize(bytes.len().next_multiple_of(0x1000) + 0x30, 0);
    let split = bytes.len();
    bytes.extend_from_slice(&r.gfx);
    let dm = decode_drawable(&bytes).unwrap();
    assert!(dm.provisional);
    assert!(
        dm.warnings
            .iter()
            .any(|w| w.contains(&format!("0x{split:X}"))),
        "{:?}",
        dm.warnings
    );
    assert_eq!(dm.model.lods[0].meshes[0].indices, vec![0, 1, 2, 0, 2, 3]);
}

#[test]
fn fragment_root_points_to_drawable() {
    let mut r = Res::default();
    let root = r.alloc(0x80);
    let d = r.alloc(0xB0);
    r.w64(root + 0x18, sp(d));
    let (blob, _) = quad_blob(2, 0);
    let g = r.geometry(&blob, &[0, 1, 2]);
    let m = r.model(&[g], &[0], &[]);
    r.lod(d, 0, &[m]);
    let s = r.string("pole_01.#ft");
    let _ = s;
    let dm = decode_drawable_pages(r.pages()).unwrap();
    assert_eq!(dm.model.lods.len(), 1);
    // Name found by the fallback scan.
    assert_eq!(dm.model.name.as_deref(), Some("pole_01"));
}

#[test]
fn axis_modes_and_unknown_mode_scoring() {
    for mode in [0u32, 1, 2, 9] {
        let mut r = Res::default();
        let d = r.alloc(0xB0);
        let (blob, pts) = quad_blob(mode, 0);
        let g = r.geometry(&blob, &[0, 1, 2, 0, 2, 3]);
        let m = r.model(&[g], &[0], &[]);
        r.lod(d, 0, &[m]);
        let dm = decode_drawable_pages(r.pages()).unwrap();
        let mesh = &dm.model.lods[0].meshes[0];
        if mode == 9 {
            // Unknown mode: slots are identity-encoded here, so the scorer
            // must keep the z-flat orientation the normals point along.
            assert!(dm.warnings.iter().any(|w| w.contains("scoring")));
            assert!(mesh.positions.iter().all(|p| p[2].abs() < 1e-4));
        } else {
            for (got, want) in mesh.positions.iter().zip(&pts) {
                for i in 0..3 {
                    assert!(
                        (got[i] - want[i]).abs() < 1e-4,
                        "mode {mode}: {got:?} vs {want:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn ped_palette_remaps_joints() {
    let mut r = Res::default();
    let d = r.alloc(0xB0);
    let (blob, _) = quad_blob(2, 1); // palette entry 1
    let g = r.geometry(&blob, &[0, 1, 2]);
    let mut pal = [joaat_lower("spine").0, joaat_lower("head").0];
    pal.sort_unstable();
    let m = r.model(&[g], &[0], &pal);
    r.lod(d, 0, &[m]);
    r.skeleton(
        d,
        &[
            ("root", None, [0.0; 3]),
            ("head", Some(0), [0.0; 3]),
            ("spine", Some(0), [0.0; 3]),
        ],
    );
    let dm = decode_drawable_pages(r.pages()).unwrap();
    let want = if pal[1] == joaat_lower("head").0 {
        1
    } else {
        2
    };
    assert_eq!(dm.model.lods[0].meshes[0].joints[0][0], want);
}

#[test]
fn skeleton_found_by_scan_without_pointer() {
    let mut r = build_drawable();
    // Clear the drawable +0x18 pointer: the scan must still find it.
    r.w64(0x18, 0);
    let dm = decode_drawable_pages(r.pages()).unwrap();
    assert_eq!(dm.model.skeleton.as_ref().unwrap().bones.len(), 2);
    assert!(dm.warnings.iter().any(|w| w.contains("scanning")));
}

#[test]
fn orient_flips_clockwise_triangles() {
    let p = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let n = [[0.0, 0.0, 1.0]; 3];
    let mut idx = vec![0, 2, 1];
    assert_eq!(orient_triangles(&p, &n, &mut idx), 1);
    assert_eq!(idx, vec![0, 1, 2]);
    assert_eq!(orient_triangles(&p, &n, &mut idx), 0);
}

#[test]
fn malformed_resources_error_without_panic() {
    assert!(decode_drawable(&[]).is_err());
    assert!(decode_drawable(&[0u8; 0x200]).is_err());
    let r = build_drawable();
    // Truncated system page.
    for cut in (0..r.sys.len()).step_by(7) {
        let _ = decode_drawable_pages(ResourcePages {
            system: &r.sys[..cut],
            graphics: &r.gfx,
        });
    }
    // Truncated graphics page.
    for cut in (0..r.gfx.len()).step_by(5) {
        let _ = decode_drawable_pages(ResourcePages {
            system: &r.sys,
            graphics: &r.gfx[..cut],
        });
    }
    // Empty graphics page: geometry fails, so the whole decode errors.
    let e = decode_drawable_pages(ResourcePages {
        system: &r.sys,
        graphics: &[],
    })
    .unwrap_err();
    assert!(e.to_string().contains("no geometry decoded"), "{e}");
    // Random corruption of the system page.
    let mut seed = 5;
    for _ in 0..300 {
        let mut sys = r.sys.clone();
        for _ in 0..8 {
            let i = lcg(&mut seed) as usize % sys.len();
            sys[i] = lcg(&mut seed) as u8;
        }
        let _ = decode_drawable_pages(ResourcePages {
            system: &sys,
            graphics: &r.gfx,
        });
    }
    // Pointers with huge counts must hit caps, not allocate.
    let mut r2 = build_drawable();
    let h = r2.sys.len();
    r2.alloc(16);
    r2.w64(h, sp(0));
    r2.w16(h + 8, u16::MAX);
    r2.w64(0x50, sp(h));
    let _ = decode_drawable_pages(r2.pages());
}
