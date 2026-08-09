//! The properties panel: exact numeric X/Y/W/H editing, one-click page
//! centering, and style controls for the selected annotation.

use eframe::egui::{self, DragValue};

use crate::doc::annotation::{Annotation, AnnotationKind};
use crate::doc::geometry::{
    CLOUD_INTENSITY_MAX, CLOUD_INTENSITY_MIN, PdfPoint, PdfRect, clamp_cloud_intensity,
};
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

    // The sequence tool is a setting, not a selection: what it will place next
    // belongs in the panel whether or not anything is selected.
    if dc.tool == crate::tools::ActiveTool::Sequence {
        sequence_settings(ui, dc);
        ui.separator();
    }

    let Some(ann) = dc.selected_annotation().cloned() else {
        ui.weak(
            "No selection.\n\nSelect a markup to edit its exact position and size, or drag a \
             box over several with the Select tool.",
        );
        return;
    };

    selection_summary(ui, dc, &ann);
    ui.add_space(4.0);

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

    if let AnnotationKind::Polygon { cloudy, .. } = ann.kind {
        let mut on = cloudy.is_some();
        if ui
            .checkbox(&mut on, "Cloudy edge")
            .on_hover_text("Draw the outline as a revision cloud")
            .changed()
        {
            record_style(dc, ann.clone(), &|a| {
                if let AnnotationKind::Polygon { cloudy, .. } = &mut a.kind {
                    *cloudy = on.then_some(crate::tools::DEFAULT_CLOUD_INTENSITY);
                }
            });
        }
        if let Some(current) = cloudy {
            let mut intensity = current;
            let slider = ui.add(
                egui::Slider::new(&mut intensity, CLOUD_INTENSITY_MIN..=CLOUD_INTENSITY_MAX)
                    .text("Scallops"),
            );
            if slider.drag_started() || slider.gained_focus() {
                dc.tool_ctl.inspector_before = Some(ann.clone());
            }
            if slider.changed()
                && let Some(a) = dc.store.get_mut(ann.id)
                && let AnnotationKind::Polygon { cloudy, .. } = &mut a.kind
            {
                *cloudy = Some(clamp_cloud_intensity(intensity));
            }
            if (slider.drag_stopped() || slider.lost_focus())
                && let Some(before) = dc.tool_ctl.inspector_before.take()
                && let Some(after) = dc.store.get(ann.id).cloned()
                && after != before
            {
                dc.history
                    .record(Command::ModifyAnnotation { before, after });
            }
        }
    }

    if let AnnotationKind::Stamp { text, font_size } = &ann.kind {
        let mut edited = text.clone();
        ui.label("Text");
        if ui
            .add(egui::TextEdit::singleline(&mut edited).desired_width(f32::INFINITY))
            .changed()
        {
            record_style(dc, ann.clone(), &|a| {
                if let AnnotationKind::Stamp { text, .. } = &mut a.kind {
                    // Already-placed stamps say what they say: the tokens were
                    // spent when it was applied, and typing "%date" into one
                    // now is a word, not a date.
                    text.clone_from(&edited);
                }
            });
        }
        let mut size = *font_size;
        let fs = field(ui, "Font size", &mut size, 0.5);
        if fs.changed {
            record_style(dc, ann.clone(), &|a| {
                if let AnnotationKind::Stamp { font_size, .. } = &mut a.kind {
                    *font_size = size.clamp(4.0, 144.0);
                }
            });
        }
    }

    if let AnnotationKind::PolyLine { arrow_end, .. } = ann.kind {
        let mut on = arrow_end;
        if ui.checkbox(&mut on, "Arrowhead at the end").changed() {
            record_style(dc, ann.clone(), &|a| {
                if let AnnotationKind::PolyLine { arrow_end, .. } = &mut a.kind {
                    *arrow_end = on;
                }
            });
        }
    }

    ui.add_space(10.0);
    let label = match dc.selection.len() {
        0 | 1 => "🗑 Delete".to_owned(),
        n => format!("🗑 Delete {n}"),
    };
    if ui.button(label).clicked() {
        crate::tools::delete_selection(dc);
    }
}

/// What is selected, and the two things that can be done to a selection as a
/// whole: tie it together, or untie it.
///
/// With several selected the fields below this still edit one of them -- the
/// last one clicked -- so the panel says so rather than leaving the user to
/// discover that "W" moved only one of their four boxes.
fn selection_summary(ui: &mut egui::Ui, dc: &mut DocState, primary: &Annotation) {
    let selected = dc.selection.len();
    if selected > 1 {
        ui.label(egui::RichText::new(format!("{selected} markups selected")).strong());
        ui.weak(format!(
            "Moving and deleting apply to all {selected}. The fields below edit the last one \
             you clicked: the {} on page {}.",
            primary.kind.label(),
            primary.page + 1
        ));
    } else {
        ui.weak(format!(
            "{} on page {}",
            primary.kind.label(),
            primary.page + 1
        ));
    }

    let grouped = dc.selected_annotations().iter().any(|a| a.group.is_some());
    if selected > 1 || grouped {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(selected > 1, egui::Button::new("Group"))
                .on_hover_text("Clicking any of them selects all of them (⌘G)")
                .clicked()
            {
                crate::tools::group_selection(dc);
            }
            if ui
                .add_enabled(grouped, egui::Button::new("Ungroup"))
                .on_hover_text("Let them be selected one at a time again (⇧⌘G)")
                .clicked()
            {
                crate::tools::ungroup_selection(dc);
            }
        });
    }
}

/// What the sequence tool will place next: the prefix and the number.
///
/// Changing the prefix re-reads the document under the new one, so switching
/// from `1, 2, 3` to `A1` starts at `A1` rather than at `A4`.
fn sequence_settings(ui: &mut egui::Ui, dc: &mut DocState) {
    ui.label(egui::RichText::new("Sequence").strong());
    ui.horizontal(|ui| {
        ui.label("Prefix");
        let mut prefix = dc.tool_ctl.sequence.prefix.clone();
        if ui
            .add(
                egui::TextEdit::singleline(&mut prefix)
                    .desired_width(70.0)
                    .hint_text("A"),
            )
            .changed()
        {
            dc.tool_ctl.sequence.prefix = prefix;
            dc.tool_ctl.sequence.next =
                crate::tools::next_sequence_number(&dc.store, &dc.tool_ctl.sequence.prefix);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Next");
        ui.add(
            DragValue::new(&mut dc.tool_ctl.sequence.next)
                .speed(1.0)
                .range(0..=99_999),
        );
    });
    ui.weak(format!(
        "Clicking the page places {}{}.",
        dc.tool_ctl.sequence.prefix, dc.tool_ctl.sequence.next
    ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::annotation::{AnnotationKind, Style};
    use crate::doc::geometry::PdfPoint;
    use crate::state::DocState;

    /// The panel has a row per kind now, and a row that is never drawn is a
    /// row nobody notices is broken. One frame per new kind, with the controls
    /// left alone, must change nothing.
    #[test]
    fn the_panel_draws_for_the_shapes_v0_6_added() {
        let ctx = egui::Context::default();
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        let mut dc = DocState::new(doc, &ctx, crate::render::engine::EnginePref::Hayro);
        let points = vec![
            PdfPoint::new(100.0, 600.0),
            PdfPoint::new(300.0, 600.0),
            PdfPoint::new(200.0, 700.0),
        ];

        for kind in [
            AnnotationKind::Polygon {
                points: points.clone(),
                cloudy: Some(1.5),
            },
            AnnotationKind::Polygon {
                points: points.clone(),
                cloudy: None,
            },
            AnnotationKind::PolyLine {
                points: points.clone(),
                arrow_end: true,
            },
            AnnotationKind::Stamp {
                text: "APPROVED".into(),
                font_size: 20.0,
            },
            AnnotationKind::ImageStamp {
                png: crate::export::pdf::tests::png_fixture(8, 8),
            },
        ] {
            let id = dc.store.alloc_id();
            let before = Annotation {
                id,
                page: 0,
                kind,
                rect: crate::tools::pen::bounding_rect(&points),
                style: Style::default(),
                group: None,
            };
            dc.store.insert(before.clone());
            dc.selection.select_one(id);

            let ctx = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(320.0, 700.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| show(ui, &mut dc));

            assert_eq!(dc.store.get(id), Some(&before), "drawing changed it");
            assert!(!dc.history.can_undo(), "drawing recorded history");
        }
    }

    /// The panel with several selected: it says how many, it says which one the
    /// fields below are about, and it offers the two things that apply to a
    /// selection as a whole. Drawing it may change nothing.
    #[test]
    fn the_panel_says_what_a_multiple_selection_is_and_leaves_it_alone() {
        let ctx = egui::Context::default();
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        let mut dc = DocState::new(doc, &ctx, crate::render::engine::EnginePref::Hayro);
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = dc.store.alloc_id();
            dc.store.insert(Annotation {
                id,
                page: 0,
                kind: AnnotationKind::Rect,
                rect: PdfRect::from_min_size(PdfPoint::new(100.0 * i as f32, 400.0), 50.0, 30.0),
                style: Style::default(),
                group: if i == 0 { None } else { Some(9) },
            });
            ids.push(id);
        }
        dc.selection.select_all(ids.clone());
        let before = dc.store.to_vec();

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(320.0, 900.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| show(ui, &mut dc));

        assert_eq!(dc.selection.len(), 3, "drawing changed the selection");
        assert_eq!(dc.store.to_vec(), before, "drawing changed the markup");
        assert!(!dc.history.can_undo(), "drawing recorded history");
    }

    /// The sequence tool's settings are shown whether or not anything is
    /// selected -- they are what the *next* click will do, and there is
    /// nothing selected at the moment it matters most.
    #[test]
    fn the_sequence_settings_are_shown_while_the_tool_is_active() {
        let ctx = egui::Context::default();
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        let mut dc = DocState::new(doc, &ctx, crate::render::engine::EnginePref::Hayro);
        dc.tool = crate::tools::ActiveTool::Sequence;
        dc.tool_ctl.sequence.prefix = "A".into();
        dc.tool_ctl.sequence.next = 7;

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(320.0, 700.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| show(ui, &mut dc));

        assert_eq!(dc.tool_ctl.sequence.prefix, "A", "drawing changed it");
        assert_eq!(dc.tool_ctl.sequence.next, 7);
        assert!(dc.selection.is_empty(), "and nothing had to be selected");
    }
}
