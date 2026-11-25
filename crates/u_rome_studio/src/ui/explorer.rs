//! Explorer: lazy tree of archives > entries > nested entries. Children are
//! listed on a worker the first time a node is expanded; the visible rows are
//! flattened once per change and drawn virtualised.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eframe::egui;

use crate::app::Studio;
use crate::backend::{Backend, EntryId, EntryInfo, MAX_DEPTH};
use crate::jobs::{JobHandle, JobSystem};
use crate::util::{format_count, format_size};

enum Children {
    Loading(JobHandle<Vec<EntryInfo>>),
    Loaded(Vec<EntryInfo>),
    Failed(String),
}

#[derive(Clone, Copy)]
enum Row {
    /// Entry `idx` in container `parent` (`None` = roots).
    Entry {
        depth: u16,
        parent: Option<EntryId>,
        idx: u32,
    },
    Loading {
        depth: u16,
    },
    Failed {
        depth: u16,
        parent: EntryId,
    },
}

#[derive(Default)]
pub struct ExplorerState {
    roots: Vec<EntryInfo>,
    roots_error: Option<String>,
    children: HashMap<EntryId, Children>,
    expanded: HashSet<EntryId>,
    rows: Vec<Row>,
    dirty: bool,
    scroll_to: Option<EntryId>,
}

impl ExplorerState {
    pub fn reset(&mut self, backend: &dyn Backend) {
        *self = Self::default();
        match backend.roots() {
            Ok(r) => self.roots = r,
            Err(e) => {
                log::error!("listing archives failed: {e}");
                self.roots_error = Some(e.to_string());
            }
        }
        self.dirty = true;
    }

    fn entry(&self, parent: Option<EntryId>, idx: u32) -> Option<&EntryInfo> {
        match parent {
            None => self.roots.get(idx as usize),
            Some(p) => match self.children.get(&p) {
                Some(Children::Loaded(v)) => v.get(idx as usize),
                _ => None,
            },
        }
    }

    fn toggle(&mut self, e: &EntryInfo, jobs: &JobSystem, backend: &Arc<dyn Backend>) {
        if !self.expanded.remove(&e.id) {
            self.expanded.insert(e.id);
            self.children
                .entry(e.id)
                .or_insert_with(|| Children::Loading(spawn_list(jobs, Arc::clone(backend), e)));
        }
        self.dirty = true;
    }

    /// Collects finished child listings.
    pub fn poll(&mut self) {
        let mut done = Vec::new();
        for (id, c) in &self.children {
            if let Children::Loading(j) = c
                && let Some(r) = j.try_take()
            {
                done.push((*id, r));
            }
        }
        for (id, r) in done {
            let c = match r {
                Ok(v) => Children::Loaded(v),
                Err(e) => {
                    log::warn!("listing children of {id:#x} failed: {e}");
                    Children::Failed(e.to_string())
                }
            };
            self.children.insert(id, c);
            self.dirty = true;
        }
    }

    /// Expands every ancestor of `id` (listing synchronously where needed)
    /// and scrolls to it.
    pub fn reveal(&mut self, backend: &dyn Backend, id: EntryId) {
        let Ok(chain) = crate::backend::location_chain(backend, id) else {
            return;
        };
        for anc in chain.iter().take(chain.len().saturating_sub(1)) {
            self.expanded.insert(anc.id);
            if !matches!(self.children.get(&anc.id), Some(Children::Loaded(_))) {
                let c = match backend.children(anc.id) {
                    Ok(v) => Children::Loaded(v),
                    Err(e) => Children::Failed(e.to_string()),
                };
                self.children.insert(anc.id, c);
            }
        }
        self.scroll_to = Some(id);
        self.dirty = true;
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        // Explicit stack: (container, depth, next index).
        let mut stack: Vec<(Option<EntryId>, u16, u32)> = vec![(None, 0, 0)];
        while let Some((parent, depth, i)) = stack.pop() {
            let list: &[EntryInfo] = match parent {
                None => &self.roots,
                Some(p) => match self.children.get(&p) {
                    Some(Children::Loaded(v)) => v,
                    _ => &[],
                },
            };
            let Some(e) = list.get(i as usize) else {
                continue;
            };
            stack.push((parent, depth, i + 1));
            rows.push(Row::Entry {
                depth,
                parent,
                idx: i,
            });
            if self.expanded.contains(&e.id) && (depth as usize) < MAX_DEPTH {
                match self.children.get(&e.id) {
                    Some(Children::Loaded(_)) => stack.push((Some(e.id), depth + 1, 0)),
                    Some(Children::Loading(_)) | None => {
                        rows.push(Row::Loading { depth: depth + 1 })
                    }
                    Some(Children::Failed(_)) => rows.push(Row::Failed {
                        depth: depth + 1,
                        parent: e.id,
                    }),
                }
            }
        }
        self.rows = rows;
        self.dirty = false;
    }

    fn row_of(&self, id: EntryId) -> Option<usize> {
        self.rows.iter().position(|r| match r {
            Row::Entry { parent, idx, .. } => self.entry(*parent, *idx).is_some_and(|e| e.id == id),
            _ => false,
        })
    }

    pub fn expanded_count(&self) -> usize {
        self.expanded.len()
    }
}

fn spawn_list(
    jobs: &JobSystem,
    backend: Arc<dyn Backend>,
    e: &EntryInfo,
) -> JobHandle<Vec<EntryInfo>> {
    let id = e.id;
    let name = crate::util::truncate(&e.display_name(), 40);
    jobs.spawn(format!("List {name}"), move |ctx| {
        ctx.check()?;
        backend.children(id).map_err(Into::into)
    })
}

pub fn show(studio: &mut Studio, ui: &mut egui::Ui) {
    let Some(backend) = studio.backend.clone() else {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            if studio.is_loading_backend() {
                ui.spinner();
                ui.label("Loading install...");
            } else {
                ui.label("No install loaded.");
                ui.label(
                    egui::RichText::new("File > Open game folder, or File > Load demo data.")
                        .weak(),
                );
            }
        });
        return;
    };
    let st = &mut studio.explorer;
    if let Some(e) = &st.roots_error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Listing archives failed: {e}"),
        );
    }
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{} archives", format_count(st.roots.len() as u64))).weak(),
        );
        if ui.small_button("Collapse all").clicked() {
            st.expanded.clear();
            st.dirty = true;
        }
    });
    ui.separator();
    if st.dirty {
        st.rebuild();
    }

    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 3.0;
    let mut scroll = egui::ScrollArea::both()
        .auto_shrink([false, false])
        .id_salt("explorer_rows");
    if let Some(target) = st.scroll_to.take()
        && let Some(r) = st.row_of(target)
    {
        let y = r as f32 * (row_h + ui.spacing().item_spacing.y);
        scroll = scroll.vertical_scroll_offset((y - 100.0).max(0.0));
    }
    let selected = studio.selection.id;
    let mut clicked: Option<EntryId> = None;
    let mut toggled: Option<EntryInfo> = None;
    let total = st.rows.len();
    scroll.show_rows(ui, row_h, total, |ui, range| {
        for r in range {
            let Some(row) = st.rows.get(r).copied() else {
                continue;
            };
            match row {
                Row::Entry { depth, parent, idx } => {
                    let Some(e) = st.entry(parent, idx) else {
                        continue;
                    };
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        ui.add_space(f32::from(depth) * 14.0);
                        if e.is_container() {
                            let open = st.expanded.contains(&e.id);
                            let sym = if open { "-" } else { "+" };
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new(sym).monospace())
                                        .small()
                                        .frame(false),
                                )
                                .clicked()
                            {
                                toggled = Some(e.clone());
                            }
                        } else {
                            ui.add_space(ui.spacing().interact_size.y * 0.6);
                        }
                        ui.label(super::kind_label(ui, e.kind));
                        let mut text = egui::RichText::new(e.display_name());
                        if e.name.is_none() {
                            text = text.monospace().weak();
                        }
                        let resp = ui.selectable_label(selected == Some(e.id), text);
                        if resp.clicked() {
                            clicked = Some(e.id);
                        }
                        if resp.double_clicked() && e.is_container() {
                            toggled = Some(e.clone());
                        }
                        let mut size = format_size(e.size);
                        if let Some(n) = e.child_count.filter(|n| *n > 0) {
                            size = format!("{size}, {} items", format_count(u64::from(n)));
                        }
                        ui.label(egui::RichText::new(size).small().weak());
                        if e.provisional {
                            ui.label(
                                egui::RichText::new("provisional")
                                    .small()
                                    .color(ui.visuals().warn_fg_color),
                            );
                        }
                    });
                }
                Row::Loading { depth } => {
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        ui.add_space(f32::from(depth) * 14.0 + 16.0);
                        ui.label(egui::RichText::new("loading...").weak().italics());
                    });
                }
                Row::Failed { depth, parent } => {
                    let msg = match st.children.get(&parent) {
                        Some(Children::Failed(m)) => m.clone(),
                        _ => String::new(),
                    };
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        ui.add_space(f32::from(depth) * 14.0 + 16.0);
                        ui.colored_label(ui.visuals().error_fg_color, format!("error: {msg}"));
                    });
                }
            }
        }
    });
    if let Some(e) = toggled {
        st.toggle(&e, &studio.jobs, &backend);
    }
    if let Some(id) = clicked {
        studio.select(id);
    }
}
