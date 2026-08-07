//! The frosted, Preview-like look. When `glass` is on (OS vibrancy active),
//! panel fills stay a touch translucent so the blurred desktop reads through;
//! like Preview's own chrome they are nearly opaque, and the solid variant
//! uses the same design language with fully opaque fills.

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke, Vec2};

use super::ACCENT;

fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn visuals(dark: bool, glass: bool) -> egui::Visuals {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let panel_base = if dark {
        Color32::from_rgb(30, 30, 34)
    } else {
        Color32::from_rgb(246, 246, 248)
    };
    let panel_alpha = if glass { 245 } else { 255 };
    visuals.panel_fill = alpha(panel_base, panel_alpha);
    visuals.window_fill = alpha(panel_base, if glass { 250 } else { 255 });

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

    let widget_alpha = if glass { 235 } else { 255 };
    let widget_base = if dark {
        Color32::from_rgb(58, 58, 64)
    } else {
        Color32::from_rgb(232, 232, 236)
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
        alpha(Color32::from_rgb(14, 14, 18), panel_alpha)
    } else {
        alpha(Color32::WHITE, panel_alpha)
    };

    visuals
}

/// Preview-like control metrics: roomy buttons on a tight grid. Shared by
/// every theme so switching palettes never reflows the chrome.
pub fn apply_spacing(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.button_padding = Vec2::new(8.0, 5.0);
        style.spacing.item_spacing = Vec2::new(6.0, 6.0);
        style.spacing.interact_size = Vec2::new(28.0, 28.0);
    });
}

/// Backdrop behind the pages. Glass keeps a hint of translucency in the page
/// margins only; the pages themselves are painted opaque white.
pub fn canvas_fill(dark: bool, glass: bool) -> Color32 {
    match (dark, glass) {
        (true, true) => Color32::from_rgba_unmultiplied(24, 24, 28, 235),
        (true, false) => Color32::from_gray(45),
        (false, true) => Color32::from_rgba_unmultiplied(216, 218, 224, 235),
        (false, false) => Color32::from_gray(180),
    }
}
