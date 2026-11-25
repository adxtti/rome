//! GPU-free preparation of decoded models for rendering: vertex
//! interleaving, index validation, normal generation, skeleton lines and the
//! ground grid. Everything here treats the input as untrusted.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Quat, Vec3};
use u_rome_core::{Mesh, Skeleton};

use crate::camera::Aabb;

/// Highest vertex count accepted per mesh. Without
/// `DownlevelFlags::FULL_DRAW_INDEX_UINT32` (WebGL2-class GL) index values are
/// limited to 2^24 - 1, so meshes above that are rejected rather than drawn
/// with corrupted indices.
pub const MAX_MESH_VERTICES: usize = (1 << 24) - 1;
/// Highest index count accepted per mesh (about 21M triangles).
pub const MAX_MESH_INDICES: usize = 1 << 26;
/// Highest triangle count expanded for the barycentric wireframe fallback
/// (16 bytes per expanded vertex, so at most 96 MiB per mesh).
pub const MAX_WIRE_TRIANGLES: usize = 2_000_000;

/// Interleaved vertex uploaded to the GPU (36 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct GpuVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}

/// Expanded vertex for the barycentric wireframe fallback (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct WireVertex {
    pub position: [f32; 3],
    pub bary: [u8; 4],
}

/// Coloured line vertex (16 bytes), used for grid, axes and skeleton.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct LineVertex {
    pub position: [f32; 3],
    pub color: [u8; 4],
}

/// A mesh validated and converted for upload.
#[derive(Clone, Debug, Default)]
pub struct PreparedMesh {
    pub vertices: Vec<GpuVertex>,
    pub indices: Vec<u32>,
    pub material: Option<usize>,
    /// Bounds of the finite positions.
    pub bounds: Option<Aabb>,
    /// Human-readable problems found while preparing, already logged.
    pub issues: Vec<String>,
}

impl PreparedMesh {
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Validate and interleave one mesh. `label` identifies it in diagnostics
/// (for example `"lod 0 mesh 3"`).
pub fn prepare_mesh(mesh: &Mesh, label: &str) -> PreparedMesh {
    let mut out = PreparedMesh {
        material: mesh.material,
        ..Default::default()
    };
    let mut issue = |msg: String| {
        log::warn!("render: {label}: {msg}");
        out.issues.push(format!("{label}: {msg}"));
    };

    let n = mesh.positions.len();
    if n == 0 {
        issue("no vertices, skipped".into());
        return out;
    }
    if n > MAX_MESH_VERTICES {
        issue(format!(
            "{n} vertices exceeds the limit of {MAX_MESH_VERTICES}, skipped"
        ));
        return out;
    }
    if mesh.indices.len() > MAX_MESH_INDICES {
        issue(format!(
            "{} indices exceeds the limit of {MAX_MESH_INDICES}, skipped",
            mesh.indices.len()
        ));
        return out;
    }

    // Indices: drop triangles that reference missing vertices.
    let indices: Vec<u32> = if mesh.indices.is_empty() {
        if !n.is_multiple_of(3) {
            issue(format!(
                "no indices and {n} vertices is not a multiple of 3, trailing vertices ignored"
            ));
        } else {
            log::debug!("render: {label}: no indices, drawing as a plain triangle list");
        }
        (0..(n - n % 3) as u32).collect()
    } else {
        let (tris, rest) = mesh.indices.as_chunks::<3>();
        if !rest.is_empty() {
            issue(format!(
                "index count {} is not a multiple of 3, last {} ignored",
                mesh.indices.len(),
                rest.len()
            ));
        }
        let mut kept = Vec::with_capacity(tris.len() * 3);
        let mut dropped = 0usize;
        let mut first_bad: Option<(usize, u32)> = None;
        for (t, tri) in tris.iter().enumerate() {
            if let Some(&bad) = tri.iter().find(|&&i| i as usize >= n) {
                dropped += 1;
                first_bad.get_or_insert((t * 3, bad));
            } else {
                kept.extend_from_slice(tri);
            }
        }
        if let Some((at, bad)) = first_bad {
            issue(format!(
                "{dropped} triangles reference vertices out of range (first: index {bad} at index offset {at}, vertex count {n}), dropped"
            ));
        }
        kept
    };

    let normals: Option<&[[f32; 3]]> = match mesh.normals.len() {
        len if len == n => Some(&mesh.normals),
        0 => None,
        len => {
            issue(format!(
                "{len} normals for {n} vertices, generating normals"
            ));
            None
        }
    };
    let generated;
    let normals = match normals {
        Some(n) => n,
        None => {
            generated = generate_normals(&mesh.positions, &indices);
            &generated[..]
        }
    };
    let uvs = match mesh.uvs.len() {
        len if len == n => Some(&mesh.uvs[..]),
        0 => None,
        len => {
            issue(format!("{len} uvs for {n} vertices, ignoring uvs"));
            None
        }
    };
    let colors = match mesh.colors.len() {
        len if len == n => Some(&mesh.colors[..]),
        0 => None,
        len => {
            issue(format!("{len} colours for {n} vertices, ignoring colours"));
            None
        }
    };

    out.vertices = (0..n)
        .map(|i| GpuVertex {
            position: mesh.positions.get(i).copied().unwrap_or_default(),
            normal: normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]),
            uv: uvs.and_then(|u| u.get(i)).copied().unwrap_or_default(),
            color: colors.and_then(|c| c.get(i)).copied().unwrap_or([255; 4]),
        })
        .collect();
    out.indices = indices;
    out.bounds = Aabb::from_points(mesh.positions.iter());
    out
}

/// Area-weighted smooth normals. Degenerate or unreferenced vertices get +Z.
pub fn generate_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    let get = |i: u32| positions.get(i as usize).map(|p| Vec3::from_array(*p));
    for &[i0, i1, i2] in indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (get(i0), get(i1), get(i2)) else {
            continue;
        };
        let face = (b - a).cross(c - a);
        if !face.is_finite() {
            continue;
        }
        for i in [i0, i1, i2] {
            if let Some(slot) = acc.get_mut(i as usize) {
                *slot += face;
            }
        }
    }
    acc.into_iter()
        .map(|n| n.try_normalize().unwrap_or(Vec3::Z).to_array())
        .collect()
}

/// Expand an indexed mesh into a non-indexed list with barycentric
/// coordinates for the wireframe fallback. Returns `None` above
/// [`MAX_WIRE_TRIANGLES`].
pub fn expand_wireframe(positions: &[[f32; 3]], indices: &[u32]) -> Option<Vec<WireVertex>> {
    let (tris, _) = indices.as_chunks::<3>();
    if tris.len() > MAX_WIRE_TRIANGLES {
        return None;
    }
    const BARY: [[u8; 4]; 3] = [[255, 0, 0, 0], [0, 255, 0, 0], [0, 0, 255, 0]];
    let mut out = Vec::with_capacity(tris.len() * 3);
    for tri in tris {
        let pts: Option<Vec<[f32; 3]>> = tri
            .iter()
            .map(|&i| positions.get(i as usize).copied())
            .collect();
        let Some(pts) = pts else { continue };
        for (p, b) in pts.into_iter().zip(BARY) {
            out.push(WireVertex {
                position: p,
                bary: b,
            });
        }
    }
    Some(out)
}

/// How bone positions are derived for the skeleton overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SkeletonSource {
    /// Compose local translation/rotation/scale down the parent chain.
    #[default]
    LocalTransforms,
    /// Invert each bone's inverse bind matrix.
    InverseBind,
}

/// The skeleton overlay is provisional: `u_rome_core::Bone::rotation`
/// component order is assumed to be `[x, y, z, w]` (glam order, and the
/// order GTA V `crBoneData` uses per CodeWalker's MIT-licensed YDR notes);
/// this has not been confirmed for GTA VI data.
pub const SKELETON_PROVISIONAL: bool = true;

/// Model-space bone origins. `None` for bones whose transform is not finite.
pub fn bone_positions(skel: &Skeleton, source: SkeletonSource) -> Vec<Option<Vec3>> {
    match source {
        SkeletonSource::InverseBind => skel
            .bones
            .iter()
            .map(|b| {
                // Row-vector convention (`p_bone = p_model * M`): the
                // column-major glam matrix built from M's rows is M^T, which
                // is the same transform in column-vector form.
                let inv = Mat4::from_cols_array_2d(&b.inverse_bind);
                let bind = inv.inverse();
                let p = bind.w_axis.truncate();
                (bind.is_finite() && p.is_finite()).then_some(p)
            })
            .collect(),
        SkeletonSource::LocalTransforms => {
            let world = bone_world_matrices(skel);
            world
                .iter()
                .map(|m| {
                    let p = m.w_axis.truncate();
                    p.is_finite().then_some(p)
                })
                .collect()
        }
    }
}

fn local_matrix(b: &u_rome_core::Bone) -> Mat4 {
    let q = Quat::from_array(b.rotation);
    let q = if q.is_finite() && q.length_squared() > 1e-12 {
        q.normalize()
    } else {
        Quat::IDENTITY
    };
    let s = Vec3::from_array(b.scale);
    let s = if s.is_finite() && s != Vec3::ZERO {
        s
    } else {
        Vec3::ONE
    };
    Mat4::from_scale_rotation_translation(s, q, Vec3::from_array(b.translation))
}

/// World matrices from the local transform hierarchy. Bones with a missing,
/// self-referencing or cyclic parent are treated as roots.
pub fn bone_world_matrices(skel: &Skeleton) -> Vec<Mat4> {
    let n = skel.bones.len();
    let mut world: Vec<Option<Mat4>> = vec![None; n];
    let mut chain = Vec::new();
    for start in 0..n {
        if world.get(start).copied().flatten().is_some() {
            continue;
        }
        // Walk up until a resolved bone or a root, bounded by `n` steps.
        chain.clear();
        let mut cur = start;
        let mut base = Mat4::IDENTITY;
        loop {
            chain.push(cur);
            let parent = skel.bones.get(cur).and_then(|b| b.parent).map(usize::from);
            match parent {
                Some(p) if p < n && p != cur && !chain.contains(&p) && chain.len() <= n => {
                    if let Some(m) = world.get(p).copied().flatten() {
                        base = m;
                        break;
                    }
                    cur = p;
                }
                Some(p) => {
                    log::debug!("render: bone {cur} has invalid parent {p}, treated as root");
                    break;
                }
                None => break,
            }
        }
        for &i in chain.iter().rev() {
            if let Some(b) = skel.bones.get(i) {
                base *= local_matrix(b);
            }
            if let Some(slot) = world.get_mut(i) {
                *slot = Some(base);
            }
        }
    }
    world
        .into_iter()
        .map(|m| m.unwrap_or(Mat4::IDENTITY))
        .collect()
}

/// Line list for the skeleton overlay: parent-to-child segments plus a small
/// cross at each root so isolated bones remain visible.
pub fn skeleton_lines(
    skel: &Skeleton,
    source: SkeletonSource,
    marker_size: f32,
    color: [u8; 4],
) -> Vec<LineVertex> {
    let pos = bone_positions(skel, source);
    let mut out = Vec::new();
    let mut push = |a: Vec3, b: Vec3| {
        out.push(LineVertex {
            position: a.to_array(),
            color,
        });
        out.push(LineVertex {
            position: b.to_array(),
            color,
        });
    };
    for (i, bone) in skel.bones.iter().enumerate() {
        let Some(Some(p)) = pos.get(i).copied() else {
            continue;
        };
        match bone
            .parent
            .and_then(|pi| pos.get(usize::from(pi)).copied().flatten())
        {
            Some(pp) if bone.parent.is_some_and(|pi| usize::from(pi) != i) => push(pp, p),
            _ => {
                for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                    push(p - axis * marker_size, p + axis * marker_size);
                }
            }
        }
    }
    out
}

/// Pick a power-of-ten grid spacing for a scene of `radius`.
pub fn grid_spacing(radius: f32) -> f32 {
    let r = if radius.is_finite() && radius > 0.0 {
        radius
    } else {
        1.0
    };
    10f32.powf((r / 2.0).log10().floor()).clamp(1e-3, 1e5)
}

/// Ground grid on the Z = 0 plane covering roughly `radius` around the
/// origin, plus coloured world axes (X red, Y green, Z blue).
pub fn grid_lines(radius: f32) -> Vec<LineVertex> {
    const MINOR: [u8; 4] = [70, 70, 74, 255];
    const MAJOR: [u8; 4] = [105, 105, 110, 255];
    const X_AXIS: [u8; 4] = [200, 60, 60, 255];
    const Y_AXIS: [u8; 4] = [60, 180, 60, 255];
    const Z_AXIS: [u8; 4] = [70, 110, 220, 255];
    let spacing = grid_spacing(radius);
    // At most 2 * 40 + 1 lines per direction.
    let half_count: i32 = 40
        .min((radius.max(spacing) * 2.0 / spacing).ceil() as i32)
        .max(1);
    let ext = spacing * half_count as f32;
    let mut out = Vec::with_capacity(((half_count * 2 + 1) * 4 + 2) as usize);
    let mut line = |a: [f32; 3], b: [f32; 3], color: [u8; 4]| {
        out.push(LineVertex { position: a, color });
        out.push(LineVertex { position: b, color });
    };
    for i in -half_count..=half_count {
        let o = i as f32 * spacing;
        let base = if i % 10 == 0 { MAJOR } else { MINOR };
        let (cx, cy) = if i == 0 {
            (Y_AXIS, X_AXIS)
        } else {
            (base, base)
        };
        // Line along Y at x = o (x = 0 is the Y axis) and along X at y = o.
        line([o, -ext, 0.0], [o, ext, 0.0], cx);
        line([-ext, o, 0.0], [ext, o, 0.0], cy);
    }
    line([0.0, 0.0, 0.0], [0.0, 0.0, spacing * 2.0], Z_AXIS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::Bone;

    fn quad() -> Mesh {
        Mesh {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..Default::default()
        }
    }

    #[test]
    fn generates_normals_and_defaults() {
        let p = prepare_mesh(&quad(), "t");
        assert_eq!(p.vertices.len(), 4);
        assert_eq!(p.triangle_count(), 2);
        for v in &p.vertices {
            assert_eq!(v.normal, [0.0, 0.0, 1.0]);
            assert_eq!(v.color, [255; 4]);
            assert_eq!(v.uv, [0.0, 0.0]);
        }
        assert!(p.issues.is_empty());
        let b = p.bounds.unwrap();
        assert_eq!(b.max, Vec3::new(1.0, 1.0, 0.0));
    }

    #[test]
    fn drops_bad_triangles_and_reports_offset() {
        let mut m = quad();
        m.indices = vec![0, 1, 2, 0, 9, 3, 1, 2];
        let p = prepare_mesh(&m, "lod 0 mesh 1");
        assert_eq!(p.indices, vec![0, 1, 2]);
        assert_eq!(p.issues.len(), 2);
        assert!(
            p.issues
                .iter()
                .any(|s| s.contains("index 9 at index offset 3"))
        );
    }

    #[test]
    fn mismatched_attributes_are_ignored() {
        let mut m = quad();
        m.uvs = vec![[0.5, 0.5]];
        m.colors = vec![[1, 2, 3, 4]; 4];
        m.normals = vec![[1.0, 0.0, 0.0]; 2];
        let p = prepare_mesh(&m, "t");
        assert_eq!(p.vertices[3].uv, [0.0, 0.0]);
        assert_eq!(p.vertices[3].color, [1, 2, 3, 4]);
        assert_eq!(p.vertices[0].normal, [0.0, 0.0, 1.0]);
        assert_eq!(p.issues.len(), 2);
    }

    #[test]
    fn empty_and_unindexed_meshes() {
        let p = prepare_mesh(&Mesh::default(), "t");
        assert!(p.vertices.is_empty() && p.indices.is_empty());
        assert_eq!(p.issues.len(), 1);
        let m = Mesh {
            positions: vec![[0.0; 3]; 7],
            ..Default::default()
        };
        let p = prepare_mesh(&m, "t");
        assert_eq!(p.indices, (0..6).collect::<Vec<u32>>());
    }

    #[test]
    fn wire_expansion() {
        let m = quad();
        let w = expand_wireframe(&m.positions, &[0, 1, 2, 0, 2, 77]).unwrap();
        assert_eq!(w.len(), 3);
        assert_eq!(w[1].bary, [0, 255, 0, 0]);
        assert_eq!(w[2].position, [1.0, 1.0, 0.0]);
    }

    fn bone(parent: Option<u16>, t: [f32; 3]) -> Bone {
        Bone {
            name: None,
            name_hash: Default::default(),
            parent,
            rotation: [0.0, 0.0, 0.0, 1.0],
            translation: t,
            scale: [1.0; 3],
            inverse_bind: Mat4::IDENTITY.to_cols_array_2d(),
        }
    }

    #[test]
    fn local_chain_accumulates() {
        let mut b1 = bone(Some(0), [0.0, 0.0, 1.0]);
        // 90 degrees about Z on bone 1: its child offset +X becomes +Y.
        b1.rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2).to_array();
        let skel = Skeleton {
            bones: vec![
                bone(None, [1.0, 0.0, 0.0]),
                b1,
                bone(Some(1), [1.0, 0.0, 0.0]),
            ],
        };
        let p = bone_positions(&skel, SkeletonSource::LocalTransforms);
        let exp = [
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(1.0, 1.0, 1.0),
        ];
        for (a, e) in p.iter().zip(exp) {
            assert!((a.unwrap() - e).length() < 1e-5, "{a:?} vs {e}");
        }
    }

    #[test]
    fn forward_parent_and_cycles_do_not_hang() {
        // Bone 0's parent is 1 (forward reference); 2 and 3 form a cycle;
        // 4 points out of range.
        let skel = Skeleton {
            bones: vec![
                bone(Some(1), [0.0, 0.0, 1.0]),
                bone(None, [1.0, 0.0, 0.0]),
                bone(Some(3), [1.0, 0.0, 0.0]),
                bone(Some(2), [1.0, 0.0, 0.0]),
                bone(Some(900), [5.0, 0.0, 0.0]),
                bone(Some(5), [0.0, 0.0, 0.0]),
            ],
        };
        let p = bone_positions(&skel, SkeletonSource::LocalTransforms);
        assert!((p[0].unwrap() - Vec3::new(1.0, 0.0, 1.0)).length() < 1e-5);
        assert!(p[2].is_some() && p[3].is_some());
        assert!((p[4].unwrap() - Vec3::new(5.0, 0.0, 0.0)).length() < 1e-5);
        let lines = skeleton_lines(&skel, SkeletonSource::LocalTransforms, 0.1, [255; 4]);
        assert!(!lines.is_empty() && lines.len().is_multiple_of(2));
    }

    #[test]
    fn inverse_bind_source() {
        let mut b = bone(None, [0.0; 3]);
        // Inverse bind of a bone at (2, 3, 4): translation by (-2, -3, -4),
        // stored row-vector style (translation in the last row).
        b.inverse_bind = Mat4::from_translation(Vec3::new(-2.0, -3.0, -4.0)).to_cols_array_2d();
        let skel = Skeleton { bones: vec![b] };
        let p = bone_positions(&skel, SkeletonSource::InverseBind);
        assert!((p[0].unwrap() - Vec3::new(2.0, 3.0, 4.0)).length() < 1e-5);
        let mut bad = bone(None, [0.0; 3]);
        bad.inverse_bind = [[f32::NAN; 4]; 4];
        let p = bone_positions(&Skeleton { bones: vec![bad] }, SkeletonSource::InverseBind);
        assert!(p[0].is_none());
    }

    #[test]
    fn grid_is_bounded() {
        for r in [0.0, 0.01, 1.0, 37.0, 1e9, f32::NAN] {
            let g = grid_lines(r);
            assert!(g.len().is_multiple_of(2));
            assert!(g.len() <= (81 * 4 + 2));
            assert!(g.iter().all(|v| v.position.iter().all(|c| c.is_finite())));
        }
        assert_eq!(grid_spacing(2.0), 1.0);
        assert_eq!(grid_spacing(50.0), 10.0);
    }
}
