//! ROME logo assets: window icon and theme-aware wordmark.
//!
//! Sources live in `assets/`: `rome-icon.svg` (square app icon) and the
//! repository's `assets/rome-logo-*.svg` wordmarks, pre-rendered to PNG with
//! resvg so Studio needs no SVG renderer at runtime.

use std::sync::Arc;

use eframe::egui;

/// 256 px square app icon (white R-star on a dark rounded tile).
pub const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");
/// Wordmark with dark ink, for light themes.
const LOGO_DARK_INK_PNG: &[u8] = include_bytes!("../assets/logo-dark-ink.png");
/// Wordmark with light ink, for dark themes.
const LOGO_LIGHT_INK_PNG: &[u8] = include_bytes!("../assets/logo-light-ink.png");

/// Window and taskbar icon. `None` (default icon) if decoding fails.
pub fn window_icon() -> Option<Arc<egui::IconData>> {
    match eframe::icon_data::from_png_bytes(ICON_PNG) {
        Ok(icon) => Some(Arc::new(icon)),
        Err(e) => {
            log::warn!("app icon could not be decoded: {e}");
            None
        }
    }
}

fn load(ctx: &egui::Context, name: &str, png: &[u8]) -> Option<egui::TextureHandle> {
    let icon = eframe::icon_data::from_png_bytes(png)
        .map_err(|e| log::warn!("logo {name} could not be decoded: {e}"))
        .ok()?;
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR))
}

/// The wordmark texture matching the current theme, loaded once per context.
fn wordmark_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let dark = ctx.global_style().visuals.dark_mode;
    let (key, name, png) = if dark {
        (
            "u_rome_logo_light_ink",
            "rome-logo-light-ink",
            LOGO_LIGHT_INK_PNG,
        )
    } else {
        (
            "u_rome_logo_dark_ink",
            "rome-logo-dark-ink",
            LOGO_DARK_INK_PNG,
        )
    };
    let id = egui::Id::unique(key);
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let t = load(ctx, name, png)?;
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

/// Draws the ROME wordmark at the given height. Falls back to text.
pub fn wordmark(ui: &mut egui::Ui, height: f32) -> egui::Response {
    match wordmark_texture(ui.ctx()) {
        Some(tex) => {
            let [w, h] = tex.size();
            let aspect = w as f32 / h.max(1) as f32;
            ui.add(egui::Image::new((
                tex.id(),
                egui::vec2(height * aspect, height),
            )))
        }
        None => ui.label(egui::RichText::new("ROME").strong().size(height * 0.8)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_decode() {
        let icon = window_icon().expect("icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        for png in [LOGO_DARK_INK_PNG, LOGO_LIGHT_INK_PNG] {
            let img = eframe::icon_data::from_png_bytes(png).expect("logo decodes");
            assert!(img.width > img.height * 2, "wordmark is wide");
        }
    }
}
