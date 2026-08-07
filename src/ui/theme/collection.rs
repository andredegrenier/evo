//! Themes built from the Catppuccin palettes.
//!
//! Colour values come from the `catppuccin` crate (MIT licensed,
//! <https://github.com/catppuccin/rust>); the mapping onto our tokens is ours
//! and follows the palette's own styling guide: `base` for panels,
//! `surface0..2` for widget states, `overlay0` for borders, `text` for copy.

use catppuccin::{Color, Flavor};
use eframe::egui::Color32;

use super::tokens::{GLASS_CANVAS_ALPHA, Tokens, alpha};

fn c(color: &Color) -> Color32 {
    Color32::from_rgb(color.rgb.r, color.rgb.g, color.rgb.b)
}

pub fn tokens(flavor: &Flavor, glass: bool) -> Tokens {
    let p = &flavor.colors;
    // The desk behind the pages is the darkest palette step, so the white
    // sheets read as sitting on it.
    let canvas = c(&p.crust);
    Tokens {
        bg_panel: c(&p.base),
        bg_raised: c(&p.surface0),
        bg_sunken: if flavor.dark {
            c(&p.crust)
        } else {
            c(&p.mantle)
        },
        bg_canvas: if glass {
            alpha(canvas, GLASS_CANVAS_ALPHA)
        } else {
            canvas
        },

        ink: c(&p.text),
        ink_muted: c(&p.subtext0),

        accent: c(&p.blue),
        on_accent: c(&p.base),
        warn: c(&p.peach),
        error: c(&p.red),

        outline: c(&p.surface1),
        outline_strong: c(&p.overlay0),

        control: c(&p.surface0),
        control_hover: c(&p.surface1),
        control_active: c(&p.surface2),

        radius_s: 5,
        radius_m: 8,
        radius_l: 12,
        space_xs: 3.0,
        space_s: 6.0,
        ribbon_height: 44.0,
        glass,
        dark: flavor.dark,
    }
}
