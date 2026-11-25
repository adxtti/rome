//! 2D image viewer widget (pure egui, no wgpu needed).

use std::sync::Arc;

use egui::{
    Color32, ColorImage, Pos2, Rect, Response, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2, pos2, vec2,
};
use u_rome_core::{Image, ImageFormat};

use crate::error::RenderError;

/// Size of mip `level` for a `width` x `height` base image.
pub fn mip_size(width: u32, height: u32, level: usize) -> (u32, u32) {
    let shift = level.min(31) as u32;
    ((width >> shift).max(1), (height >> shift).max(1))
}

/// Validated RGBA8 bytes and size of one mip level.
pub fn mip_data(image: &Image, level: usize) -> Result<(&[u8], u32, u32), RenderError> {
    if image.width == 0 || image.height == 0 {
        return Err(RenderError::InvalidDimensions {
            width: image.width,
            height: image.height,
        });
    }
    let data = image.mips.get(level).ok_or(RenderError::MissingMip {
        level,
        count: image.mips.len(),
    })?;
    let (w, h) = mip_size(image.width, image.height, level);
    let expected = u64::from(w) * u64::from(h) * 4;
    if data.len() as u64 != expected {
        return Err(RenderError::MipSize {
            level,
            width: w,
            height: h,
            expected,
            actual: data.len(),
        });
    }
    Ok((data, w, h))
}

/// Which channels are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Channels {
    pub r: bool,
    pub g: bool,
    pub b: bool,
    pub a: bool,
}

impl Default for Channels {
    fn default() -> Self {
        Self {
            r: true,
            g: true,
            b: true,
            a: true,
        }
    }
}

impl Channels {
    fn count(&self) -> usize {
        [self.r, self.g, self.b, self.a]
            .iter()
            .filter(|&&c| c)
            .count()
    }

    /// True when the displayed image uses alpha (checkerboard needed).
    pub fn shows_alpha(&self) -> bool {
        self.a && self.count() > 1
    }
}

/// Apply the channel selection to RGBA8 pixels for display.
///
/// - Exactly one channel selected: that channel as opaque greyscale.
/// - Otherwise: unselected colour channels are zeroed, alpha is kept only if
///   A is selected (opaque otherwise).
pub fn compose_display(rgba: &[u8], ch: Channels) -> Vec<u8> {
    let single = (ch.count() == 1).then(|| [ch.r, ch.g, ch.b, ch.a].iter().position(|&c| c));
    let mut out = Vec::with_capacity(rgba.len() - rgba.len() % 4);
    for px in rgba.as_chunks::<4>().0 {
        match single {
            Some(Some(i)) => {
                let v = px[i];
                out.extend_from_slice(&[v, v, v, 255]);
            }
            _ => out.extend_from_slice(&[
                if ch.r { px[0] } else { 0 },
                if ch.g { px[1] } else { 0 },
                if ch.b { px[2] } else { 0 },
                if ch.a { px[3] } else { 255 },
            ]),
        }
    }
    out
}

/// Nearest-neighbour downscale by an integer factor so both sides fit
/// `max_side`. Returns the input unchanged if it already fits.
pub fn downscale_to_fit(rgba: Vec<u8>, w: u32, h: u32, max_side: u32) -> (Vec<u8>, u32, u32) {
    let max_side = max_side.max(1);
    if w <= max_side && h <= max_side {
        return (rgba, w, h);
    }
    let factor = w.max(h).div_ceil(max_side).max(1);
    let (nw, nh) = (w.div_ceil(factor).max(1), h.div_ceil(factor).max(1));
    let mut out = Vec::with_capacity(nw as usize * nh as usize * 4);
    for y in 0..nh {
        for x in 0..nw {
            let (sx, sy) = ((x * factor).min(w - 1), (y * factor).min(h - 1));
            let i = (sy as usize * w as usize + sx as usize) * 4;
            out.extend_from_slice(rgba.get(i..i + 4).unwrap_or(&[0, 0, 0, 0]));
        }
    }
    (out, nw, nh)
}

/// Pixel under `pos` for an image drawn at `image_min` with `zoom` screen
/// points per pixel, or `None` outside the image.
pub fn screen_to_pixel(
    pos: Pos2,
    image_min: Pos2,
    zoom: f32,
    w: u32,
    h: u32,
) -> Option<(u32, u32)> {
    if zoom.is_nan() || zoom <= 0.0 || !zoom.is_finite() {
        return None;
    }
    let x = ((pos.x - image_min.x) / zoom).floor();
    let y = ((pos.y - image_min.y) / zoom).floor();
    (x >= 0.0 && y >= 0.0 && x < w as f32 && y < h as f32).then_some((x as u32, y as u32))
}

/// Zoom that fits a `w` x `h` image into `view` with a small margin.
pub fn fit_zoom(w: u32, h: u32, view: Vec2) -> f32 {
    let z = ((view.x - 16.0) / w.max(1) as f32).min((view.y - 16.0) / h.max(1) as f32);
    if z.is_finite() && z > 0.0 {
        z.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

/// Zoom by `factor` keeping the image point under `cursor` (relative to the
/// view centre) fixed. Returns the new `(zoom, offset)`.
pub fn zoom_about(zoom: f32, offset: Vec2, cursor: Vec2, factor: f32) -> (f32, Vec2) {
    let new_zoom = (zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    if !new_zoom.is_finite() || zoom <= 0.0 {
        return (zoom, offset);
    }
    let p = (cursor - offset) / zoom;
    (new_zoom, cursor - p * new_zoom)
}

const MIN_ZOOM: f32 = 1.0 / 64.0;
const MAX_ZOOM: f32 = 256.0;

#[derive(Clone, Copy, PartialEq, Eq)]
struct DisplayKey {
    generation: u64,
    mip: usize,
    channels: Channels,
}

/// Image viewer with zoom/pan, mip selector, channel toggles, alpha
/// checkerboard and a pixel inspector.
pub struct ImageView {
    image: Option<Arc<Image>>,
    generation: u64,
    mip: usize,
    channels: Channels,
    zoom: f32,
    offset: Vec2,
    fit_pending: bool,
    texture: Option<TextureHandle>,
    texture_key: Option<DisplayKey>,
    texture_error: Option<String>,
    downscaled: bool,
    checker: Option<TextureHandle>,
}

impl Default for ImageView {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageView {
    pub fn new() -> Self {
        Self {
            image: None,
            generation: 0,
            mip: 0,
            channels: Channels::default(),
            zoom: 1.0,
            offset: Vec2::ZERO,
            fit_pending: true,
            texture: None,
            texture_key: None,
            texture_error: None,
            downscaled: false,
            checker: None,
        }
    }

    /// Show a copy of `image`.
    pub fn set_image(&mut self, image: &Image) {
        self.set_image_shared(Arc::new(image.clone()));
    }

    /// Show a shared image without copying pixel data.
    pub fn set_image_shared(&mut self, image: Arc<Image>) {
        if let Err(e) = mip_data(&image, 0) {
            log::warn!("render: image view: {e}");
        }
        self.image = Some(image);
        self.generation = self.generation.wrapping_add(1);
        self.mip = 0;
        self.fit_pending = true;
        self.texture_key = None;
    }

    pub fn clear(&mut self) {
        self.image = None;
        self.texture = None;
        self.texture_key = None;
    }

    pub fn image(&self) -> Option<&Image> {
        self.image.as_deref()
    }

    pub fn channels_mut(&mut self) -> &mut Channels {
        &mut self.channels
    }

    pub fn show(&mut self, ui: &mut Ui) -> Response {
        let image = self.image.clone();
        ui.vertical(|ui| {
            self.toolbar(ui, image.as_deref());
            match image.as_deref() {
                Some(img) => self.canvas(ui, img),
                None => {
                    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, Color32::from_gray(24));
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "No image",
                        egui::FontId::proportional(14.0),
                        Color32::GRAY,
                    );
                    resp
                }
            }
        })
        .inner
    }

    fn toolbar(&mut self, ui: &mut Ui, image: Option<&Image>) {
        ui.horizontal_wrapped(|ui| {
            let Some(img) = image else {
                ui.label("Image");
                return;
            };
            let mip_count = img.mips.len().max(1);
            let label = |i: usize| {
                let (w, h) = mip_size(img.width, img.height, i);
                format!("Mip {i}: {w}x{h}")
            };
            egui::ComboBox::from_id_salt(ui.scope_id().with("mip"))
                .selected_text(label(self.mip))
                .show_ui(ui, |ui| {
                    for i in 0..mip_count {
                        ui.selectable_value(&mut self.mip, i, label(i));
                    }
                });
            ui.separator();
            for (name, flag, color) in [
                ("R", &mut self.channels.r, Color32::from_rgb(230, 80, 80)),
                ("G", &mut self.channels.g, Color32::from_rgb(80, 200, 80)),
                ("B", &mut self.channels.b, Color32::from_rgb(90, 130, 240)),
                ("A", &mut self.channels.a, Color32::LIGHT_GRAY),
            ] {
                let text = RichText::new(name).strong();
                let text = if *flag { text.color(color) } else { text };
                if ui.selectable_label(*flag, text).clicked() {
                    *flag = !*flag;
                }
            }
            ui.separator();
            if ui.button("Fit").clicked() {
                self.fit_pending = true;
            }
            if ui.button("1:1").clicked() {
                self.zoom = 1.0;
                self.offset = Vec2::ZERO;
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            ui.separator();
            let fmt = match img.format {
                ImageFormat::Rgba8 => "RGBA8",
                ImageFormat::Rgba8Srgb => "RGBA8 sRGB",
            };
            ui.label(format!(
                "{}x{} {fmt}, {} mips",
                img.width,
                img.height,
                img.mips.len()
            ));
            if img.provisional {
                provisional_badge(ui);
            }
            if self.downscaled {
                ui.label(RichText::new("downscaled for display").weak());
            }
        });
    }

    fn ensure_texture(&mut self, ui: &Ui, img: &Image) {
        let key = DisplayKey {
            generation: self.generation,
            mip: self.mip,
            channels: self.channels,
        };
        if self.texture_key == Some(key) {
            return;
        }
        self.texture_key = Some(key);
        match mip_data(img, self.mip) {
            Ok((data, w, h)) => {
                let max_side = ui.ctx().input(|i| i.max_texture_side).max(1) as u32;
                let rgba = compose_display(data, self.channels);
                let (rgba, dw, dh) = downscale_to_fit(rgba, w, h, max_side);
                self.downscaled = (dw, dh) != (w, h);
                let ci = ColorImage::from_rgba_unmultiplied([dw as usize, dh as usize], &rgba);
                match &mut self.texture {
                    Some(t) => t.set(ci, TextureOptions::NEAREST),
                    None => {
                        self.texture = Some(ui.ctx().load_texture(
                            "u_rome_image_view",
                            ci,
                            TextureOptions::NEAREST,
                        ))
                    }
                }
                self.texture_error = None;
            }
            Err(e) => {
                log::warn!("render: image view: {e}");
                self.texture_error = Some(e.to_string());
            }
        }
    }

    fn canvas(&mut self, ui: &mut Ui, img: &Image) -> Response {
        self.mip = self.mip.min(img.mips.len().saturating_sub(1));
        self.ensure_texture(ui, img);
        let inspector_h = 22.0;
        let size = ui.available_size() - vec2(0.0, inspector_h);
        let (rect, resp) =
            ui.allocate_exact_size(size.max(vec2(32.0, 32.0)), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(24));

        if let Some(err) = &self.texture_error {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("Cannot display: {err}"),
                egui::FontId::proportional(14.0),
                Color32::from_rgb(240, 120, 100),
            );
            ui.label("");
            return resp;
        }
        let (w, h) = mip_size(img.width, img.height, self.mip);
        if self.fit_pending {
            self.zoom = fit_zoom(w, h, rect.size());
            self.offset = Vec2::ZERO;
            self.fit_pending = false;
        }

        // Interaction.
        if resp.dragged() {
            self.offset += resp.drag_delta();
        }
        if resp.double_clicked() {
            self.fit_pending = true;
        }
        if let Some(hover) = resp.hover_pos() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = (scroll * 0.0025).exp() * pinch;
            if (factor - 1.0).abs() > 1e-4 {
                (self.zoom, self.offset) =
                    zoom_about(self.zoom, self.offset, hover - rect.center(), factor);
            }
        }

        let img_size = vec2(w as f32, h as f32) * self.zoom;
        let img_rect = Rect::from_center_size(rect.center() + self.offset, img_size);

        if self.channels.shows_alpha() {
            let checker = self.checker.get_or_insert_with(|| {
                let (a, b) = (Color32::from_gray(90), Color32::from_gray(140));
                ui.ctx().load_texture(
                    "u_rome_checker",
                    ColorImage::new([2, 2], vec![a, b, b, a]),
                    TextureOptions::NEAREST_REPEAT,
                )
            });
            let vis = img_rect.intersect(rect);
            if vis.is_positive() {
                // 2x2 texture, 8 points per square: 16 points per repeat.
                let uv0 = (vis.min - img_rect.min) / 16.0;
                let uv1 = (vis.max - img_rect.min) / 16.0;
                painter.image(
                    checker.id(),
                    vis,
                    Rect::from_min_max(pos2(uv0.x, uv0.y), pos2(uv1.x, uv1.y)),
                    Color32::WHITE,
                );
            }
        }
        if let Some(tex) = &self.texture {
            painter.image(
                tex.id(),
                img_rect,
                Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        painter.rect_stroke(
            img_rect,
            0.0,
            Stroke::new(1.0, Color32::from_gray(60)),
            egui::StrokeKind::Outside,
        );

        // Pixel inspector.
        let picked = resp
            .hover_pos()
            .and_then(|p| screen_to_pixel(p, img_rect.min, self.zoom, w, h));
        ui.horizontal(|ui| {
            ui.set_min_height(inspector_h - 4.0);
            let Some((x, y)) = picked else {
                ui.label(RichText::new("Hover the image to inspect pixels").weak());
                return;
            };
            if self.zoom >= 6.0 {
                let r = Rect::from_min_size(
                    img_rect.min + vec2(x as f32, y as f32) * self.zoom,
                    Vec2::splat(self.zoom),
                );
                painter.rect_stroke(
                    r,
                    0.0,
                    Stroke::new(1.0, Color32::YELLOW),
                    egui::StrokeKind::Inside,
                );
            }
            let px = mip_data(img, self.mip).ok().and_then(|(d, _, _)| {
                let i = (y as usize * w as usize + x as usize) * 4;
                d.get(i..i + 4)
            });
            if let Some(&[r, g, b, a]) = px {
                let (sw, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                ui.painter()
                    .rect_filled(sw, 2.0, Color32::from_rgba_unmultiplied(r, g, b, 255));
                ui.monospace(format!(
                    "x {x:>5} y {y:>5}   R {r:>3} G {g:>3} B {b:>3} A {a:>3}   #{r:02X}{g:02X}{b:02X}{a:02X}   ({:.3}, {:.3}, {:.3}, {:.3})",
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    a as f32 / 255.0
                ));
            }
        });
        resp
    }
}

/// A small orange "PROVISIONAL" badge with an explanatory tooltip.
pub fn provisional_badge(ui: &mut Ui) -> Response {
    egui::Frame::new()
        .fill(Color32::from_rgb(150, 90, 10))
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(
                RichText::new("PROVISIONAL")
                    .small()
                    .strong()
                    .color(Color32::WHITE),
            )
        })
        .response
        .on_hover_text(
            "Part of this decode is approximate (for example fitted tables). \
             Pixels may differ from the game.",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, mips: Vec<Vec<u8>>) -> Image {
        Image {
            width: w,
            height: h,
            format: ImageFormat::Rgba8,
            mips,
            provisional: false,
        }
    }

    #[test]
    fn mip_sizes() {
        assert_eq!(mip_size(256, 64, 0), (256, 64));
        assert_eq!(mip_size(256, 64, 3), (32, 8));
        assert_eq!(mip_size(256, 64, 7), (2, 1));
        assert_eq!(mip_size(256, 64, 200), (1, 1));
    }

    #[test]
    fn mip_validation() {
        let img = image(4, 2, vec![vec![0; 32], vec![0; 8], vec![0; 3]]);
        assert!(mip_data(&img, 0).is_ok());
        assert_eq!(mip_data(&img, 1).unwrap().1, 2);
        assert!(matches!(
            mip_data(&img, 2),
            Err(RenderError::MipSize {
                expected: 4,
                actual: 3,
                ..
            })
        ));
        assert!(matches!(
            mip_data(&img, 5),
            Err(RenderError::MissingMip { .. })
        ));
        assert!(mip_data(&image(0, 4, vec![vec![]]), 0).is_err());
    }

    #[test]
    fn channel_composition() {
        let px = [10, 20, 30, 40, 50, 60, 70, 80];
        assert_eq!(compose_display(&px, Channels::default()), px.to_vec());
        let only_g = Channels {
            r: false,
            g: true,
            b: false,
            a: false,
        };
        assert_eq!(
            compose_display(&px, only_g),
            vec![20, 20, 20, 255, 60, 60, 60, 255]
        );
        let only_a = Channels {
            r: false,
            g: false,
            b: false,
            a: true,
        };
        assert_eq!(compose_display(&px, only_a)[..4], [40, 40, 40, 255]);
        let rb = Channels {
            r: true,
            g: false,
            b: true,
            a: false,
        };
        assert_eq!(compose_display(&px, rb)[..4], [10, 0, 30, 255]);
        assert!(!rb.shows_alpha() && Channels::default().shows_alpha());
        // Trailing partial pixel is ignored instead of panicking.
        assert_eq!(compose_display(&[1, 2, 3], Channels::default()).len(), 0);
    }

    #[test]
    fn downscale() {
        let data = vec![7u8; 10 * 3 * 4];
        let (out, w, h) = downscale_to_fit(data.clone(), 10, 3, 4);
        assert_eq!((w, h), (4, 1));
        assert_eq!(out.len(), 16);
        let (same, w, h) = downscale_to_fit(data, 10, 3, 16);
        assert_eq!((w, h, same.len()), (10, 3, 120));
    }

    #[test]
    fn pixel_picking() {
        let min = pos2(100.0, 50.0);
        assert_eq!(
            screen_to_pixel(pos2(100.0, 50.0), min, 4.0, 8, 8),
            Some((0, 0))
        );
        assert_eq!(
            screen_to_pixel(pos2(131.9, 81.9), min, 4.0, 8, 8),
            Some((7, 7))
        );
        assert_eq!(screen_to_pixel(pos2(132.0, 60.0), min, 4.0, 8, 8), None);
        assert_eq!(screen_to_pixel(pos2(99.0, 60.0), min, 4.0, 8, 8), None);
        assert_eq!(screen_to_pixel(pos2(110.0, 60.0), min, 0.0, 8, 8), None);
    }

    #[test]
    fn zoom_keeps_cursor_point() {
        let (z, off) = (2.0, vec2(10.0, -5.0));
        let cursor = vec2(40.0, 30.0);
        let before = (cursor - off) / z;
        let (z2, off2) = zoom_about(z, off, cursor, 1.5);
        assert!((z2 - 3.0).abs() < 1e-6);
        let after = (cursor - off2) / z2;
        assert!((before - after).length() < 1e-4);
        assert!((fit_zoom(1000, 500, vec2(516.0, 516.0)) - 0.5).abs() < 1e-6);
        assert_eq!(fit_zoom(0, 0, vec2(f32::NAN, 1.0)), 1.0);
    }
}
