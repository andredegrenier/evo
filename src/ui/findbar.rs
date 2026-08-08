//! Preview-style find-in-document bar (⌘F): a floating field over the canvas
//! with a match counter, next/previous stepping, and live highlights painted
//! by the canvas.
//!
//! Text comes from [`crate::library::textjob::TextWorker`], which extracts
//! embedded text and OCRs scanned pages in the background.

use std::ops::Range;
use std::path::PathBuf;

use eframe::egui::{self, Key};
use egui_phosphor::regular as icon;

use crate::library::extract::{self, LineLayout, PageTextLayout, TextSource};
use crate::library::textjob::TextWorker;
use crate::state::{DocState, FindMatch};

/// Draw the find bar over `canvas` and keep its matches in sync. `models_dir`
/// enables OCR for scanned pages when the models are already downloaded.
pub fn show(
    ctx: &egui::Context,
    dc: &mut DocState,
    models_dir: Option<PathBuf>,
    canvas: egui::Rect,
) {
    if !dc.find.open {
        return;
    }

    if dc.text_worker.is_none() && dc.page_text.len() < dc.doc.pages.len() {
        dc.text_worker = Some(TextWorker::spawn(
            dc.doc.source.clone(),
            models_dir,
            ctx.clone(),
        ));
    }
    drain_worker(dc);

    if dc.find.dirty || dc.find.query != dc.find.last_query {
        let query_changed = dc.find.query != dc.find.last_query;
        recompute(dc);
        dc.find.dirty = false;
        dc.find.last_query = dc.find.query.clone();
        if query_changed {
            dc.find.active = 0;
            scroll_to_active(dc);
        } else if dc.find.active >= dc.find.matches.len() {
            dc.find.active = 0;
        }
    }

    let mut close = false;
    let mut step_by: i32 = 0;
    // Float over the canvas's top-right corner, clear of the surrounding
    // panels (menu, toolbar, inspector).
    egui::Area::new(egui::Id::new("find-bar"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
        .constrain_to(canvas)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut dc.find.query)
                            .hint_text("Find in document")
                            .desired_width(200.0),
                    );
                    if dc.find.focus_pending {
                        field.request_focus();
                        dc.find.focus_pending = false;
                    }
                    // egui drops focus on Enter, so step and take it back.
                    if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        step_by = if ui.input(|i| i.modifiers.shift) {
                            -1
                        } else {
                            1
                        };
                        field.request_focus();
                    }

                    let pending = dc.page_text.len() < dc.doc.pages.len();
                    if pending {
                        ui.add(egui::Spinner::new().size(14.0))
                            .on_hover_text("Reading the document's text…");
                    }
                    let counter = ui.label(counter_label(dc, pending));
                    if let Some(detail) = counter_hover(dc) {
                        counter.on_hover_text(detail);
                    }

                    let has = !dc.find.matches.is_empty();
                    if ui
                        .add_enabled(has, egui::Button::new(icon::CARET_UP))
                        .on_hover_text("Previous match (⇧⏎)")
                        .clicked()
                    {
                        step_by = -1;
                    }
                    if ui
                        .add_enabled(has, egui::Button::new(icon::CARET_DOWN))
                        .on_hover_text("Next match (⏎)")
                        .clicked()
                    {
                        step_by = 1;
                    }
                    if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                        close = true;
                    }
                });
            });
        });

    if step_by != 0 {
        step(dc, step_by);
    }
    if close || ctx.input(|i| i.key_pressed(Key::Escape)) {
        dc.find.open = false;
    }
}

fn counter_label(dc: &DocState, pending: bool) -> String {
    if dc.find.query.trim().is_empty() {
        String::new()
    } else if dc.find.matches.is_empty() {
        if pending {
            "…".into()
        } else {
            "No matches".into()
        }
    } else {
        format!("{} of {}", dc.find.active + 1, dc.find.matches.len())
    }
}

/// Tooltip for the counter: the active match in context, plus a note when any
/// of the searched text was recovered by OCR.
fn counter_hover(dc: &DocState) -> Option<String> {
    let m = dc.find.matches.get(dc.find.active)?;
    let layout = dc.page_text.get(&m.source_page)?;
    let line = layout.lines.get(m.line)?;
    let mut detail = format!("Page {}: {}", m.source_page + 1, snippet(line, &m.range));
    if dc
        .page_text
        .values()
        .any(|p| p.source == Some(TextSource::Ocr))
    {
        detail.push_str("\nSome pages were read by OCR.");
    }
    Some(detail)
}

/// A little of the matched line around the match, on char boundaries.
pub fn snippet(line: &LineLayout, range: &Range<usize>) -> String {
    const CONTEXT: usize = 24;
    let text = &line.text;
    let mut start = range.start.saturating_sub(CONTEXT);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (range.end + CONTEXT).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(text[start..end].trim());
    if end < text.len() {
        out.push('…');
    }
    out
}

/// Move finished pages from the worker into the cache.
///
/// The MCP server's find tool calls this too: it reads the same cache, and a
/// caller that asks twice should see the pages that arrived in between.
pub fn drain_worker(dc: &mut DocState) {
    let Some(worker) = &dc.text_worker else {
        return;
    };
    let mut arrived: Vec<(usize, PageTextLayout)> = Vec::new();
    while let Some(page) = worker.try_recv() {
        arrived.push(page);
    }
    if arrived.is_empty() {
        return;
    }
    for (page, layout) in arrived {
        dc.page_text.insert(page, layout);
    }
    dc.find.dirty = true;
}

/// Rebuild the match list in display order, visiting each source page once.
fn recompute(dc: &mut DocState) {
    let query = dc.find.query.trim().to_owned();
    dc.find.matches = matches_for(dc, &query);
}

/// Every match for `query` in the text read so far, in display order and
/// visiting each source page once.
///
/// Shared with the MCP find tool, so what an assistant is told is where the
/// find bar would take the user.
pub fn matches_for(dc: &DocState, query: &str) -> Vec<FindMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut matches = Vec::new();
    for &logical in &dc.pages.order {
        let source = dc.pages.source_of(logical);
        if !seen.insert(source) {
            continue;
        }
        let Some(layout) = dc.page_text.get(&source) else {
            continue;
        };
        for (index, line) in layout.lines.iter().enumerate() {
            for range in extract::find_in_line(&line.text, query) {
                if let Some(rect) = extract::rect_for_range(line, range.clone()) {
                    matches.push(FindMatch {
                        source_page: source,
                        line: index,
                        range,
                        rect,
                    });
                }
            }
        }
    }
    matches
}

fn step(dc: &mut DocState, delta: i32) {
    let count = dc.find.matches.len();
    if count == 0 {
        return;
    }
    dc.find.active = (dc.find.active as i32 + delta).rem_euclid(count as i32) as usize;
    scroll_to_active(dc);
}

/// Ask the canvas to center the active match.
fn scroll_to_active(dc: &mut DocState) {
    let Some(m) = dc.find.matches.get(dc.find.active) else {
        return;
    };
    let (source, rect) = (m.source_page, m.rect);
    if let Some(position) = dc
        .pages
        .order
        .iter()
        .position(|&logical| dc.pages.source_of(logical) == source)
    {
        dc.viewport.scroll_to_rect = Some((position, rect));
    }
}
