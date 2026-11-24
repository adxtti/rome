//! GPU-free camera math: orbit camera, bounds framing, picking rays.
//!
//! World space is RAGE convention: right-handed, Z-up, metres. Projection
//! matrices target wgpu clip space (depth 0..1).

use glam::{Mat4, Vec2, Vec3, Vec4Swizzles};

/// Axis-aligned bounding box. Non-finite points are ignored when building.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// Bounds of all finite points, or `None` if there are none.
    pub fn from_points<'a>(points: impl IntoIterator<Item = &'a [f32; 3]>) -> Option<Self> {
        let mut out: Option<Self> = None;
        for p in points {
            let v = Vec3::from_array(*p);
            if !v.is_finite() {
                continue;
            }
            out = Some(match out {
                None => Self { min: v, max: v },
                Some(b) => Self {
                    min: b.min.min(v),
                    max: b.max.max(v),
                },
            });
        }
        out
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    /// Half the diagonal length (radius of the bounding sphere around the box).
    pub fn radius(&self) -> f32 {
        (self.max - self.min).length() * 0.5
    }

    pub fn corners(&self) -> [Vec3; 8] {
        let (a, b) = (self.min, self.max);
        [
            Vec3::new(a.x, a.y, a.z),
            Vec3::new(b.x, a.y, a.z),
            Vec3::new(a.x, b.y, a.z),
            Vec3::new(b.x, b.y, a.z),
            Vec3::new(a.x, a.y, b.z),
            Vec3::new(b.x, a.y, b.z),
            Vec3::new(a.x, b.y, b.z),
            Vec3::new(b.x, b.y, b.z),
        ]
    }
}

/// A ray in world space. `dir` is normalised.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

impl Ray {
    pub fn at(&self, t: f32) -> Vec3 {
        self.origin + self.dir * t
    }
}

const MIN_DISTANCE: f32 = 1e-3;
const MAX_DISTANCE: f32 = 1e6;
const MAX_PITCH: f32 = 89.0_f32 * (std::f32::consts::PI / 180.0);

/// Orbit camera around a target point, Z-up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrbitCamera {
    pub target: Vec3,
    /// Rotation around world Z in radians; 0 puts the eye on +X.
    pub yaw: f32,
    /// Elevation in radians, clamped to +-89 degrees.
    pub pitch: f32,
    pub distance: f32,
    /// Vertical field of view in radians.
    pub fov_y: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: Vec3::ZERO,
            // Front-right three-quarter view. RAGE peds and vehicles face +Y.
            yaw: 60f32.to_radians(),
            pitch: 20f32.to_radians(),
            distance: 5.0,
            fov_y: 45f32.to_radians(),
        }
    }
}

impl OrbitCamera {
    /// Unit vector from target to eye.
    pub fn eye_dir(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(cp * cy, cp * sy, sp)
    }

    pub fn eye(&self) -> Vec3 {
        self.target + self.eye_dir() * self.distance
    }

    /// Unit view direction (eye towards target).
    pub fn forward(&self) -> Vec3 {
        -self.eye_dir()
    }

    /// Camera right vector in world space.
    pub fn right(&self) -> Vec3 {
        let r = self.forward().cross(Vec3::Z);
        if r.length_squared() < 1e-12 {
            Vec3::X
        } else {
            r.normalize()
        }
    }

    /// Camera up vector in world space.
    pub fn up(&self) -> Vec3 {
        self.right().cross(self.forward()).normalize()
    }

    pub fn view(&self) -> Mat4 {
        glam::camera::rh::view::look_at_mat4(self.eye(), self.target, Vec3::Z)
    }

    /// Near and far planes that enclose a scene of `scene_radius` around the
    /// target plus some margin for the ground grid.
    pub fn clip_planes(&self, scene_radius: f32) -> (f32, f32) {
        let r = if scene_radius.is_finite() {
            scene_radius.max(MIN_DISTANCE)
        } else {
            1.0
        };
        let near = (self.distance * 0.002).clamp(1e-4, 10.0);
        let far = ((self.distance + r * 4.0) * 4.0).max(near * 100.0);
        (near, far)
    }

    pub fn projection(&self, aspect: f32, near: f32, far: f32) -> Mat4 {
        let aspect = sanitize_aspect(aspect);
        // wgpu NDC: Z in [0, 1], Y up, which is glam's `directx` convention.
        glam::camera::rh::proj::directx::perspective(self.fov_y, aspect, near, far)
    }

    pub fn view_proj(&self, aspect: f32, scene_radius: f32) -> Mat4 {
        let (near, far) = self.clip_planes(scene_radius);
        self.projection(aspect, near, far) * self.view()
    }

    /// Rotate by a pointer drag in pixels.
    pub fn orbit(&mut self, delta_px: Vec2) {
        if !delta_px.is_finite() {
            return;
        }
        self.yaw = (self.yaw - delta_px.x * 0.008).rem_euclid(std::f32::consts::TAU);
        self.pitch = (self.pitch + delta_px.y * 0.008).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Move the target so the point under the cursor follows a drag of
    /// `delta_px` pixels in a viewport `viewport_h_px` pixels tall.
    pub fn pan(&mut self, delta_px: Vec2, viewport_h_px: f32) {
        if !delta_px.is_finite() || viewport_h_px.is_nan() || viewport_h_px <= 0.0 {
            return;
        }
        let per_px = self.world_per_pixel(viewport_h_px);
        self.target += (-self.right() * delta_px.x + self.up() * delta_px.y) * per_px;
    }

    /// World units covered by one pixel at the target distance.
    pub fn world_per_pixel(&self, viewport_h_px: f32) -> f32 {
        2.0 * self.distance * (self.fov_y * 0.5).tan() / viewport_h_px.max(1.0)
    }

    /// Dolly towards the target. Positive `scroll` (wheel up) zooms in.
    pub fn zoom(&mut self, scroll: f32) {
        if !scroll.is_finite() {
            return;
        }
        self.distance = (self.distance * (-scroll * 0.002).exp()).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// Place the target at the box centre and back off until the bounding
    /// sphere fits both the horizontal and vertical field of view.
    pub fn frame_bounds(&mut self, bounds: Aabb, aspect: f32) {
        if !bounds.min.is_finite() || !bounds.max.is_finite() {
            return;
        }
        let radius = bounds.radius().max(MIN_DISTANCE);
        let aspect = sanitize_aspect(aspect);
        let half_y = self.fov_y * 0.5;
        let half_x = (half_y.tan() * aspect).atan();
        let half = half_y.min(half_x).max(1e-3);
        self.target = bounds.center();
        self.distance = (radius / half.sin() * 1.05).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// World-space ray through a point given in normalised device
    /// coordinates (x right, y up, both -1..1).
    pub fn ray_from_ndc(&self, ndc: Vec2, aspect: f32, scene_radius: f32) -> Option<Ray> {
        let inv = self.view_proj(aspect, scene_radius).inverse();
        if !inv.is_finite() {
            return None;
        }
        let near = inv * ndc.extend(0.0).extend(1.0);
        let far = inv * ndc.extend(1.0).extend(1.0);
        if near.w.abs() < 1e-12 || far.w.abs() < 1e-12 {
            return None;
        }
        let a = near.xyz() / near.w;
        let b = far.xyz() / far.w;
        let dir = (b - a).try_normalize()?;
        Some(Ray { origin: a, dir })
    }
}

fn sanitize_aspect(aspect: f32) -> f32 {
    if aspect.is_finite() && aspect > 1e-4 {
        aspect
    } else {
        1.0
    }
}

/// Convert a screen position into NDC for a viewport at `min` with `size`.
pub fn screen_to_ndc(pos: Vec2, min: Vec2, size: Vec2) -> Vec2 {
    let size = size.max(Vec2::splat(1.0));
    let t = (pos - min) / size;
    Vec2::new(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0)
}

/// Moller-Trumbore ray/triangle intersection, two-sided. Returns distance.
pub fn ray_triangle(ray: &Ray, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let p = ray.dir.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = ray.origin - a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = ray.dir.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t > 0.0 && t.is_finite()).then_some(t)
}

/// Closest hit of `ray` against an indexed triangle list. Triangles with
/// out-of-range indices are skipped.
pub fn ray_mesh(ray: &Ray, positions: &[[f32; 3]], indices: &[u32]) -> Option<f32> {
    let mut best: Option<f32> = None;
    let get = |i: u32| positions.get(i as usize).map(|p| Vec3::from_array(*p));
    for &[i0, i1, i2] in indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (get(i0), get(i1), get(i2)) else {
            continue;
        };
        if let Some(t) = ray_triangle(ray, a, b, c)
            && best.is_none_or(|bt| t < bt)
        {
            best = Some(t);
        }
    }
    best
}

/// Screen-space directions of the world axes for a corner gizmo.
/// Returns `(axis index, direction with +y down, depth)`; larger depth is
/// further from the viewer. Sorted back to front.
pub fn gizmo_axes(cam: &OrbitCamera) -> [(usize, Vec2, f32); 3] {
    let (r, u, f) = (cam.right(), cam.up(), cam.forward());
    let mut res = [(0usize, Vec3::X), (1, Vec3::Y), (2, Vec3::Z)]
        .map(|(i, a)| (i, Vec2::new(a.dot(r), -a.dot(u)), a.dot(f)));
    res.sort_by(|a, b| b.2.total_cmp(&a.2));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(m: Mat4, p: Vec3) -> Vec3 {
        let c = m * p.extend(1.0);
        c.xyz() / c.w
    }

    #[test]
    fn eye_is_z_up_orbit() {
        let cam = OrbitCamera {
            yaw: 0.0,
            pitch: 0.0,
            distance: 2.0,
            ..Default::default()
        };
        assert!((cam.eye() - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-5);
        assert!((cam.up() - Vec3::Z).length() < 1e-5);
        // forward = -X, right = forward x Z = +Y.
        assert!((cam.right() - Vec3::Y).length() < 1e-5);
    }

    #[test]
    fn target_projects_to_center_with_valid_depth() {
        let cam = OrbitCamera::default();
        let vp = cam.view_proj(16.0 / 9.0, 1.0);
        let p = project(vp, cam.target);
        assert!(p.x.abs() < 1e-4 && p.y.abs() < 1e-4);
        assert!(p.z > 0.0 && p.z < 1.0);
    }

    #[test]
    fn world_up_projects_upwards() {
        let cam = OrbitCamera::default();
        let vp = cam.view_proj(1.0, 1.0);
        let p = project(vp, cam.target + Vec3::Z * 0.1);
        assert!(p.y > 0.0);
    }

    #[test]
    fn frame_bounds_fits_all_corners() {
        for aspect in [0.3f32, 1.0, 2.5] {
            for (yaw, pitch) in [(0.0f32, 0.0f32), (1.0, 0.5), (3.0, -1.2)] {
                let mut cam = OrbitCamera {
                    yaw,
                    pitch,
                    ..Default::default()
                };
                let b = Aabb {
                    min: Vec3::new(-3.0, 10.0, 0.0),
                    max: Vec3::new(1.0, 12.0, 7.0),
                };
                cam.frame_bounds(b, aspect);
                assert!((cam.target - b.center()).length() < 1e-5);
                let vp = cam.view_proj(aspect, b.radius());
                for c in b.corners() {
                    let p = project(vp, c);
                    assert!(p.x.abs() <= 1.0 && p.y.abs() <= 1.0, "{aspect} {p:?}");
                    assert!(p.z > 0.0 && p.z < 1.0, "depth {p:?}");
                }
            }
        }
    }

    #[test]
    fn frame_bounds_ignores_non_finite() {
        let mut cam = OrbitCamera::default();
        let before = cam;
        cam.frame_bounds(
            Aabb {
                min: Vec3::splat(f32::NAN),
                max: Vec3::ONE,
            },
            1.0,
        );
        assert_eq!(cam, before);
    }

    #[test]
    fn aabb_skips_non_finite_points() {
        let pts = [[0.0, 0.0, 0.0], [f32::NAN, 1.0, 1.0], [2.0, -1.0, 3.0]];
        let b = Aabb::from_points(pts.iter()).unwrap();
        assert_eq!(b.min, Vec3::new(0.0, -1.0, 0.0));
        assert_eq!(b.max, Vec3::new(2.0, 0.0, 3.0));
        assert!(Aabb::from_points([].iter()).is_none());
    }

    #[test]
    fn zoom_and_orbit_clamp() {
        let mut cam = OrbitCamera::default();
        cam.zoom(1e9);
        assert!(cam.distance >= MIN_DISTANCE);
        cam.zoom(-1e9);
        assert!(cam.distance <= MAX_DISTANCE);
        cam.zoom(f32::NAN);
        assert!(cam.distance.is_finite());
        cam.orbit(Vec2::new(0.0, 1e6));
        assert!(cam.pitch <= MAX_PITCH);
        cam.orbit(Vec2::new(0.0, -1e7));
        assert!(cam.pitch >= -MAX_PITCH);
    }

    #[test]
    fn pan_moves_target_in_view_plane() {
        let mut cam = OrbitCamera::default();
        let f = cam.forward();
        cam.pan(Vec2::new(100.0, -40.0), 600.0);
        assert!(cam.target.length() > 0.0);
        assert!(cam.target.dot(f).abs() < 1e-4);
        // Drag right: scene follows the cursor, so the target moves left.
        let mut cam2 = OrbitCamera::default();
        cam2.pan(Vec2::new(10.0, 0.0), 600.0);
        assert!(cam2.target.dot(cam2.right()) < 0.0);
    }

    #[test]
    fn center_ray_hits_target() {
        let cam = OrbitCamera {
            target: Vec3::new(1.0, 2.0, 3.0),
            ..Default::default()
        };
        let ray = cam.ray_from_ndc(Vec2::ZERO, 1.5, 1.0).unwrap();
        assert!((ray.dir - cam.forward()).length() < 1e-3);
        let t = (cam.target - ray.origin).dot(ray.dir);
        assert!((ray.at(t) - cam.target).length() < 1e-3);
    }

    #[test]
    fn offset_ray_reprojects_to_same_ndc() {
        let cam = OrbitCamera::default();
        let aspect = 1.6;
        let ndc = Vec2::new(0.4, -0.7);
        let ray = cam.ray_from_ndc(ndc, aspect, 1.0).unwrap();
        let p = project(cam.view_proj(aspect, 1.0), ray.at(3.0));
        assert!((p.truncate() - ndc).length() < 1e-3);
    }

    #[test]
    fn screen_to_ndc_corners() {
        let min = Vec2::new(10.0, 20.0);
        let size = Vec2::new(100.0, 50.0);
        assert_eq!(screen_to_ndc(min, min, size), Vec2::new(-1.0, 1.0));
        assert_eq!(screen_to_ndc(min + size, min, size), Vec2::new(1.0, -1.0));
    }

    #[test]
    fn triangle_hit_and_miss() {
        let ray = Ray {
            origin: Vec3::new(0.2, 0.2, 5.0),
            dir: -Vec3::Z,
        };
        let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::Y);
        assert!((ray_triangle(&ray, a, b, c).unwrap() - 5.0).abs() < 1e-5);
        // Two-sided.
        assert!(ray_triangle(&ray, a, c, b).is_some());
        let miss = Ray {
            origin: Vec3::new(2.0, 2.0, 5.0),
            dir: -Vec3::Z,
        };
        assert!(ray_triangle(&miss, a, b, c).is_none());
    }

    #[test]
    fn ray_mesh_skips_bad_indices_and_picks_closest() {
        let pos = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
        ];
        let idx = [0, 1, 2, 3, 4, 5, 0, 1, 99];
        let ray = Ray {
            origin: Vec3::new(0.1, 0.1, 5.0),
            dir: -Vec3::Z,
        };
        assert!((ray_mesh(&ray, &pos, &idx).unwrap() - 4.0).abs() < 1e-5);
    }

    #[test]
    fn gizmo_sorted_back_to_front() {
        let cam = OrbitCamera::default();
        let g = gizmo_axes(&cam);
        assert!(g[0].2 >= g[1].2 && g[1].2 >= g[2].2);
        // Z axis points up on screen (negative y in screen space).
        let z = g.iter().find(|a| a.0 == 2).unwrap();
        assert!(z.1.y < 0.0);
    }
}
