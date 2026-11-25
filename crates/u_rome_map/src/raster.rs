//! Top-down density raster for Studio's 2D map view.

use u_rome_core::{Image, ImageFormat};

use crate::map::{MapData, Placement};

/// Largest raster edge accepted; larger requests are clamped.
pub const MAX_RASTER_EDGE: u32 = 8192;

/// A 2D world rectangle (RAGE X east, Y north), metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds2 {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Bounds2 {
    pub fn new(min: [f32; 2], max: [f32; 2]) -> Self {
        Self { min, max }
    }

    /// Bounds of the finite positions, or `None` when there are none.
    pub fn from_points(points: impl IntoIterator<Item = [f32; 3]>) -> Option<Self> {
        let mut b: Option<Self> = None;
        for p in points {
            if !(p[0].is_finite() && p[1].is_finite()) {
                continue;
            }
            b = Some(match b {
                None => Self::new([p[0], p[1]], [p[0], p[1]]),
                Some(b) => Self::new(
                    [b.min[0].min(p[0]), b.min[1].min(p[1])],
                    [b.max[0].max(p[0]), b.max[1].max(p[1])],
                ),
            });
        }
        b
    }

    /// Bounds of every entity, prop and grass instance in a set of maps.
    pub fn from_maps(maps: &[MapData]) -> Option<Self> {
        Self::from_points(maps.iter().flat_map(map_points))
    }

    fn is_valid(&self) -> bool {
        self.min
            .iter()
            .chain(self.max.iter())
            .all(|v| v.is_finite())
            && self.max[0] > self.min[0]
            && self.max[1] > self.min[1]
    }
}

/// Every placed point of a map: entities, prop instances, grass instances.
pub fn map_points(map: &MapData) -> impl Iterator<Item = [f32; 3]> + '_ {
    let ents = map.entities.iter().map(|e| e.position);
    let props = map
        .prop_batches
        .iter()
        .flat_map(|b| b.instances.iter().map(|i| i.translation()));
    let grass = map
        .grass_batches
        .iter()
        .flat_map(|b| b.instances.iter().map(|i| i.position));
    ents.chain(props).chain(grass)
}

/// Render a density heat map of placements, north up.
///
/// Each pixel counts the placements inside it; counts are log-scaled against
/// the densest pixel and mapped through a dark-blue to yellow-white ramp.
/// Empty pixels are fully transparent so the image can overlay a basemap.
/// Width and height are clamped to `1..=MAX_RASTER_EDGE`; degenerate bounds
/// produce a transparent image.
pub fn render_topdown(placements: &[Placement], bounds: Bounds2, width: u32, height: u32) -> Image {
    render_topdown_points(placements.iter().map(|p| p.position), bounds, width, height)
}

/// [`render_topdown`] over arbitrary positions (e.g. [`map_points`]).
pub fn render_topdown_points(
    points: impl IntoIterator<Item = [f32; 3]>,
    bounds: Bounds2,
    width: u32,
    height: u32,
) -> Image {
    let w = width.clamp(1, MAX_RASTER_EDGE) as usize;
    let h = height.clamp(1, MAX_RASTER_EDGE) as usize;
    let mut counts = vec![0u32; w * h];
    if bounds.is_valid() {
        let sx = w as f32 / (bounds.max[0] - bounds.min[0]);
        let sy = h as f32 / (bounds.max[1] - bounds.min[1]);
        for p in points {
            let fx = (p[0] - bounds.min[0]) * sx;
            let fy = (bounds.max[1] - p[1]) * sy; // row 0 = north
            let inside = |v: f32, n: usize| v.is_finite() && v >= 0.0 && v <= n as f32;
            if !inside(fx, w) || !inside(fy, h) {
                continue;
            }
            // Points exactly on the max edge belong to the last pixel.
            let x = (fx as usize).min(w - 1);
            let y = (fy as usize).min(h - 1);
            if let Some(c) = counts.get_mut(y * w + x) {
                *c = c.saturating_add(1);
            }
        }
    }
    let max = counts.iter().copied().max().unwrap_or(0);
    let norm = (1.0 + max as f32).ln();
    let mut rgba = Vec::with_capacity(w * h * 4);
    for &c in &counts {
        if c == 0 || norm <= 0.0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let t = (1.0 + c as f32).ln() / norm;
            rgba.extend_from_slice(&heat(t));
        }
    }
    Image {
        width: w as u32,
        height: h as u32,
        format: ImageFormat::Rgba8,
        mips: vec![rgba],
        provisional: false,
    }
}

/// Heat ramp: dark blue, purple, orange, yellow, white. `t` in 0..=1.
fn heat(t: f32) -> [u8; 4] {
    const STOPS: [[f32; 3]; 5] = [
        [20.0, 30.0, 110.0],
        [130.0, 40.0, 150.0],
        [235.0, 110.0, 40.0],
        [250.0, 220.0, 60.0],
        [255.0, 255.0, 240.0],
    ];
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pos = t * (STOPS.len() - 1) as f32;
    let i = (pos as usize).min(STOPS.len() - 2);
    let f = pos - i as f32;
    let a = STOPS.get(i).copied().unwrap_or([0.0; 3]);
    let b = STOPS.get(i + 1).copied().unwrap_or([0.0; 3]);
    let mix = |k: usize| (a[k] + (b[k] - a[k]) * f).round().clamp(0.0, 255.0) as u8;
    // Alpha rises with density so sparse areas stay subtle over a basemap.
    let alpha = (110.0 + 145.0 * t).round().clamp(0.0, 255.0) as u8;
    [mix(0), mix(1), mix(2), alpha]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &Image, x: usize, y: usize) -> [u8; 4] {
        let i = (y * img.width as usize + x) * 4;
        let m = &img.mips[0];
        [m[i], m[i + 1], m[i + 2], m[i + 3]]
    }

    #[test]
    fn raster_counts_and_orientation() {
        let b = Bounds2::new([0.0, 0.0], [100.0, 100.0]);
        let mut pts = vec![[5.0, 95.0, 0.0]; 10]; // north-west corner, dense
        pts.push([95.0, 5.0, 0.0]); // south-east, sparse
        pts.push([100.0, 100.0, 0.0]); // max edge: last column, first row
        pts.push([-1.0, 50.0, 0.0]); // outside
        pts.push([f32::NAN, 1.0, 0.0]);
        let img = render_topdown_points(pts, b, 10, 10);
        assert_eq!((img.width, img.height), (10, 10));
        assert_eq!(img.mips.len(), 1);
        assert_eq!(img.mips[0].len(), 400);
        let dense = px(&img, 0, 0);
        let sparse = px(&img, 9, 9);
        assert_eq!(dense, heat(1.0));
        assert!(sparse[3] > 0 && sparse[3] < dense[3]);
        assert!(px(&img, 9, 0)[3] > 0);
        assert_eq!(px(&img, 5, 5), [0, 0, 0, 0]);
        let lit = img.mips[0].chunks(4).filter(|p| p[3] > 0).count();
        assert_eq!(lit, 3);
    }

    #[test]
    fn degenerate_inputs() {
        let img = render_topdown(&[], Bounds2::new([0.0, 0.0], [0.0, 0.0]), 0, 100_000);
        assert_eq!((img.width, img.height), (1, MAX_RASTER_EDGE));
        assert!(img.mips[0].iter().all(|&b| b == 0));
        let img = render_topdown_points(
            [[1.0, 1.0, 0.0]],
            Bounds2::new([f32::NAN, 0.0], [1.0, 2.0]),
            4,
            4,
        );
        assert!(img.mips[0].iter().all(|&b| b == 0));
        assert_eq!(Bounds2::from_points([[f32::NAN, 0.0, 0.0]]), None);
        let b = Bounds2::from_points([[1.0, 5.0, 0.0], [-3.0, 2.0, 9.0]]).unwrap();
        assert_eq!(b, Bounds2::new([-3.0, 2.0], [1.0, 5.0]));
    }
}
