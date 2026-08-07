//! The frosted, Preview-like look. When `glass` is on (OS vibrancy active),
//! panel fills are translucent so the blurred desktop reads through; the
//! solid variant uses the same design language with opaque fills.

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};

pub const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x7c, 0xf6);

fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn apply(ctx: &egui::Context, glass: bool) {
    let dark = ctx.theme() == egui::Theme::Dark;
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let panel_base = if dark {
        Color32::from_rgb(26, 27, 32)
    } else {
        Color32::from_rgb(243, 244, 247)
    };
    let panel_alpha = if glass { 205 } else { 255 };
    visuals.panel_fill = alpha(panel_base, panel_alpha);
    visuals.window_fill = alpha(panel_base, if glass { 235 } else { 255 });

    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(70),
    };

    visuals.selection.bg_fill = alpha(ACCENT, 70);
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;

    let widget_alpha = if glass { 160 } else { 255 };
    let widget_base = if dark {
        Color32::from_rgb(48, 50, 58)
    } else {
        Color32::from_rgb(228, 230, 235)
    };
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(7);
        w.weak_bg_fill = alpha(widget_base, widget_alpha);
    }
    visuals.widgets.hovered.weak_bg_fill = alpha(widget_base, 255);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(7);

    visuals.extreme_bg_color = if dark {
        alpha(Color32::from_rgb(14, 14, 18), if glass { 170 } else { 255 })
    } else {
        alpha(Color32::WHITE, if glass { 190 } else { 255 })
    };

    ctx.set_visuals(visuals);
}

/// Fill for the page canvas backdrop: translucent when glass so the OS blur
/// shows in the margins around pages; the pages themselves stay opaque white.
pub fn canvas_fill(ctx: &egui::Context, glass: bool) -> Color32 {
    let dark = ctx.theme() == egui::Theme::Dark;
    match (dark, glass) {
        (true, true) => Color32::from_rgba_unmultiplied(18, 18, 24, 150),
        (true, false) => Color32::from_gray(45),
        (false, true) => Color32::from_rgba_unmultiplied(210, 213, 220, 130),
        (false, false) => Color32::from_gray(180),
    }
}
