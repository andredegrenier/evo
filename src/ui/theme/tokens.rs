//! The design tokens every theme is expressed in, and the one place they are
//! turned into egui `Visuals`.
//!
//! Chrome we draw by hand -- the ribbon, its group cards, the wizard -- needs
//! the same colours and metrics as the widgets egui draws for us. Without a
//! shared vocabulary the two drift: a hand-painted card ends up a slightly
//! different grey from the panel it sits on, in one theme but not the others.

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke, Vec2};

/// A complete theme, independent of egui's own structures.
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    /// Panels: menu bar, ribbon, status bar, side panels.
    pub bg_panel: Color32,
    /// Surfaces sitting on top of a panel: ribbon groups, cards, popovers.
    pub bg_raised: Color32,
    /// Wells sitting below it: text fields, search boxes, list backgrounds.
    pub bg_sunken: Color32,
    /// The desk the pages lie on.
    pub bg_canvas: Color32,

    pub ink: Color32,
    pub ink_muted: Color32,

    pub accent: Color32,
    pub on_accent: Color32,
    pub warn: Color32,
    pub error: Color32,

    /// Hairline separators and widget outlines.
    pub outline: Color32,
    /// Outlines that need to read as an edge rather than a hint.
    pub outline_strong: Color32,

    /// Resting, hovered and pressed fills for interactive widgets.
    pub control: Color32,
    pub control_hover: Color32,
    pub control_active: Color32,

    pub radius_s: u8,
    pub radius_m: u8,
    pub radius_l: u8,

    pub space_xs: f32,
    pub space_s: f32,

    pub ribbon_height: f32,
    /// True when panels are translucent for OS blur-behind.
    pub glass: bool,
    pub dark: bool,
}

pub fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// Panels stay nearly opaque even with blur behind them, the way Preview's own
/// chrome does; full translucency makes text hard to read over a busy desktop.
pub const GLASS_PANEL_ALPHA: u8 = 245;
pub const GLASS_WINDOW_ALPHA: u8 = 250;
pub const GLASS_CANVAS_ALPHA: u8 = 235;

impl Tokens {
    /// Metrics are shared by every theme, so switching palette never reflows
    /// the chrome.
    pub fn radius(&self, r: u8) -> CornerRadius {
        CornerRadius::same(r)
    }

    pub fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.outline)
    }

    /// A raised card: the shape ribbon groups and inline panels are drawn in.
    pub fn card(&self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.bg_raised)
            .stroke(self.hairline())
            .corner_radius(self.radius(self.radius_m))
            .inner_margin(self.space_s)
    }
}

/// Build egui's `Visuals` from a token set.
pub fn visuals(t: &Tokens) -> egui::Visuals {
    let mut v = if t.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let panel_alpha = if t.glass { GLASS_PANEL_ALPHA } else { 255 };
    v.panel_fill = alpha(t.bg_panel, panel_alpha);
    v.window_fill = alpha(t.bg_raised, if t.glass { GLASS_WINDOW_ALPHA } else { 255 });
    v.extreme_bg_color = alpha(t.bg_sunken, panel_alpha);
    v.faint_bg_color = t.bg_raised;
    v.code_bg_color = t.bg_sunken;

    v.window_corner_radius = t.radius(t.radius_l);
    v.menu_corner_radius = t.radius(t.radius_l);
    v.window_stroke = Stroke::new(1.0, t.outline);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if t.dark { 90 } else { 50 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if t.dark { 80 } else { 40 }),
    };

    let states = [
        (&mut v.widgets.noninteractive, t.control),
        (&mut v.widgets.inactive, t.control),
        (&mut v.widgets.hovered, t.control_hover),
        (&mut v.widgets.active, t.control_active),
        (&mut v.widgets.open, t.control_hover),
    ];
    for (w, fill) in states {
        w.corner_radius = t.radius(t.radius_s);
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, t.outline);
        w.fg_stroke = Stroke::new(1.0, t.ink);
    }
    // Panel backgrounds and separators shouldn't get an outline of their own.
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.outline);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.ink_muted);
    v.widgets.active.bg_stroke = Stroke::new(1.0, t.outline_strong);
    v.widgets.active.fg_stroke = Stroke::new(1.0, t.ink);

    v.selection.bg_fill = alpha(t.accent, 70);
    v.selection.stroke = Stroke::new(1.0, t.accent);
    v.hyperlink_color = t.accent;
    v.warn_fg_color = t.warn;
    v.error_fg_color = t.error;

    v
}

/// Preview-like control metrics: roomy buttons on a tight grid.
pub fn apply_spacing(ctx: &egui::Context, t: &Tokens) {
    ctx.all_styles_mut(|style| {
        style.spacing.button_padding = Vec2::new(8.0, 5.0);
        style.spacing.item_spacing = Vec2::new(t.space_s, t.space_s);
        style.spacing.interact_size = Vec2::new(28.0, 28.0);
        style.spacing.menu_margin = egui::Margin::same(6);
    });
}
