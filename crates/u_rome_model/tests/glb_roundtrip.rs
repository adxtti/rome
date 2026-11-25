//! Export a synthetic skinned cube and re-parse it with the `gltf` crate.

// Small matrix helpers read more clearly with index loops.
#![allow(clippy::needless_range_loop)]

use u_rome_core::{
    Bone, Image, ImageFormat, Material, Mesh, Model, ModelLod, Skeleton, joaat, joaat_lower,
};
use u_rome_model::glb::{ColorExport, GlbOptions, export_glb, export_glb_with};

type M4 = [[f32; 4]; 4];

fn mul(a: &M4, b: &M4) -> M4 {
    let mut o = [[0f32; 4]; 4];
    for r in 0..4 {
        for c in 0..4 {
            o[r][c] = (0..4).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    o
}

fn transpose(a: &M4) -> M4 {
    let mut o = [[0f32; 4]; 4];
    for r in 0..4 {
        for c in 0..4 {
            o[r][c] = a[c][r];
        }
    }
    o
}

/// Column-vector rigid transform from quaternion (x, y, z, w) + translation.
fn rigid(q: [f32; 4], t: [f32; 3]) -> M4 {
    let [x, y, z, w] = q;
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
            t[0],
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
            t[1],
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
            t[2],
        ],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// Inverse of a column-vector rigid transform.
fn rigid_inverse(m: &M4) -> M4 {
    let mut o = [[0f32; 4]; 4];
    for r in 0..3 {
        for c in 0..3 {
            o[r][c] = m[c][r];
        }
    }
    for r in 0..3 {
        o[r][3] = -(0..3).map(|k| o[r][k] * m[k][3]).sum::<f32>();
    }
    o[3][3] = 1.0;
    o
}

fn skinned_cube() -> (Model, Image) {
    let s = std::f32::consts::FRAC_1_SQRT_2;
    // RAGE bones: root at origin, child 1 m up (Z) rotated 90 degrees about Z.
    let root_local = ([0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0]);
    let child_local = ([0.0, 0.0, s, s], [0.0, 0.0, 1.0]);
    let root_world = rigid(root_local.0, root_local.1);
    let child_world = mul(&root_world, &rigid(child_local.0, child_local.1));
    let bone = |name: &str, parent: Option<u16>, local: ([f32; 4], [f32; 3]), world: &M4| Bone {
        name: Some(name.into()),
        name_hash: joaat_lower(name),
        parent,
        rotation: local.0,
        translation: local.1,
        scale: [1.0, 1.0, 1.0],
        // Row-vector convention: transpose of the column-vector inverse.
        inverse_bind: transpose(&rigid_inverse(world)),
    };
    let skeleton = Skeleton {
        bones: vec![
            bone("SKEL_ROOT", None, root_local, &root_world),
            bone("spine-1", Some(0), child_local, &child_world),
        ],
    };

    let mut mesh = Mesh::default();
    for z in [0.0f32, 2.0] {
        for y in [-0.5f32, 0.5] {
            for x in [-0.5f32, 0.5] {
                mesh.positions.push([x, y, z]);
                let n = [x, y, z - 1.0];
                let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                mesh.normals.push([n[0] / l, n[1] / l, n[2] / l]);
                mesh.uvs.push([x + 0.5, y + 0.5]);
                mesh.colors.push([(x * 100.0 + 100.0) as u8, 7, 3, 255]);
                if z == 0.0 {
                    mesh.joints.push([0, 0, 0, 0]);
                    mesh.weights.push([1.0, 0.0, 0.0, 0.0]);
                } else {
                    mesh.joints.push([1, 0, 0, 0]);
                    mesh.weights.push([0.75, 0.25, 0.0, 0.0]);
                }
            }
        }
    }
    // 12 triangles of a box (vertex i = x + 2y + 4z).
    mesh.indices = vec![
        0, 2, 1, 1, 2, 3, 4, 5, 6, 5, 7, 6, 0, 1, 4, 1, 5, 4, 2, 6, 3, 3, 6, 7, 0, 4, 2, 2, 4, 6,
        1, 3, 5, 3, 7, 5,
    ];
    mesh.material = Some(0);

    let img = Image {
        width: 2,
        height: 2,
        format: ImageFormat::Rgba8Srgb,
        mips: vec![(0u8..16).collect(), vec![1, 2, 3, 4]],
        provisional: false,
    };
    let model = Model {
        name: Some("cube".into()),
        lods: vec![ModelLod {
            meshes: vec![mesh, Mesh::default()],
        }],
        materials: vec![Material {
            shader: joaat("standard"),
            shader_name: Some("standard".into()),
            textures: vec![],
        }],
        skeleton: Some(skeleton),
    };
    (model, img)
}

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

#[test]
fn skinned_cube_round_trip() {
    let (model, img) = skinned_cube();
    let glb = export_glb(&model, 0, &[(0, img)]).unwrap();
    let gltf = gltf::Gltf::from_slice(&glb).expect("valid glb");
    let blob = gltf.blob.clone().expect("BIN chunk");
    let doc = &gltf.document;

    // One mesh (the empty one is skipped), one skin, one material + texture.
    assert_eq!(doc.meshes().count(), 1);
    assert_eq!(doc.skins().count(), 1);
    assert_eq!(doc.materials().count(), 1);
    assert_eq!(doc.textures().count(), 1);

    let mesh = doc.meshes().next().unwrap();
    let prim = mesh.primitives().next().unwrap();
    assert_eq!(prim.mode(), gltf::mesh::Mode::Triangles);
    let reader = prim.reader(|_| Some(&blob[..]));
    let src = &model.lods[0].meshes[0];

    let pos: Vec<[f32; 3]> = reader.read_positions().unwrap().collect();
    assert_eq!(pos.len(), 8);
    for (g, r) in pos.iter().zip(&src.positions) {
        assert_eq!(*g, [r[0], r[2], -r[1]]);
    }
    let nrm: Vec<[f32; 3]> = reader.read_normals().unwrap().collect();
    for (g, r) in nrm.iter().zip(&src.normals) {
        assert!(approx(g[0], r[0]) && approx(g[1], r[2]) && approx(g[2], -r[1]));
    }
    let uv: Vec<[f32; 2]> = reader.read_tex_coords(0).unwrap().into_f32().collect();
    assert_eq!(uv, src.uvs);
    let col: Vec<[u8; 4]> = reader.read_colors(0).unwrap().into_rgba_u8().collect();
    assert_eq!(col, src.colors);
    let joints: Vec<[u16; 4]> = reader.read_joints(0).unwrap().into_u16().collect();
    let weights: Vec<[f32; 4]> = reader.read_weights(0).unwrap().into_f32().collect();
    assert_eq!(joints[7], [1, 0, 0, 0]);
    assert_eq!(weights[7], [0.75, 0.25, 0.0, 0.0]);
    let idx: Vec<u32> = reader.read_indices().unwrap().into_u32().collect();
    assert_eq!(idx, src.indices);

    // Winding is preserved by the axis change: the face normal of each
    // triangle still points away from the cube centre (0, 1, 0) in glTF.
    for t in idx.chunks(3) {
        let [a, b, c] = [pos[t[0] as usize], pos[t[1] as usize], pos[t[2] as usize]];
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let centre = [
            (a[0] + b[0] + c[0]) / 3.0,
            (a[1] + b[1] + c[1]) / 3.0 - 1.0,
            (a[2] + b[2] + c[2]) / 3.0,
        ];
        let d = n[0] * centre[0] + n[1] * centre[1] + n[2] * centre[2];
        assert!(d > 0.0, "triangle {t:?} faces inward");
    }

    // Skin: joints are both bones plus the armature; IBM x joint world = I.
    let skin = doc.skins().next().unwrap();
    let joint_nodes: Vec<gltf::Node> = skin.joints().collect();
    assert_eq!(joint_nodes.len(), 3);
    assert_eq!(joint_nodes[0].name(), Some("SKEL_ROOT"));
    assert_eq!(joint_nodes[1].name(), Some("spine-1"));
    let ibms: Vec<[[f32; 4]; 4]> = skin
        .reader(|_| Some(&blob[..]))
        .read_inverse_bind_matrices()
        .unwrap()
        .collect();
    // World matrices from the node hierarchy (gltf matrices are column-major,
    // i.e. m[col][row]).
    let mut parent = vec![None; doc.nodes().count()];
    for n in doc.nodes() {
        for c in n.children() {
            parent[c.index()] = Some(n.index());
        }
    }
    let local = |i: usize| transpose(&doc.nodes().nth(i).unwrap().transform().matrix());
    let world = |mut i: usize| {
        let mut m = local(i);
        while let Some(p) = parent[i] {
            m = mul(&local(p), &m);
            i = p;
        }
        m
    };
    for (j, ibm) in joint_nodes.iter().zip(&ibms) {
        let prod = mul(&world(j.index()), &transpose(ibm));
        for r in 0..4 {
            for c in 0..4 {
                let want = if r == c { 1.0 } else { 0.0 };
                assert!(approx(prod[r][c], want), "joint {:?}: {prod:?}", j.name());
            }
        }
    }
    // The child bone sits 1 m up: glTF +Y.
    let child_world = world(joint_nodes[1].index());
    assert!(approx(child_world[1][3], 1.0));

    // Embedded PNG decodes back to the top mip.
    let image = doc.images().next().unwrap();
    let gltf::image::Source::View { view, mime_type } = image.source() else {
        panic!("image not embedded");
    };
    assert_eq!(mime_type, "image/png");
    let png_bytes = &blob[view.offset()..view.offset() + view.length()];
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .unwrap();
    let mut buf = vec![0; dec.output_buffer_size().unwrap()];
    let info = dec.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (2, 2));
    assert_eq!(
        &buf[..info.buffer_size()],
        &(0u8..16).collect::<Vec<_>>()[..]
    );
}

#[test]
fn options_and_u16_joints() {
    let (mut model, _) = skinned_cube();
    // 300 bones forces u16 joints.
    let skel = model.skeleton.as_mut().unwrap();
    let template = skel.bones[1].clone();
    while skel.bones.len() < 300 {
        let mut b = template.clone();
        b.parent = Some(0);
        b.name = Some(format!("b{}", skel.bones.len()));
        skel.bones.push(b);
    }
    model.lods[0].meshes[0].joints[7] = [299, 0, 0, 0];
    // And one joint past the skeleton, which must be dropped.
    model.lods[0].meshes[0].joints[6] = [400, 0, 0, 0];
    model.lods[0].meshes[0].weights[6] = [1.0, 0.0, 0.0, 0.0];
    let opts = GlbOptions {
        colors: ColorExport::Custom,
        skin: true,
    };
    let glb = export_glb_with(&model, 0, &[], &opts).unwrap();
    let gltf = gltf::Gltf::from_slice(&glb).unwrap();
    let blob = gltf.blob.clone().unwrap();
    let prim = gltf
        .document
        .meshes()
        .next()
        .unwrap()
        .primitives()
        .next()
        .unwrap();
    let reader = prim.reader(|_| Some(&blob[..]));
    assert!(reader.read_colors(0).is_none());
    assert!(
        prim.attributes()
            .any(|(s, _)| matches!(s, gltf::Semantic::Extras(ref n) if n == "RAGE_COLOR"))
    );
    let joints: Vec<[u16; 4]> = reader.read_joints(0).unwrap().into_u16().collect();
    assert_eq!(joints[7], [299, 0, 0, 0]);
    // Joint 400 had all the weight; it falls back to the armature joint.
    assert_eq!(joints[6], [300, 0, 0, 0]);
    assert_eq!(gltf.document.skins().next().unwrap().joints().count(), 301);

    let opts = GlbOptions {
        colors: ColorExport::None,
        skin: false,
    };
    let glb = export_glb_with(&model, 0, &[], &opts).unwrap();
    let gltf = gltf::Gltf::from_slice(&glb).unwrap();
    assert_eq!(gltf.document.skins().count(), 0);
    // Bones are still exported as nodes.
    assert!(gltf.document.nodes().count() > 300);
}
