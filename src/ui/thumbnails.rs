//! Page thumbnail sidebar: navigate, drag to reorder, multi-select
//! (click / shift-click / cmd-click), and a context menu with page
//! operations: copy/paste, rotate, delete, print selection, extract.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::doc::history::Command;
use crate::doc::page_ops::PageList;
use crate::export::pdf::ExportOptions;
use crate::render::RenderRequest;
use crate::state::DocState;
use crate::ui::canvas::THUMB_SCALE;
use crate::ui::theme::ACCENT;

const THUMB_WIDTH: f32 = 120.0;

/// Side effects the app shell must perform (they outlive the DocState borrow).
pub enum RailAction {
    /// Open these PDF bytes as a new document (Extract as New Document).
    OpenExtracted(Vec<u8>),
    /// A temp file was produced for printing; track it for cleanup.
    TempPrintFile(std::path::PathBuf),
    /// Surface an error dialog.
    Error(String),
}

pub fn show(ui: &mut egui::Ui, dc: &mut DocState) -> Option<RailAction> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(4.0);
            let order = dc.pages.order.clone();
            let mut reorder: Option<(usize, usize)> = None;

            for (position, logical) in order.iter().copied().enumerate() {
                let source = dc.pages.source_of(logical);
                let info = dc.doc.pages[source];
                let rotation = dc.pages.rotation_of(logical);
                let (pw, ph) = if rotation.swaps_axes() {
                    (info.height, info.width)
                } else {
                    (info.width, info.height)
                };
                let thumb_h = THUMB_WIDTH * ph / pw.max(1.0);
                let desired = Vec2::new(THUMB_WIDTH, thumb_h);
                let is_selected = dc.rail.selected.contains(&position);

                let id = ui.id().with("thumb").with(position);
                let response = ui
                    .dnd_drag_source(id, position, |ui| {
                        ui.vertical_centered(|ui| {
                            let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
                            paint_thumb(ui, dc, source, rect, rotation, is_selected);
                            ui.label(format!("{}", position + 1));
                            resp
                        })
                        .inner
                    })
                    .response;

                let inner_resp = response.clone();

                if inner_resp.clicked() {
                    let (shift, cmd) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
                    dc.rail.click(position, shift, cmd);
                    if !shift && !cmd {
                        dc.viewport.scroll_to_page = Some(position);
                    }
                }

                // Drop another thumb onto this one -> reorder.
                if let Some(from) = inner_resp.dnd_release_payload::<usize>()
                    && *from != position
                {
                    reorder = Some((*from, position));
                }
                if inner_resp.dnd_hover_payload::<usize>().is_some() {
                    ui.painter().rect_stroke(
                        inner_resp.rect,
                        CornerRadius::same(2),
                        Stroke::new(2.0, ACCENT),
                        StrokeKind::Outside,
                    );
                }

                inner_resp.context_menu(|ui| {
                    // Right-clicking outside the selection selects just that page.
                    if !dc.rail.selected.contains(&position) {
                        dc.rail.click(position, false, false);
                    }
                    if let Some(a) = context_menu(ui, dc, position) {
                        action = Some(a);
                    }
                });

                ui.add_space(6.0);
            }

            if let Some((from, to)) = reorder {
                let before = dc.pages.clone();
                dc.pages.reorder(from, to);
                record_pages(dc, before);
                dc.rail.clear();
            }
        });
    action
}

fn selected_positions(dc: &DocState) -> Vec<usize> {
    dc.rail.selected.iter().copied().collect()
}

fn context_menu(ui: &mut egui::Ui, dc: &mut DocState, position: usize) -> Option<RailAction> {
    let mut action = None;
    let selected = selected_positions(dc);
    let n = selected.len();
    let plural = |s: &str| {
        if n > 1 {
            format!("{s} {n} Pages")
        } else {
            format!("{s} Page")
        }
    };

    if ui.button(plural("Copy")).clicked() {
        dc.page_clipboard = selected.iter().map(|&p| dc.pages.order[p]).collect();
        ui.close();
    }
    let paste_label = format!("Paste {} After", dc.page_clipboard.len());
    if ui
        .add_enabled(
            !dc.page_clipboard.is_empty(),
            egui::Button::new(paste_label),
        )
        .clicked()
    {
        paste_after(dc, position);
        ui.close();
    }

    ui.separator();

    if ui
        .button(plural("Rotate"))
        .on_hover_text("Clockwise")
        .clicked()
    {
        let before = dc.pages.clone();
        for &p in &selected {
            dc.pages.rotate_cw(dc.pages.order[p]);
        }
        record_pages(dc, before);
        ui.close();
    }
    if ui.button("Rotate Counter-Clockwise").clicked() {
        let before = dc.pages.clone();
        for &p in &selected {
            dc.pages.rotate_ccw(dc.pages.order[p]);
        }
        record_pages(dc, before);
        ui.close();
    }

    ui.separator();

    if ui.button(plural("Print…")).clicked() {
        let subset = dc.pages.subset(&selected);
        match crate::export::print::print_via_system_viewer(&dc.doc, &subset, &dc.store) {
            Ok(temp) => action = Some(RailAction::TempPrintFile(temp)),
            Err(e) => action = Some(RailAction::Error(format!("Print failed: {e}"))),
        }
        ui.close();
    }
    if ui.button("Extract to File…").clicked() {
        action = extract_to_file(dc, &selected);
        ui.close();
    }
    if ui.button("Extract as New Document").clicked() {
        let subset = dc.pages.subset(&selected);
        match crate::export::pdf::export_pdf_bytes(
            &dc.doc,
            &subset,
            &dc.store,
            ExportOptions::default(),
        ) {
            Ok(bytes) => action = Some(RailAction::OpenExtracted(bytes)),
            Err(e) => action = Some(RailAction::Error(format!("Extract failed: {e}"))),
        }
        ui.close();
    }

    ui.separator();

    let deletable = dc.pages.len() > n;
    if ui
        .add_enabled(deletable, egui::Button::new(plural("Delete")))
        .clicked()
    {
        let before = dc.pages.clone();
        for &p in selected.iter().rev() {
            dc.pages.delete_at(p);
        }
        record_pages(dc, before);
        dc.rail.clear();
        ui.close();
    }
    action
}

/// Duplicate the clipboard's logical pages right after display position
/// `after`, cloning their annotations, as one undo step.
fn paste_after(dc: &mut DocState, after: usize) {
    let before = dc.pages.clone();
    let mut commands = Vec::new();
    let mut insert_pos = after + 1;
    for logical in dc.page_clipboard.clone() {
        if logical >= dc.pages.states.len() {
            continue; // clipboard survived an undo that removed the page
        }
        let new_logical = dc.pages.duplicate(logical, insert_pos);
        insert_pos += 1;
        let cloned: Vec<_> = dc.store.on_page(logical).cloned().collect();
        for mut ann in cloned {
            ann.id = dc.store.alloc_id();
            ann.page = new_logical;
            dc.store.insert(ann.clone());
            commands.push(Command::AddAnnotation(ann));
        }
    }
    let mut batch = vec![Command::SetPageList {
        before,
        after: dc.pages.clone(),
    }];
    batch.extend(commands);
    dc.history.record(Command::Batch(batch));
}

fn extract_to_file(dc: &mut DocState, selected: &[usize]) -> Option<RailAction> {
    let default_name = dc
        .doc
        .path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| format!("{} (pages).pdf", s.to_string_lossy()))
        .unwrap_or_else(|| "Extracted pages.pdf".into());
    let path = rfd::FileDialog::new()
        .add_filter("PDF document", &["pdf"])
        .set_file_name(default_name)
        .save_file()?;
    let subset = dc.pages.subset(selected);
    match crate::export::pdf::export_pdf(
        &dc.doc,
        &subset,
        &dc.store,
        ExportOptions::default(),
        &path,
    ) {
        Ok(()) => None,
        Err(e) => Some(RailAction::Error(format!("Extract failed: {e}"))),
    }
}

fn record_pages(dc: &mut DocState, before: PageList) {
    let after = dc.pages.clone();
    dc.history.record(Command::SetPageList { before, after });
}

fn paint_thumb(
    ui: &egui::Ui,
    dc: &mut DocState,
    source: usize,
    rect: Rect,
    rotation: crate::doc::geometry::ExtraRotation,
    is_selected: bool,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), Color32::WHITE);
    let tex = dc.thumb_cache.get(source, THUMB_SCALE);
    if let Some(tex) = tex {
        super::canvas::paint_rotated_texture_pub(painter, &tex, rect, rotation);
    } else if !dc.thumb_cache.is_pending(source, THUMB_SCALE) {
        dc.thumb_cache.mark_pending(source, THUMB_SCALE);
        dc.worker.request(RenderRequest {
            page: source,
            scale: THUMB_SCALE,
        });
    }
    if is_selected {
        painter.rect_stroke(
            rect.expand(1.5),
            CornerRadius::same(3),
            Stroke::new(2.5, ACCENT),
            StrokeKind::Outside,
        );
    } else {
        painter.rect_stroke(
            rect,
            CornerRadius::same(2),
            Stroke::new(1.0, Color32::from_gray(180)),
            StrokeKind::Outside,
        );
    }
}
