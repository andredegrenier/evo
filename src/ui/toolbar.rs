//! Tool buttons and style controls.

use eframe::egui::{self, Color32};

use crate::doc::annotation::Color;
use crate::state::DocState;
use crate::tools::ActiveTool;
use crate::ui::canvas;

const TOOLS: [(ActiveTool, &str, &str); 9] = [
    (ActiveTool::Select, "☰", "Select (V)"),
    (ActiveTool::Pan, "✋", "Pan (Space)"),
    (ActiveTool::Highlight, "🖍", "Highlight (H)"),
    (ActiveTool::Text, "T", "Text (T)"),
    (ActiveTool::Rect, "▢", "Rectangle (R)"),
    (ActiveTool::Ellipse, "◯", "Ellipse (O)"),
    (ActiveTool::Line, "╱", "Line (L)"),
    (ActiveTool::Arrow, "➚", "Arrow (A)"),
    (ActiveTool::Pen, "✏", "Pen (P)"),
];

pub fn show(ui: &mut egui::Ui, dc: &mut DocState) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(dc.history.can_undo(), egui::Button::new("⟲"))
            .on_hover_text("Undo (⌘Z)")
            .clicked()
        {
            if dc.editing_text.is_some() {
                canvas::commit_text_edit(dc);
            }
            dc.history.undo(&mut dc.store, &mut dc.pages);
        }
        if ui
            .add_enabled(dc.history.can_redo(), egui::Button::new("⟳"))
            .on_hover_text("Redo (⇧⌘Z)")
            .clicked()
        {
            dc.history.redo(&mut dc.store, &mut dc.pages);
        }

        ui.separator();

        for (tool, icon, tip) in TOOLS {
            let selected = dc.tool == tool;
            let button = egui::Button::new(icon).selected(selected);
            if ui.add(button).on_hover_text(tip).clicked() {
                if dc.editing_text.is_some() {
                    canvas::commit_text_edit(dc);
                }
                dc.tool = tool;
            }
        }

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
