//! Themes built from the Catppuccin palettes.
//!
//! Colour values come from the `catppuccin` crate (MIT licensed,
//! <https://github.com/catppuccin/rust>); the mapping onto egui's `Visuals` is
//! ours and follows the palette's own styling guide: `base` for panels,
//! `surface0..2` for widget states, `overlay0` for borders, `text` for copy.

use catppuccin::{Color, Flavor};
use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};

fn c(color: &Color) -> Color32 {
    Color32::from_rgb(color.rgb.r, color.rgb.g, color.rgb.b)
}

fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn visuals(flavor: &Flavor, glass: bool) -> egui::Visuals {
    let p = &flavor.colors;
    let mut visuals = if flavor.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let panel_alpha = if glass { 245 } else { 255 };
    visuals.panel_fill = alpha(c(&p.base), panel_alpha);
    visuals.window_fill = alpha(c(&p.base), if glass { 250 } else { 255 });
    // Text fields and other "wells" sink below the panel.
    visuals.extreme_bg_color = if flavor.dark {
        c(&p.crust)
    } else {
        c(&p.mantle)
    };
    visuals.faint_bg_color = c(&p.surface0);
    visuals.code_bg_color = c(&p.mantle);

    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0, c(&p.overlay0));
    visuals.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(70),
    };

    let states = [
        (&mut visuals.widgets.noninteractive, &p.surface0),
        (&mut visuals.widgets.inactive, &p.surface0),
        (&mut visuals.widgets.hovered, &p.surface1),
        (&mut visuals.widgets.active, &p.surface2),
        (&mut visuals.widgets.open, &p.surface1),
    ];
    for (w, fill) in states {
        w.corner_radius = CornerRadius::same(7);
        w.bg_fill = c(fill);
        w.weak_bg_fill = c(fill);
        w.bg_stroke = Stroke::new(1.0, c(&p.overlay0));
        w.fg_stroke = Stroke::new(1.0, c(&p.text));
    }
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, c(&p.subtext0));
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, c(&p.text));

    visuals.selection.bg_fill = alpha(c(&p.blue), 70);
    visuals.selection.stroke = Stroke::new(1.0, c(&p.blue));
    visuals.hyperlink_color = c(&p.blue);
    visuals.warn_fg_color = c(&p.peach);
    visuals.error_fg_color = c(&p.red);

    visuals
}

/// Backdrop behind the pages: the darkest palette step so the white pages
/// read as sheets sitting on the desk.
pub fn canvas_fill(flavor: &Flavor, glass: bool) -> Color32 {
    alpha(c(&flavor.colors.crust), if glass { 235 } else { 255 })
}
