//! Search: query box, kind filter and a virtualised results table.

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use u_rome_core::AssetKind;

use crate::app::Studio;
use crate::search::{self, ALL_KINDS, SearchQuery, SortKey};
use crate::util::{format_count, format_size};

/// Upper bound on materialised results.
pub const MAX_RESULTS: usize = 2_000_000;

pub struct SearchState {
    pub query: String,
    pub kind: Option<AssetKind>,
    pub request_focus: bool,
    results: Vec<u32>,
    computed_for: Option<(SearchQuery, u64)>,
    sort: (SortKey, bool),
    truncated: bool,
    elapsed_ms: f64,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            kind: None,
            request_focus: false,
            results: Vec::new(),
            computed_for: None,
            sort: (SortKey::Index, false),
            truncated: false,
            elapsed_ms: 0.0,
        }
    }
}

impl SearchState {
    pub fn invalidate(&mut self) {
        self.computed_for = None;
    }

    pub fn result_count(&self) -> Option<usize> {
        self.computed_for.as_ref().map(|_| self.results.len())
    }
}

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let st = &mut studio.search;
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut st.query)
                .id(egui::Id::unique("search_query"))
                .hint_text("Search names, 0xHASH, kind:tex, >1mb, -exclude")
                .desired_width((ui.available_width() - 150.0).max(120.0)),
        );
        if st.request_focus {
            st.request_focus = false;
            resp.request_focus();
        }
        egui::ComboBox::from_id_salt("search_kind")
            .width(130.0)
            .selected_text(st.kind.map(|k| k.label()).unwrap_or("All kinds"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut st.kind, None, "All kinds");
                for k in ALL_KINDS {
                    ui.selectable_value(&mut st.kind, Some(k), k.label());
                }
            });
    })
    .response
    .on_hover_text(
        "Terms are AND-ed. word, -word, 0x1234ABCD (hash or offset), kind:mesh, \
         codec:zstd, >1mb, <10k, is:provisional, is:unnamed, is:container",
    );

    let Some(index) = studio.index.clone() else {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(studio.index_status()).weak());
        return;
    };

    let q = SearchQuery {
        text: st.query.clone(),
        kind: st.kind,
    };
    let key = (q, studio.index_generation);
    if st.computed_for.as_ref() != Some(&key) {
        let t = std::time::Instant::now();
        st.results = search::filter(&index, &key.0, MAX_RESULTS);
        st.truncated = st.results.len() >= MAX_RESULTS;
        if st.sort.0 != SortKey::Index || st.sort.1 {
            search::sort_results(&index, &mut st.results, st.sort.0, st.sort.1);
        }
        st.elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
        st.computed_for = Some(key);
    }
    ui.label(
        egui::RichText::new(format!(
            "{} of {} entries{} ({:.1} ms)",
            format_count(st.results.len() as u64),
            format_count(index.len() as u64),
            if st.truncated { ", truncated" } else { "" },
            st.elapsed_ms
        ))
        .small()
        .weak(),
    );

    let selected = studio.selection.id;
    let mut clicked = None;
    let mut revealed = None;
    let mut resort: Option<SortKey> = None;
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let header = |ui: &mut egui::Ui,
                  label: &str,
                  key: SortKey,
                  sort: (SortKey, bool),
                  out: &mut Option<SortKey>| {
        let arrow = if sort.0 == key {
            if sort.1 { " v" } else { " ^" }
        } else {
            ""
        };
        if ui
            .add(
                egui::Button::new(egui::RichText::new(format!("{label}{arrow}")).strong())
                    .frame(false),
            )
            .clicked()
        {
            *out = Some(key);
        }
    };
    let sort = st.sort;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink([false, false])
        .sense(egui::Sense::click())
        .column(Column::exact(44.0))
        .column(Column::initial(260.0).at_least(80.0).clip(true))
        .column(Column::initial(80.0).at_least(50.0))
        .column(Column::initial(90.0).at_least(60.0))
        .column(Column::remainder().clip(true))
        .header(row_h, |mut h| {
            h.col(|ui| header(ui, "Kind", SortKey::Kind, sort, &mut resort));
            h.col(|ui| header(ui, "Name", SortKey::Name, sort, &mut resort));
            h.col(|ui| header(ui, "Size", SortKey::Size, sort, &mut resort));
            h.col(|ui| header(ui, "Offset", SortKey::Offset, sort, &mut resort));
            h.col(|ui| {
                ui.strong("Path");
            });
        })
        .body(|body| {
            body.rows(row_h, st.results.len(), |mut row| {
                let Some(&i) = st.results.get(row.index()) else {
                    return;
                };
                let Some(e) = index.row(i) else {
                    return;
                };
                row.set_selected(selected == Some(e.id));
                row.col(|ui| {
                    ui.label(super::kind_label(ui, e.kind));
                });
                row.col(|ui| {
                    let mut t = egui::RichText::new(e.display_name());
                    if e.name.is_none() {
                        t = t.monospace().weak();
                    }
                    ui.label(t);
                });
                row.col(|ui| {
                    ui.label(format_size(e.size));
                });
                row.col(|ui| {
                    ui.monospace(format!("{:#010x}", e.offset));
                });
                row.col(|ui| {
                    ui.label(egui::RichText::new(index.path(i).unwrap_or_default()).weak());
                });
                let resp = row.response();
                if resp.clicked() {
                    clicked = Some(e.id);
                }
                if resp.double_clicked() {
                    revealed = Some(e.id);
                }
            });
        });

    if let Some(k) = resort {
        st.sort = if st.sort.0 == k {
            (k, !st.sort.1)
        } else {
            (k, false)
        };
        search::sort_results(&index, &mut st.results, st.sort.0, st.sort.1);
    }
    if let Some(id) = clicked {
        studio.select(id);
    }
    if let Some(id) = revealed {
        studio.reveal(id);
    }
}
