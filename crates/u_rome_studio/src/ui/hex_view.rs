//! Hex view. Virtualised with its own integer row scrolling (an egui
//! `ScrollArea` measures content in f32 pixels, which loses precision past a
//! few million rows), so 100 MB+ entries scroll exactly. Only the visible
//! window is read from the `ByteSource`.

use std::sync::Arc;

use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::app::Studio;
use crate::backend::{ByteSource, read_vec};
use crate::hex::{
    self, MAX_COPY_BYTES, format_row, hex_column, offset_digits, parse_offset, parse_pattern,
    selection_range,
};
use crate::jobs::{JobError, JobHandle, JobSystem};
use crate::util::{format_count, format_size};

const SCROLLBAR_W: f32 = 12.0;
const GOTO_ID: &str = "hex_goto";

pub struct HexState {
    source: Option<Arc<dyn ByteSource>>,
    label: String,
    bytes_per_row: usize,
    top_row: u64,
    cursor: Option<u64>,
    anchor: Option<u64>,
    pub request_goto_focus: bool,
    goto_text: String,
    find_text: String,
    message: Option<(String, bool)>,
    find_job: Option<(JobHandle<Option<u64>>, usize)>,
    cache: (u64, Vec<u8>),
    scroll_px: f32,
    visible_rows: u64,
    scroll_grab: Option<f32>,
}

impl HexState {
    pub fn new(bytes_per_row: usize) -> Self {
        Self {
            source: None,
            label: String::new(),
            bytes_per_row: bytes_per_row.clamp(4, 64),
            top_row: 0,
            cursor: None,
            anchor: None,
            request_goto_focus: false,
            goto_text: String::new(),
            find_text: String::new(),
            message: None,
            find_job: None,
            cache: (u64::MAX, Vec::new()),
            scroll_px: 0.0,
            visible_rows: 1,
            scroll_grab: None,
        }
    }

    pub fn set_source(&mut self, source: Option<Arc<dyn ByteSource>>, label: String) {
        if let Some((j, _)) = self.find_job.take() {
            j.cancel();
        }
        self.source = source;
        self.label = label;
        self.top_row = 0;
        self.cursor = None;
        self.anchor = None;
        self.message = None;
        self.cache = (u64::MAX, Vec::new());
    }

    pub fn set_bytes_per_row(&mut self, n: usize) {
        let n = n.clamp(4, 64);
        if n != self.bytes_per_row {
            let first = self.top_row * self.bytes_per_row as u64;
            self.bytes_per_row = n;
            self.top_row = first / n as u64;
            self.cache = (u64::MAX, Vec::new());
        }
    }

    fn len(&self) -> u64 {
        self.source.as_ref().map(|s| s.len()).unwrap_or(0)
    }

    fn total_rows(&self) -> u64 {
        self.len().div_ceil(self.bytes_per_row as u64)
    }

    fn max_top(&self) -> u64 {
        self.total_rows().saturating_sub(self.visible_rows.max(1))
    }

    fn ensure_visible(&mut self, offset: u64) {
        let row = offset / self.bytes_per_row as u64;
        let vis = self.visible_rows.max(1);
        if row < self.top_row || row >= self.top_row + vis {
            self.top_row = row.saturating_sub(vis / 3);
        }
        self.top_row = self.top_row.min(self.max_top());
    }

    fn select(&mut self, start: u64, len: u64) {
        self.anchor = Some(start);
        self.cursor = Some(start + len.max(1) - 1);
        self.ensure_visible(start);
    }

    fn selection(&self) -> Option<(u64, u64)> {
        match (self.anchor, self.cursor) {
            (Some(a), Some(c)) => Some(selection_range(a, c)),
            (None, Some(c)) => Some((c, 1)),
            _ => None,
        }
    }

    /// Collects a finished pattern search.
    pub fn poll(&mut self) {
        let Some((job, plen)) = &self.find_job else {
            return;
        };
        let plen = *plen as u64;
        if let Some(r) = job.try_take() {
            self.find_job = None;
            match r {
                Ok(Some(off)) => {
                    self.select(off, plen);
                    self.message = Some((format!("Found at {off:#x}"), false));
                }
                Ok(None) => self.message = Some(("Pattern not found".into(), true)),
                Err(JobError::Cancelled) => self.message = Some(("Search cancelled".into(), false)),
                Err(e) => self.message = Some((format!("Search failed: {e}"), true)),
            }
        }
    }

    fn goto(&mut self) {
        let len = self.len();
        match parse_offset(&self.goto_text, self.cursor.unwrap_or(0), len) {
            Ok(off) => {
                self.select(off, 1);
                self.message = None;
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    fn find_next(&mut self, jobs: &JobSystem) {
        let Some(src) = self.source.clone() else {
            return;
        };
        let pat = match parse_pattern(&self.find_text) {
            Ok(p) => p,
            Err(e) => {
                self.message = Some((e, true));
                return;
            }
        };
        if let Some((j, _)) = self.find_job.take() {
            j.cancel();
        }
        let start = self.cursor.map(|c| c + 1).unwrap_or(0);
        let plen = pat.len();
        self.message = Some(("Searching...".into(), false));
        let job = jobs.spawn(
            format!(
                "Find {} in {}",
                self.find_text.trim(),
                crate::util::truncate(&self.label, 32)
            ),
            move |ctx| hex::find_pattern(src.as_ref(), &pat, start, ctx),
        );
        self.find_job = Some((job, plen));
    }

    fn copy(&mut self, ctx: &egui::Context, fmt: CopyFormat) {
        let (Some(src), Some((start, len))) = (self.source.clone(), self.selection()) else {
            return;
        };
        if fmt == CopyFormat::Offset {
            ctx.copy_text(format!("{start:#x}"));
            return;
        }
        let bytes = match read_vec(src.as_ref(), start, len, MAX_COPY_BYTES as usize) {
            Ok(b) => b,
            Err(e) => {
                self.message = Some((format!("Copy failed: {e}"), true));
                return;
            }
        };
        let text = match fmt {
            CopyFormat::Hex => hex::to_hex_string(&bytes),
            CopyFormat::Ascii => hex::to_ascii_string(&bytes),
            CopyFormat::CArray | CopyFormat::Offset => hex::to_c_array(&bytes),
        };
        ctx.copy_text(text);
        self.message = Some(if len > MAX_COPY_BYTES {
            (
                format!("Copied the first {} only", format_size(MAX_COPY_BYTES)),
                true,
            )
        } else {
            (
                format!("Copied {} bytes", format_count(bytes.len() as u64)),
                false,
            )
        });
    }

    fn window(&mut self, start: u64, len: u64) -> &[u8] {
        if self.cache.0 != start
            || self.cache.1.len() as u64 != len.min(self.len().saturating_sub(start))
        {
            let data = self
                .source
                .as_ref()
                .map(|s| read_vec(s.as_ref(), start, len, 1 << 20))
                .transpose();
            match data {
                Ok(d) => self.cache = (start, d.unwrap_or_default()),
                Err(e) => {
                    log::warn!("hex read at {start:#x} failed: {e}");
                    self.message = Some((format!("Read failed at {start:#x}: {e}"), true));
                    self.cache = (start, Vec::new());
                }
            }
        }
        &self.cache.1
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyFormat {
    Hex,
    Ascii,
    CArray,
    Offset,
}

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let jobs = studio.jobs.clone();
    let st = &mut studio.hex;
    toolbar(st, ui, &jobs);
    if st.source.is_none() {
        ui.add_space(8.0);
        let msg = if studio.selection.is_loading() {
            "Loading entry data..."
        } else if let Some(e) = &studio.selection.source_error {
            e.as_str()
        } else {
            "Select an entry to view its bytes."
        };
        ui.label(egui::RichText::new(msg).weak());
        return;
    }
    status_line(st, ui);
    ui.separator();
    hex_area(st, ui);
}

fn toolbar(st: &mut HexState, ui: &mut egui::Ui, jobs: &JobSystem) {
    ui.horizontal(|ui| {
        let goto = ui.add(
            egui::TextEdit::singleline(&mut st.goto_text)
                .id(egui::Id::unique(GOTO_ID))
                .hint_text("Goto: 0x1A0, +16, end")
                .desired_width(150.0),
        );
        if st.request_goto_focus {
            st.request_goto_focus = false;
            goto.request_focus();
        }
        if (goto.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            || ui.button("Go").clicked()
        {
            st.goto();
        }
        ui.separator();
        let find = ui.add(
            egui::TextEdit::singleline(&mut st.find_text)
                .hint_text("Find: DE AD ?? EF or \"text\"")
                .desired_width(190.0),
        );
        let enter = find.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if st.find_job.is_some() {
            ui.spinner();
            if ui.button("Cancel").clicked()
                && let Some((j, _)) = &st.find_job
            {
                j.cancel();
            }
        } else if ui.button("Find next").clicked() || enter {
            st.find_next(jobs);
        }
        if let Some((m, err)) = &st.message {
            let c = if *err {
                ui.visuals().warn_fg_color
            } else {
                ui.visuals().weak_text_color()
            };
            ui.colored_label(c, m);
        }
    });
}

fn status_line(st: &mut HexState, ui: &mut egui::Ui) {
    let len = st.len();
    let mut text = format!(
        "{}  |  {} ({} bytes)",
        crate::util::truncate(&st.label, 48),
        format_size(len),
        format_count(len)
    );
    if let Some((s, n)) = st.selection() {
        text.push_str(&format!(
            "  |  sel {s:#x}..{:#x} ({} bytes)",
            s + n - 1,
            format_count(n)
        ));
    }
    if let (Some(c), Some(src)) = (st.cursor, st.source.as_ref())
        && let Ok(b) = read_vec(src.as_ref(), c, 8, 8)
    {
        let r = u_rome_core::Reader::new(&b);
        if let Ok(v) = r.u8_at(0) {
            text.push_str(&format!("  |  u8 {v}"));
        }
        if let Ok(v) = r.u16_at(0) {
            text.push_str(&format!("  u16 {v}"));
        }
        if let Ok(v) = r.u32_at(0) {
            text.push_str(&format!("  u32 {v} ({v:#x})"));
        }
        if let Ok(v) = r.f32_at(0) {
            text.push_str(&format!("  f32 {v}"));
        }
    }
    ui.label(egui::RichText::new(text).small().monospace().weak());
}

fn hex_area(st: &mut HexState, ui: &mut egui::Ui) {
    let font: FontId = egui::TextStyle::Monospace.resolve(ui.style());
    let (char_w, glyph_h) = ui
        .ctx()
        .fonts_mut(|f| (f.glyph_width(&font, '0'), f.row_height(&font)));
    let row_h = glyph_h + 2.0;
    let bpr = st.bytes_per_row;
    let len = st.len();

    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let area = Rect::from_min_max(full.min, Pos2::new(full.max.x - SCROLLBAR_W, full.max.y));
    let track = Rect::from_min_max(Pos2::new(area.max.x, full.min.y), full.max);
    st.visible_rows = ((area.height() / row_h).floor() as u64).max(1);

    let id = ui.scope_id().with("hex_area");
    let resp = ui.interact(area, id, Sense::click_and_drag());
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            id,
            egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        )
    });

    let digits = offset_digits(len);
    let off_x = area.left() + 6.0;
    let hex_x = off_x + (digits as f32 + 2.0) * char_w;
    let hex_chars = hex_column(bpr - 1) + 2;
    let ascii_x = hex_x + (hex_chars as f32 + 2.0) * char_w;

    let byte_at = |pos: Pos2, top: u64| -> Option<u64> {
        let row = ((pos.y - area.top()) / row_h).floor().max(0.0) as u64;
        let col = if pos.x >= ascii_x {
            (((pos.x - ascii_x) / char_w).floor().max(0.0) as usize).min(bpr - 1)
        } else if pos.x >= hex_x - char_w {
            let c = ((pos.x - hex_x) / char_w).floor().max(0.0) as usize;
            hex::byte_at_column(c, bpr).or_else(|| hex::byte_at_column(c.saturating_sub(1), bpr))?
        } else {
            0
        };
        let off = (top + row) * bpr as u64 + col as u64;
        (len > 0).then(|| off.min(len - 1))
    };

    // Mouse wheel.
    if resp.hovered() {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            st.scroll_px += dy;
            let rows = (st.scroll_px / row_h).trunc();
            st.scroll_px -= rows * row_h;
            if rows > 0.0 {
                st.top_row = st.top_row.saturating_sub(rows as u64);
            } else if rows < 0.0 {
                st.top_row = st.top_row.saturating_add((-rows) as u64);
            }
        }
    }

    // Selection with the mouse.
    let shift = ui.input(|i| i.modifiers.shift);
    if resp.is_pointer_button_down_on() || resp.clicked() {
        resp.request_focus();
    }
    if let Some(pos) = resp.interact_pointer_pos() {
        if resp.drag_started() || (resp.clicked() && !resp.dragged()) {
            if let Some(b) = byte_at(pos, st.top_row) {
                if !(shift && st.anchor.is_some()) {
                    st.anchor = Some(b);
                }
                st.cursor = Some(b);
            }
        } else if resp.dragged() {
            if pos.y < area.top() {
                st.top_row = st.top_row.saturating_sub(1);
            } else if pos.y > area.bottom() {
                st.top_row += 1;
            }
            let clamped = Pos2::new(pos.x, pos.y.clamp(area.top(), area.bottom() - 1.0));
            if let Some(b) = byte_at(clamped, st.top_row) {
                st.cursor = Some(b);
            }
        }
    }

    // Keyboard.
    if resp.has_focus() && len > 0 {
        use egui::Key;
        let (keys, copy) = ui.input(|i| {
            let k = [
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::ArrowUp,
                Key::ArrowDown,
                Key::PageUp,
                Key::PageDown,
                Key::Home,
                Key::End,
            ]
            .map(|k| i.key_pressed(k));
            let copy = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            (k, copy)
        });
        let cmd = ui.input(|i| i.modifiers.command);
        let cur = st.cursor.unwrap_or(0);
        let page = st.visible_rows.max(1) * bpr as u64;
        let b = bpr as u64;
        let next = if keys[0] {
            Some(cur.saturating_sub(1))
        } else if keys[1] {
            Some(cur + 1)
        } else if keys[2] {
            Some(cur.saturating_sub(b))
        } else if keys[3] {
            Some(cur + b)
        } else if keys[4] {
            Some(cur.saturating_sub(page))
        } else if keys[5] {
            Some(cur + page)
        } else if keys[6] {
            Some(if cmd { 0 } else { cur - cur % b })
        } else if keys[7] {
            Some(if cmd { len - 1 } else { cur - cur % b + b - 1 })
        } else {
            None
        };
        if let Some(n) = next {
            let n = n.min(len - 1);
            if !shift || st.anchor.is_none() {
                st.anchor = Some(n);
            }
            st.cursor = Some(n);
            st.ensure_visible(n);
        }
        if copy {
            st.copy(ui.ctx(), CopyFormat::Hex);
        }
    }

    resp.context_menu(|ui| {
        let has_sel = st.selection().is_some();
        for (label, f) in [
            ("Copy as hex", CopyFormat::Hex),
            ("Copy as ASCII", CopyFormat::Ascii),
            ("Copy as C array", CopyFormat::CArray),
            ("Copy offset", CopyFormat::Offset),
        ] {
            if ui.add_enabled(has_sel, egui::Button::new(label)).clicked() {
                st.copy(ui.ctx(), f);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Select all").clicked() {
            st.anchor = Some(0);
            st.cursor = Some(len.saturating_sub(1));
            ui.close();
        }
    });

    // Scrollbar.
    let total = st.total_rows();
    let max_top = st.max_top();
    st.top_row = st.top_row.min(max_top);
    let sid = ui.scope_id().with("hex_scrollbar");
    let sresp = ui.interact(track, sid, Sense::click_and_drag());
    let thumb_h = if total == 0 {
        track.height()
    } else {
        (track.height() * (st.visible_rows as f64 / total as f64).min(1.0) as f32).max(24.0)
    };
    let travel = (track.height() - thumb_h).max(1.0);
    if let Some(p) = sresp.interact_pointer_pos()
        && max_top > 0
    {
        let grab = *st.scroll_grab.get_or_insert_with(|| {
            let thumb_top = track.top() + travel * (st.top_row as f64 / max_top as f64) as f32;
            let off = p.y - thumb_top;
            if (0.0..=thumb_h).contains(&off) {
                off
            } else {
                thumb_h / 2.0
            }
        });
        let frac = ((p.y - grab - track.top()) / travel).clamp(0.0, 1.0) as f64;
        st.top_row = (frac * max_top as f64).round() as u64;
    }
    if !sresp.is_pointer_button_down_on() {
        st.scroll_grab = None;
    }

    // Paint.
    let painter = ui.painter_at(full);
    let vis = ui.visuals().clone();
    painter.rect_filled(area, 0.0, vis.extreme_bg_color);
    let start = st.top_row * bpr as u64;
    let want = (st.visible_rows + 1) * bpr as u64;
    let sel = st.selection();
    let cursor = st.cursor;
    let focused = resp.has_focus();
    let data = st.window(start, want).to_vec();
    let text_c = vis.text_color();
    let weak_c = vis.weak_text_color();
    let sel_fill = vis
        .selection
        .bg_fill
        .gamma_multiply(if focused { 1.0 } else { 0.6 });
    for (r, chunk) in data.chunks(bpr).enumerate() {
        let y = area.top() + r as f32 * row_h + 1.0;
        if y > area.bottom() {
            break;
        }
        let row_off = start + (r * bpr) as u64;
        painter.text(
            Pos2::new(off_x, y),
            Align2::LEFT_TOP,
            format!("{row_off:0digits$X}"),
            font.clone(),
            weak_c,
        );
        for (i, b) in chunk.iter().enumerate() {
            let off = row_off + i as u64;
            let hx = hex_x + hex_column(i) as f32 * char_w;
            let ax = ascii_x + i as f32 * char_w;
            let in_sel = sel.is_some_and(|(s, n)| off >= s && off < s + n);
            if in_sel {
                painter.rect_filled(
                    Rect::from_min_size(
                        Pos2::new(hx - 1.0, y - 1.0),
                        Vec2::new(char_w * 2.0 + 2.0, row_h),
                    ),
                    0.0,
                    sel_fill,
                );
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(ax, y - 1.0), Vec2::new(char_w, row_h)),
                    0.0,
                    sel_fill,
                );
            }
            if cursor == Some(off) {
                let stroke = Stroke::new(1.0, vis.selection.stroke.color);
                painter.rect_stroke(
                    Rect::from_min_size(
                        Pos2::new(hx - 1.0, y - 1.0),
                        Vec2::new(char_w * 2.0 + 2.0, row_h),
                    ),
                    0.0,
                    stroke,
                    StrokeKind::Inside,
                );
                painter.rect_stroke(
                    Rect::from_min_size(Pos2::new(ax, y - 1.0), Vec2::new(char_w, row_h)),
                    0.0,
                    stroke,
                    StrokeKind::Inside,
                );
            }
            let c = if *b == 0 { weak_c } else { text_c };
            painter.text(
                Pos2::new(hx, y),
                Align2::LEFT_TOP,
                format!("{b:02X}"),
                font.clone(),
                c,
            );
        }
        let (_, ascii) = format_row(chunk, chunk.len());
        painter.text(
            Pos2::new(ascii_x, y),
            Align2::LEFT_TOP,
            ascii,
            font.clone(),
            text_c,
        );
    }
    // Separators and scrollbar.
    let sep = Stroke::new(1.0, vis.widgets.noninteractive.bg_stroke.color);
    for x in [hex_x - char_w, ascii_x - char_w] {
        painter.line_segment([Pos2::new(x, area.top()), Pos2::new(x, area.bottom())], sep);
    }
    painter.rect_filled(track, 0.0, vis.faint_bg_color);
    let thumb_top = if max_top == 0 {
        track.top()
    } else {
        track.top() + travel * (st.top_row as f64 / max_top as f64) as f32
    };
    let thumb = Rect::from_min_size(
        Pos2::new(track.left() + 2.0, thumb_top),
        Vec2::new(SCROLLBAR_W - 4.0, thumb_h),
    );
    let thumb_c = if sresp.hovered() || sresp.dragged() {
        vis.widgets.hovered.bg_fill
    } else {
        vis.widgets.inactive.bg_fill
    };
    painter.rect_filled(thumb, 3.0, thumb_c);
    if focused {
        painter.rect_stroke(
            area,
            0.0,
            Stroke::new(1.0, Color32::from_gray(90)),
            StrokeKind::Inside,
        );
    }
}
