//! Page thumbnail sidebar: navigate, drag to reorder, right-click to
//! rotate/delete.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::doc::history::Command;
use crate::render::RenderRequest;
use crate::state::DocState;
use crate::ui::canvas::THUMB_SCALE;

const THUMB_WIDTH: f32 = 120.0;

pub fn show(ui: &mut egui::Ui, dc: &mut DocState) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(4.0);
            let order = dc.pages.order.clone();
            let mut reorder: Option<(usize, usize)> = None;

            for (position, original) in order.iter().copied().enumerate() {
                let info = dc.doc.pages[original];
                let rotation = dc.pages.rotation_of(original);
                let (pw, ph) = if rotation.swaps_axes() {
                    (info.height, info.width)
                } else {
                    (info.width, info.height)
                };
                let thumb_h = THUMB_WIDTH * ph / pw.max(1.0);
                let desired = Vec2::new(THUMB_WIDTH, thumb_h);

                let id = ui.id().with("thumb").with(position);
                let response = ui
                    .dnd_drag_source(id, position, |ui| {
                        ui.vertical_centered(|ui| {
                            let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
                            paint_thumb(ui, dc, original, rect, rotation);
                            ui.label(format!("{}", position + 1));
                            resp
                        })
                        .inner
                    })
                    .response;

                let inner_resp = response.clone();

                // Click -> jump to page.
                if inner_resp.clicked() {
                    dc.viewport.scroll_to_page = Some(position);
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
                        Stroke::new(2.0, Color32::from_rgb(0x2f, 0x7c, 0xf6)),
                        StrokeKind::Outside,
                    );
                }

                inner_resp.context_menu(|ui| {
                    if ui.button("Rotate Clockwise").clicked() {
                        let before = dc.pages.clone();
                        dc.pages.rotate_cw(original);
                        record_pages(dc, before);
                        ui.close();
                    }
                    if ui.button("Rotate Counter-Clockwise").clicked() {
                        let before = dc.pages.clone();
                        dc.pages.rotate_ccw(original);
                        record_pages(dc, before);
                        ui.close();
                    }
                    ui.separator();
                    let deletable = dc.pages.len() > 1;
                    if ui
                        .add_enabled(deletable, egui::Button::new("Delete Page"))
                        .clicked()
                    {
                        let before = dc.pages.clone();
                        dc.pages.delete_at(position);
                        record_pages(dc, before);
                        ui.close();
                    }
                });

                ui.add_space(6.0);
            }

            if let Some((from, to)) = reorder {
                let before = dc.pages.clone();
                dc.pages.reorder(from, to);
                record_pages(dc, before);
            }
        });
}

fn record_pages(dc: &mut DocState, before: crate::doc::page_ops::PageList) {
    let after = dc.pages.clone();
    dc.history.record(Command::SetPageList { before, after });
}

fn paint_thumb(
    ui: &egui::Ui,
    dc: &mut DocState,
    original: usize,
    rect: Rect,
    rotation: crate::doc::geometry::ExtraRotation,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), Color32::WHITE);
    let tex = dc.thumb_cache.get(original, THUMB_SCALE);
    if let Some(tex) = tex {
        super::canvas::paint_rotated_texture_pub(painter, &tex, rect, rotation);
    } else if !dc.thumb_cache.is_pending(original, THUMB_SCALE) {
        dc.thumb_cache.mark_pending(original, THUMB_SCALE);
        dc.worker.request(RenderRequest {
            page: original,
            scale: THUMB_SCALE,
        });
    }
    painter.rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0, Color32::from_gray(180)),
        StrokeKind::Outside,
    );
}
