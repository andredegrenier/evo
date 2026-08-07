//! The properties panel: exact numeric X/Y/W/H editing, one-click page
//! centering, and style controls for the selected annotation.

use eframe::egui::{self, DragValue};

use crate::doc::annotation::{Annotation, AnnotationKind};
use crate::doc::geometry::{PdfPoint, PdfRect};
use crate::doc::history::Command;
use crate::state::DocState;
use crate::ui::ribbon::{from_egui, to_egui};

struct FieldResponse {
    changed: bool,
    started: bool,
    ended: bool,
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut f32, speed: f32) -> FieldResponse {
    let mut out = FieldResponse {
        changed: false,
        started: false,
        ended: false,
    };
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(DragValue::new(value).speed(speed).max_decimals(2));
        out.changed = resp.changed();
        out.started = resp.drag_started() || resp.gained_focus();
        out.ended = resp.drag_stopped() || resp.lost_focus();
    });
    out
}

pub fn show(ui: &mut egui::Ui, dc: &mut DocState) {
    ui.heading("Inspector");
    ui.separator();

    let Some(ann) = dc.selected_annotation().cloned() else {
        ui.weak("No selection.\n\nSelect a markup to edit its exact position and size.");
        return;
    };

    let info = dc.doc.pages[dc.pages.source_of(ann.page)];
    let (page_w, page_h) = (info.width, info.height);

    // Position/size in points; Y shown from the top of the page (like
    // Bluebeam/Preview users expect), converted to y-up internally.
    let rect = ann.rect;
    let mut x = rect.min.x;
    let mut y_top = page_h - rect.max.y;
    let mut w = rect.width();
    let mut h = rect.height();

    ui.label(egui::RichText::new("Position & Size (pt)").strong());
    let fx = field(ui, "X", &mut x, 0.5);
    let fy = field(ui, "Y", &mut y_top, 0.5);
    let fw = field(ui, "W", &mut w, 0.5);
    let fh = field(ui, "H", &mut h, 0.5);

    let any_changed = fx.changed || fy.changed || fw.changed || fh.changed;
    let any_started = fx.started || fy.started || fw.started || fh.started;
    let any_ended = fx.ended || fy.ended || fw.ended || fh.ended;

    if any_started && dc.tool_ctl.inspector_before.is_none() {
        dc.tool_ctl.inspector_before = Some(ann.clone());
    }

    if any_changed {
        let w = w.max(0.1);
        let h = h.max(0.1);
        let new_rect = PdfRect::from_min_size(PdfPoint::new(x, page_h - y_top - h), w, h);
        let mut updated = ann.clone();
        updated.set_bounds(new_rect);
        dc.store.replace(updated);
    }

    if any_ended
        && let Some(before) = dc.tool_ctl.inspector_before.take()
        && let Some(after) = dc.store.get(ann.id).cloned()
        && after != before
    {
        dc.history
            .record(Command::ModifyAnnotation { before, after });
    }

    ui.add_space(6.0);
    ui.label(egui::RichText::new("Align on page").strong());
    ui.horizontal(|ui| {
        if ui
            .button("Center H")
            .on_hover_text("Center horizontally on the page")
            .clicked()
        {
            center(dc, &ann, page_w / 2.0, None);
        }
        if ui
            .button("Center V")
            .on_hover_text("Center vertically on the page")
            .clicked()
        {
            center(dc, &ann, page_h / 2.0, Some(()));
        }
    });

    ui.add_space(6.0);
    ui.label(egui::RichText::new("Style").strong());

    let record_style = |dc: &mut DocState, before: Annotation, mutate: &dyn Fn(&mut Annotation)| {
        let mut after = before.clone();
        mutate(&mut after);
        if after != before {
            dc.store.replace(after.clone());
            dc.history
                .record(Command::ModifyAnnotation { before, after });
        }
    };

    ui.horizontal(|ui| {
        ui.label("Stroke");
        let mut stroke = to_egui(ann.style.stroke);
        if ui.color_edit_button_srgba(&mut stroke).changed() {
            record_style(dc, ann.clone(), &|a| a.style.stroke = from_egui(stroke));
        }
        ui.label("Fill");
        let mut fill = to_egui(ann.style.fill);
        if ui.color_edit_button_srgba(&mut fill).changed() {
            record_style(dc, ann.clone(), &|a| a.style.fill = from_egui(fill));
        }
    });

    let mut width = ann.style.stroke_width;
    let fwd = field(ui, "Stroke width", &mut width, 0.1);
    if fwd.changed {
        record_style(dc, ann.clone(), &|a| {
            a.style.stroke_width = width.clamp(0.0, 24.0)
        });
    }

    let mut opacity = ann.style.opacity * 100.0;
    let fop = field(ui, "Opacity %", &mut opacity, 1.0);
    if fop.changed {
        record_style(dc, ann.clone(), &|a| {
            a.style.opacity = (opacity / 100.0).clamp(0.05, 1.0)
        });
    }

    if let AnnotationKind::TextBox {
        font_size, align, ..
    } = ann.kind
    {
        let mut size = font_size;
        let fs = field(ui, "Font size", &mut size, 0.5);
        if fs.changed {
            record_style(dc, ann.clone(), &|a| {
                if let AnnotationKind::TextBox { font_size, .. } = &mut a.kind {
                    *font_size = size.clamp(4.0, 144.0);
                }
            });
        }
        ui.horizontal(|ui| {
            ui.label("Align");
            for (label, value) in [
                ("Left", crate::doc::annotation::TextAlign::Left),
                ("Center", crate::doc::annotation::TextAlign::Center),
                ("Right", crate::doc::annotation::TextAlign::Right),
            ] {
                if ui.selectable_label(align == value, label).clicked() && align != value {
                    record_style(dc, ann.clone(), &|a| {
                        if let AnnotationKind::TextBox { align, .. } = &mut a.kind {
                            *align = value;
                        }
                    });
                }
            }
        });
    }

    ui.add_space(10.0);
    if ui.button("🗑 Delete").clicked() {
        if let Some(removed) = dc.store.remove(ann.id) {
            dc.history.record(Command::RemoveAnnotation(removed));
        }
        dc.selection = None;
    }
}

/// One-click centering: move the annotation so its center lands on the page
/// centerline. `vertical` = Some -> center vertically, None -> horizontally.
fn center(dc: &mut DocState, ann: &Annotation, target: f32, vertical: Option<()>) {
    let before = ann.clone();
    let mut after = ann.clone();
    let c = ann.rect.center();
    match vertical {
        None => after.translate(target - c.x, 0.0),
        Some(()) => after.translate(0.0, target - c.y),
    }
    if after != before {
        dc.store.replace(after.clone());
        dc.history
            .record(Command::ModifyAnnotation { before, after });
    }
}
