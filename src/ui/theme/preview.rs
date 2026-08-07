//! The default look: macOS Preview's chrome, rebuilt in egui.
//!
//! Solid by default. An earlier version leaned on window translucency for its
//! character, which read as murky rather than layered -- panel, card and page
//! all washed into each other over a busy desktop. The layering now comes from
//! the palette itself (a panel grey, a raised near-white, a sunken well) and
//! translucency is an optional finish on top of it.

use eframe::egui::Color32;

use super::ACCENT;
use super::tokens::{GLASS_CANVAS_ALPHA, Tokens, alpha};

pub fn tokens(dark: bool, glass: bool) -> Tokens {
    let base = if dark { dark_tokens() } else { light_tokens() };
    Tokens {
        bg_canvas: if glass {
            alpha(base.bg_canvas, GLASS_CANVAS_ALPHA)
        } else {
            base.bg_canvas
        },
        glass,
        dark,
        ..base
    }
}

fn light_tokens() -> Tokens {
    Tokens {
        bg_panel: Color32::from_rgb(246, 246, 247),
        bg_raised: Color32::from_rgb(255, 255, 255),
        bg_sunken: Color32::from_rgb(255, 255, 255),
        // Pages are white, so the desk has to be clearly darker or the sheet
        // edges disappear.
        bg_canvas: Color32::from_rgb(178, 178, 182),

        ink: Color32::from_rgb(28, 28, 30),
        ink_muted: Color32::from_rgb(112, 112, 120),

        accent: ACCENT,
        on_accent: Color32::WHITE,
        warn: Color32::from_rgb(0xb7, 0x6e, 0x00),
        error: Color32::from_rgb(0xc0, 0x39, 0x2b),

        outline: Color32::from_rgb(222, 222, 226),
        outline_strong: Color32::from_rgb(196, 196, 202),

        control: Color32::from_rgb(233, 233, 237),
        control_hover: Color32::from_rgb(223, 223, 229),
        control_active: Color32::from_rgb(211, 211, 218),

        radius_s: 5,
        radius_m: 8,
        radius_l: 12,
        space_xs: 3.0,
        space_s: 6.0,
        ribbon_height: 44.0,
        glass: false,
        dark: false,
    }
}

fn dark_tokens() -> Tokens {
    Tokens {
        // Graphite rather than black: pure black makes the white pages glare.
        bg_panel: Color32::from_rgb(32, 32, 35),
        bg_raised: Color32::from_rgb(44, 44, 48),
        bg_sunken: Color32::from_rgb(22, 22, 25),
        bg_canvas: Color32::from_rgb(26, 26, 29),

        ink: Color32::from_rgb(236, 236, 240),
        ink_muted: Color32::from_rgb(150, 150, 158),

        accent: ACCENT,
        on_accent: Color32::WHITE,
        warn: Color32::from_rgb(0xe8, 0xa3, 0x3d),
        error: Color32::from_rgb(0xef, 0x6f, 0x62),

        outline: Color32::from_rgb(58, 58, 63),
        outline_strong: Color32::from_rgb(80, 80, 88),

        control: Color32::from_rgb(58, 58, 64),
        control_hover: Color32::from_rgb(69, 69, 76),
        control_active: Color32::from_rgb(82, 82, 90),

        radius_s: 5,
        radius_m: 8,
        radius_l: 12,
        space_xs: 3.0,
        space_s: 6.0,
        ribbon_height: 44.0,
        glass: false,
        dark: true,
    }
}
