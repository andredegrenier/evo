//! Tool buttons and style controls.

use eframe::egui::{self, Color32};
use egui_phosphor::regular as icon;

use crate::doc::annotation::Color;
use crate::state::DocState;
use crate::tools::ActiveTool;
use crate::ui::canvas;

const ICON_SIZE: f32 = 16.0;
const BUTTON_SIZE: egui::Vec2 = egui::Vec2::new(28.0, 28.0);

const TOOLS: [(ActiveTool, &str, &str); 9] = [
    (ActiveTool::Select, icon::CURSOR, "Select (V)"),
    (ActiveTool::Pan, icon::HAND, "Pan (Space)"),
    (ActiveTool::Highlight, icon::HIGHLIGHTER, "Highlight (H)"),
    (ActiveTool::Text, icon::TEXT_T, "Text (T)"),
    (ActiveTool::Rect, icon::RECTANGLE, "Rectangle (R)"),
    (ActiveTool::Ellipse, icon::CIRCLE, "Ellipse (O)"),
    (ActiveTool::Line, icon::LINE_SEGMENT, "Line (L)"),
    (ActiveTool::Arrow, icon::ARROW_UP_RIGHT, "Arrow (A)"),
    (ActiveTool::Pen, icon::SCRIBBLE, "Pen (P)"),
];

fn icon_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(ICON_SIZE)).min_size(BUTTON_SIZE)
}

pub fn show(ui: &mut egui::Ui, dc: &mut DocState) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                dc.history.can_undo(),
                icon_button(icon::ARROW_COUNTER_CLOCKWISE),
            )
            .on_hover_text("Undo (⌘Z)")
            .clicked()
        {
            if dc.editing_text.is_some() {
                canvas::commit_text_edit(dc);
            }
            dc.history.undo(&mut dc.store, &mut dc.pages);
        }
        if ui
            .add_enabled(dc.history.can_redo(), icon_button(icon::ARROW_CLOCKWISE))
            .on_hover_text("Redo (⇧⌘Z)")
            .clicked()
        {
            dc.history.redo(&mut dc.store, &mut dc.pages);
        }

        ui.separator();

        // The tools read as one segmented control, Preview-style.
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(8)
            .inner_margin(2.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (tool, glyph, tip) in TOOLS {
                    let selected = dc.tool == tool;
                    let button = icon_button(glyph).selected(selected);
                    if ui.add(button).on_hover_text(tip).clicked() {
                        if dc.editing_text.is_some() {
                            canvas::commit_text_edit(dc);
                        }
                        dc.tool = tool;
                    }
                }
            });

        ui.separator();

        // Stroke color.
        let mut stroke = to_egui(dc.current_style.stroke);
        ui.label("Stroke");
        if ui.color_edit_button_srgba(&mut stroke).changed() {
            dc.current_style.stroke = from_egui(stroke);
        }

        let mut fill = to_egui(dc.current_style.fill);
        ui.label("Fill");
        if ui.color_edit_button_srgba(&mut fill).changed() {
            dc.current_style.fill = from_egui(fill);
        }

        ui.label("Width");
        ui.add(
            egui::DragValue::new(&mut dc.current_style.stroke_width)
                .range(0.5..=24.0)
                .speed(0.1),
        );

        ui.label("Font");
        ui.add(
            egui::DragValue::new(&mut dc.current_font_size)
                .range(6.0..=96.0)
                .speed(0.5),
        );
    });
}

pub fn to_egui(c: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

pub fn from_egui(c: Color32) -> Color {
    Color::rgba(c.r(), c.g(), c.b(), c.a())
}
