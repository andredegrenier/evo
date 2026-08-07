//! The library home view, shown when no document is open: a card grid of
//! imported PDFs with import, open, and delete actions.

use std::collections::HashMap;

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::library::{DocMeta, Library, spawn_thumbnail_job};
use crate::ui::theme::ACCENT;

const CARD_W: f32 = 168.0;
const THUMB_H: f32 = 190.0;

#[derive(Default)]
pub struct LibraryViewState {
    docs: Vec<DocMeta>,
    loaded: bool,
    pub query: String,
    /// Full-text results for the current query (None = show the card grid).
    search_hits: Option<Vec<crate::library::search::SearchHit>>,
    thumbs: HashMap<String, egui::TextureHandle>,
    /// Thumbnails we've already kicked off background renders for.
    requested: HashMap<String, ()>,
}

impl LibraryViewState {
    pub fn mark_dirty(&mut self) {
        self.loaded = false;
    }
}

pub enum LibraryAction {
    Open(String),
    /// Open a document and scroll to a source page (from a search result).
    OpenAtPage(String, usize),
    Error(String),
}

pub fn show(
    ui: &mut egui::Ui,
    library: &Library,
    state: &mut LibraryViewState,
) -> Option<LibraryAction> {
    let mut action = None;

    if !state.loaded {
        match library.list() {
            Ok(docs) => {
                state.docs = docs;
                state.loaded = true;
            }
            Err(e) => return Some(LibraryAction::Error(e.to_string())),
        }
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.heading("Library");
        ui.add_space(12.0);
        if ui.button("＋ Import PDFs…").clicked()
            && let Some(files) = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .pick_files()
        {
            for file in files {
                match library.import(&file) {
                    Ok(meta) => {
                        if let Ok(bytes) = library.load_bytes(&meta.id) {
                            spawn_thumbnail_job(
                                std::sync::Arc::new(bytes),
                                library.thumb_path(&meta.id),
                                ui.ctx().clone(),
                            );
                        }
                    }
                    Err(e) => action = Some(LibraryAction::Error(e.to_string())),
                }
            }
            state.mark_dirty();
        }
        if let Some(status) = library.index_status()
            && status.pending > 0
        {
            ui.weak(format!(
                "Indexing {} document{}…",
                status.pending,
                if status.pending == 1 { "" } else { "s" }
            ));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.query)
                    .hint_text("🔍 Search titles and contents…")
                    .desired_width(240.0),
            );
            if resp.changed() {
                state.search_hits = if state.query.trim().is_empty() {
                    None
                } else {
                    library.search(state.query.trim()).ok()
                };
            }
        });
    });
    ui.add_space(8.0);
    ui.separator();

    if state.docs.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak("No documents yet. Import PDFs or drop them here.");
        });
        return action;
    }

    if state.search_hits.is_some() {
        let hits = state.search_hits.clone().unwrap_or_default();
        show_search_results(ui, &hits, &mut action);
        return action;
    }

    let query = state.query.to_lowercase();
    let visible: Vec<DocMeta> = state
        .docs
        .iter()
        .filter(|d| query.is_empty() || d.title.to_lowercase().contains(&query))
        .cloned()
        .collect();

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(16.0, 20.0);
                ui.add_space(12.0);
                for meta in &visible {
                    if let Some(a) = doc_card(ui, library, state, meta) {
                        action = Some(a);
                    }
                }
            });
            ui.add_space(16.0);
        });
    action
}

fn doc_card(
    ui: &mut egui::Ui,
    library: &Library,
    state: &mut LibraryViewState,
    meta: &DocMeta,
) -> Option<LibraryAction> {
    let mut action = None;
    ui.allocate_ui(Vec2::new(CARD_W, THUMB_H + 44.0), |ui| {
        ui.vertical(|ui| {
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(CARD_W, THUMB_H), Sense::click());

            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(6), Color32::WHITE);

            match thumb_texture(ui, library, state, &meta.id) {
                Some(tex) => {
                    let size = tex.size_vec2();
                    let scale = (rect.width() / size.x).min(rect.height() / size.y).min(1.0);
                    let draw = Rect::from_center_size(rect.center(), size * scale);
                    painter.image(
                        tex.id(),
                        draw,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                None => {
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "📄",
                        egui::FontId::proportional(40.0),
                        Color32::from_gray(150),
                    );
                }
            }
            let stroke = if response.hovered() {
                Stroke::new(2.0, ACCENT)
            } else {
                Stroke::new(1.0, Color32::from_gray(180))
            };
            painter.rect_stroke(rect, CornerRadius::same(6), stroke, StrokeKind::Outside);

            if response.clicked() {
                action = Some(LibraryAction::Open(meta.id.clone()));
            }
            response.context_menu(|ui| {
                if ui.button("Open").clicked() {
                    action = Some(LibraryAction::Open(meta.id.clone()));
                    ui.close();
                }
                ui.separator();
                if ui.button("Delete from Library").clicked() {
                    if let Err(e) = library.delete(&meta.id) {
                        action = Some(LibraryAction::Error(e.to_string()));
                    }
                    state.mark_dirty();
                    ui.close();
                }
            });

            ui.add_space(4.0);
            ui.scope(|ui| {
                ui.set_max_width(CARD_W);
                ui.label(egui::RichText::new(&meta.title).strong().size(13.0));
                ui.weak(format!(
                    "{} page{}",
                    meta.page_count,
                    if meta.page_count == 1 { "" } else { "s" }
                ));
            });
        });
    });
    action
}

fn thumb_texture(
    ui: &egui::Ui,
    library: &Library,
    state: &mut LibraryViewState,
    id: &str,
) -> Option<egui::TextureHandle> {
    if let Some(tex) = state.thumbs.get(id) {
        return Some(tex.clone());
    }
    let path = library.thumb_path(id);
    if path.exists() {
        let bytes = std::fs::read(&path).ok()?;
        let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
        let (w, h) = img.dimensions();
        let color =
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
        let tex = ui.ctx().load_texture(
            format!("lib-thumb-{id}"),
            color,
            egui::TextureOptions::LINEAR,
        );
        state.thumbs.insert(id.to_owned(), tex.clone());
        Some(tex)
    } else {
        // Kick off a render once; the repaint arrives when the PNG lands.
        if !state.requested.contains_key(id)
            && let Ok(bytes) = library.load_bytes(id)
        {
            state.requested.insert(id.to_owned(), ());
            spawn_thumbnail_job(std::sync::Arc::new(bytes), path, ui.ctx().clone());
        }
        None
    }
}

fn show_search_results(
    ui: &mut egui::Ui,
    hits: &[crate::library::search::SearchHit],
    action: &mut Option<LibraryAction>,
) {
    use eframe::egui::text::LayoutJob;
    use eframe::egui::{FontId, TextFormat};

    if hits.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak("No matches. Documents still being indexed will appear once ready.");
        });
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(8.0);
            for hit in hits {
                let response = egui::Frame::group(ui.style())
                    .corner_radius(8)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width() - 24.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&hit.title).strong());
                            ui.weak(format!("p. {}", hit.page + 1));
                        });
                        // Snippet with highlighted match ranges.
                        let mut job = LayoutJob::default();
                        let normal = TextFormat {
                            font_id: FontId::proportional(12.5),
                            color: ui.visuals().text_color(),
                            ..Default::default()
                        };
                        let highlight = TextFormat {
                            font_id: FontId::proportional(12.5),
                            color: ui.visuals().strong_text_color(),
                            background: ACCENT.gamma_multiply(0.35),
                            ..Default::default()
                        };
                        let text = &hit.snippet;
                        let mut cursor = 0;
                        for range in &hit.highlights {
                            let (start, end) =
                                (range.start.min(text.len()), range.end.min(text.len()));
                            if start > cursor
                                && text.is_char_boundary(cursor)
                                && text.is_char_boundary(start)
                            {
                                job.append(&text[cursor..start], 0.0, normal.clone());
                            }
                            if end > start
                                && text.is_char_boundary(start)
                                && text.is_char_boundary(end)
                            {
                                job.append(&text[start..end], 0.0, highlight.clone());
                            }
                            cursor = end.max(cursor);
                        }
                        if cursor < text.len() && text.is_char_boundary(cursor) {
                            job.append(&text[cursor..], 0.0, normal);
                        }
                        ui.label(job);
                    })
                    .response;
                if response.interact(egui::Sense::click()).clicked() {
                    *action = Some(LibraryAction::OpenAtPage(hit.doc_id.clone(), hit.page));
                }
                ui.add_space(6.0);
            }
        });
}
