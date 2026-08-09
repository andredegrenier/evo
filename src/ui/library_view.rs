//! The library home view, shown when no document is open: a card grid of
//! imported PDFs with import, open, and delete actions.

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2};

use egui_phosphor::regular as icon;

use crate::library::enrich::EnrichStatus;
use crate::library::indexer::IndexStatus;
use crate::library::{DocMeta, Library, PageTextStatus, spawn_thumbnail_job};
use crate::ui::theme::ACCENT;

const CARD_W: f32 = 168.0;
const THUMB_H: f32 = 190.0;
/// How much of a summary fits under a card's title before it is cut.
const SUMMARY_CHARS: usize = 90;
/// Tags shown in full on a card; the rest become "+n".
const TAG_CHIPS: usize = 3;
/// How often to re-poll the indexer while it has work outstanding.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(600);

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
    /// Last observed indexer queue depth; a change means the stored per-page
    /// statuses moved on and the card metadata needs re-reading.
    last_pending: Option<usize>,
    /// Same idea for summaries: a new one changes the card under it.
    last_enriched: Option<usize>,
    /// ⌘F on the library home focuses the search field.
    pub focus_search_pending: bool,
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
    /// Files the person picked. The app imports them rather than this view:
    /// a protected one needs a password, and asking for one is the app's
    /// modal, not the library grid's.
    Import(Vec<PathBuf>),
    Error(String),
}

pub fn show(
    ui: &mut egui::Ui,
    library: &Library,
    state: &mut LibraryViewState,
    pref: crate::render::engine::EnginePref,
) -> Option<LibraryAction> {
    let mut action = None;

    let status = library.index_status();
    let enrich = library.enrich_status();
    if let Some(st) = &status {
        if st.pending > 0 || st.ocr_pending > 0 {
            ui.ctx().request_repaint_after(POLL_INTERVAL);
        }
        if state.last_pending != Some(st.pending) {
            state.last_pending = Some(st.pending);
            state.mark_dirty();
        }
    }
    if let Some(st) = &enrich {
        if st.pending > 0 {
            ui.ctx().request_repaint_after(POLL_INTERVAL);
        }
        // A finished summary changes a card, so re-read when the count moves.
        if state.last_enriched != Some(st.done) {
            state.last_enriched = Some(st.done);
            state.mark_dirty();
        }
    }

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
        if ui.button(format!("{} Import PDFs…", icon::PLUS)).clicked()
            && let Some(files) = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .pick_files()
        {
            action = Some(LibraryAction::Import(files));
        }
        if let Some(st) = &status {
            index_activity(ui, library, st);
        }
        if let Some(st) = &enrich {
            enrich_activity(ui, library, st);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.query)
                    .hint_text(format!(
                        "{} Search titles and contents…",
                        icon::MAGNIFYING_GLASS
                    ))
                    .desired_width(240.0),
            );
            if state.focus_search_pending {
                resp.request_focus();
                state.focus_search_pending = false;
            }
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
                    if let Some(a) = doc_card(ui, library, state, meta, status.as_ref(), pref) {
                        action = Some(a);
                    }
                }
            });
            ui.add_space(16.0);
        });
    action
}

/// What the indexer is doing right now, plus a dismissible error banner.
fn index_activity(ui: &mut egui::Ui, library: &Library, status: &IndexStatus) {
    if let Some(current) = &status.current {
        ui.add(egui::Spinner::new().size(14.0));
        if status.ocr_total > 0 {
            ui.weak(format!(
                "{current} ({}/{} pages)",
                status.ocr_done, status.ocr_total
            ));
        } else {
            ui.weak(current);
        }
    } else if status.pending > 0 {
        ui.add(egui::Spinner::new().size(14.0));
        ui.weak(format!(
            "Indexing {} document{}…",
            status.pending,
            if status.pending == 1 { "" } else { "s" }
        ));
    }
    if let Some(error) = &status.last_error {
        ui.label(
            egui::RichText::new(format!("{} indexing problem", icon::WARNING_CIRCLE))
                .color(ui.visuals().warn_fg_color),
        )
        .on_hover_text(error);
        if ui
            .small_button(icon::X)
            .on_hover_text("Dismiss this message")
            .clicked()
        {
            library.clear_index_error();
        }
    }
}

/// What the summarizer is doing, and why it stopped if it did. Same shape as
/// the indexer's line so the header does not turn into a status console.
fn enrich_activity(ui: &mut egui::Ui, library: &Library, status: &EnrichStatus) {
    if let Some(current) = &status.current {
        ui.add(egui::Spinner::new().size(14.0));
        ui.weak(format!("Summarizing {current}"));
    } else if status.pending > 0 {
        ui.add(egui::Spinner::new().size(14.0));
        ui.weak(format!(
            "Summarizing {} document{}…",
            status.pending,
            if status.pending == 1 { "" } else { "s" }
        ));
    }
    if let Some(error) = &status.last_error {
        ui.label(
            egui::RichText::new(format!("{} summaries paused", icon::WARNING_CIRCLE))
                .color(ui.visuals().warn_fg_color),
        )
        .on_hover_text(error);
        if ui
            .small_button(icon::X)
            .on_hover_text("Dismiss this message")
            .clicked()
        {
            library.clear_enrich_error();
        }
    }
}

/// The summary and tags a card shows, if the document has been described.
fn card_summary(ui: &mut egui::Ui, meta: &DocMeta) {
    if let Some(summary) = &meta.summary {
        let short = shorten(summary, SUMMARY_CHARS);
        let label = ui.weak(egui::RichText::new(&short).size(11.0));
        if short != *summary {
            label.on_hover_text(summary);
        }
    }

    let tags = meta.all_tags();
    if tags.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 2.0);
        for tag in tags.iter().take(TAG_CHIPS) {
            chip(ui, tag);
        }
        if tags.len() > TAG_CHIPS {
            ui.weak(egui::RichText::new(format!("+{}", tags.len() - TAG_CHIPS)).size(10.0))
                .on_hover_text(tags[TAG_CHIPS..].join(", "));
        }
    });
}

fn chip(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(Stroke::new(
            1.0,
            ui.visuals().weak_text_color().gamma_multiply(0.4),
        ))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(10.0));
        });
}

/// `text` cut to `max` characters on a word boundary, with an ellipsis.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .nth(max)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let head = &text[..cut];
    let head = match head.rfind(char::is_whitespace) {
        Some(i) if i > max / 2 => &head[..i],
        _ => head,
    };
    format!("{}…", head.trim_end_matches([' ', ',', ';', '.']))
}

/// Per-document indexing badges, derived from the stored per-page statuses.
fn card_badges(ui: &mut egui::Ui, meta: &DocMeta, status: Option<&IndexStatus>) {
    let has = |want: PageTextStatus| meta.text_status.contains(&want);
    let (pending, failed, ocr) = (
        has(PageTextStatus::Pending),
        has(PageTextStatus::Failed),
        has(PageTextStatus::Ocr),
    );
    if !(pending || failed || ocr) {
        return;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if pending {
            ui.add(egui::Spinner::new().size(11.0));
            let current = status
                .filter(|s| s.current_id.as_deref() == Some(meta.id.as_str()))
                .filter(|s| s.ocr_total > 0);
            let label = match current {
                Some(s) => format!("OCR {}/{}…", s.ocr_done, s.ocr_total),
                None => "Indexing…".to_owned(),
            };
            ui.weak(egui::RichText::new(label).size(11.0));
        }
        if failed {
            ui.label(
                egui::RichText::new(icon::WARNING_CIRCLE)
                    .size(13.0)
                    .color(ui.visuals().warn_fg_color),
            )
            .on_hover_text(
                meta.index_error
                    .as_deref()
                    .unwrap_or("some pages could not be indexed"),
            );
        }
        if ocr && !pending {
            ui.weak(egui::RichText::new("OCR").size(10.0))
                .on_hover_text("Text on scanned pages was recovered by OCR; it is searchable.");
        }
    });
}

fn doc_card(
    ui: &mut egui::Ui,
    library: &Library,
    state: &mut LibraryViewState,
    meta: &DocMeta,
    status: Option<&IndexStatus>,
    pref: crate::render::engine::EnginePref,
) -> Option<LibraryAction> {
    let mut action = None;
    ui.allocate_ui(Vec2::new(CARD_W, THUMB_H + 64.0), |ui| {
        ui.vertical(|ui| {
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(CARD_W, THUMB_H), Sense::click());

            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(6), Color32::WHITE);

            match thumb_texture(ui, library, state, &meta.id, pref) {
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
                        icon::FILE_PDF,
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
                if ui
                    .button(format!("{} Re-index (repeat OCR)", icon::ARROW_CLOCKWISE))
                    .clicked()
                {
                    if let Err(e) = library.reindex(&meta.id) {
                        action = Some(LibraryAction::Error(e.to_string()));
                    }
                    state.mark_dirty();
                    ui.close();
                }
                ui.separator();
                if ui
                    .button(format!("{} Delete from Library", icon::TRASH))
                    .clicked()
                {
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
                card_badges(ui, meta, status);
                card_summary(ui, meta);
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
    pref: crate::render::engine::EnginePref,
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
            spawn_thumbnail_job(std::sync::Arc::new(bytes), path, ui.ctx().clone(), pref);
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
                            if hit.is_summary {
                                ui.weak("summary")
                                    .on_hover_text("Matched the summary or tags of this document");
                            } else {
                                ui.weak(format!("p. {}", hit.page + 1));
                            }
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
