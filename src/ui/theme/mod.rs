//! Theming: the hand-built Preview look (default) plus a small collection of
//! Catppuccin palettes. The choice is picked from View > Theme and persisted
//! across launches; `glass` tracks whether OS blur-behind is active, which
//! makes panel fills slightly translucent.

use eframe::egui::{self, Color32};
use serde::{Deserialize, Serialize};

pub mod collection;
pub mod preview;
pub mod tokens;

pub use tokens::Tokens;

pub const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x7c, 0xf6);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    PreviewSystem,
    PreviewLight,
    PreviewDark,
    CatppuccinLatte,
    CatppuccinFrappe,
    CatppuccinMacchiato,
    CatppuccinMocha,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 7] = [
        ThemeChoice::PreviewSystem,
        ThemeChoice::PreviewLight,
        ThemeChoice::PreviewDark,
        ThemeChoice::CatppuccinLatte,
        ThemeChoice::CatppuccinFrappe,
        ThemeChoice::CatppuccinMacchiato,
        ThemeChoice::CatppuccinMocha,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::PreviewSystem => "Preview (System)",
            ThemeChoice::PreviewLight => "Preview Light",
            ThemeChoice::PreviewDark => "Preview Dark",
            ThemeChoice::CatppuccinLatte => "Catppuccin Latte",
            ThemeChoice::CatppuccinFrappe => "Catppuccin Frappé",
            ThemeChoice::CatppuccinMacchiato => "Catppuccin Macchiato",
            ThemeChoice::CatppuccinMocha => "Catppuccin Mocha",
        }
    }

    /// The light/dark preference this choice imposes on egui. Only the
    /// Preview default follows the OS.
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::PreviewSystem => egui::ThemePreference::System,
            ThemeChoice::PreviewLight | ThemeChoice::CatppuccinLatte => {
                egui::ThemePreference::Light
            }
            _ => egui::ThemePreference::Dark,
        }
    }

    /// The Catppuccin flavor backing this choice, if any.
    fn flavor(self) -> Option<&'static catppuccin::Flavor> {
        let palette = &catppuccin::PALETTE;
        match self {
            ThemeChoice::CatppuccinLatte => Some(&palette.latte),
            ThemeChoice::CatppuccinFrappe => Some(&palette.frappe),
            ThemeChoice::CatppuccinMacchiato => Some(&palette.macchiato),
            ThemeChoice::CatppuccinMocha => Some(&palette.mocha),
            _ => None,
        }
    }
}

/// The token set for a choice. Chrome we paint by hand reads its colours and
/// metrics from here, so it matches the widgets egui draws from the same set.
pub fn tokens(ctx: &egui::Context, choice: ThemeChoice, glass: bool) -> Tokens {
    match choice.flavor() {
        Some(flavor) => collection::tokens(flavor, glass),
        None => preview::tokens(ctx.theme() == egui::Theme::Dark, glass),
    }
}

/// Install `choice` into the context. Safe to call every frame; the app only
/// does so when the choice, the resolved light/dark theme, or `glass` changed.
pub fn apply(ctx: &egui::Context, choice: ThemeChoice, glass: bool) {
    ctx.set_theme(choice.preference());
    // `set_theme` can change what `ctx.theme()` resolves to, so read the
    // tokens after it.
    let t = tokens(ctx, choice, glass);
    ctx.set_visuals(tokens::visuals(&t));
    tokens::apply_spacing(ctx, &t);
}

/// Fill for the page canvas backdrop: translucent when glass so the OS blur
/// shows in the margins around pages; the pages themselves stay opaque white.
pub fn canvas_fill(ctx: &egui::Context, choice: ThemeChoice, glass: bool) -> Color32 {
    tokens(ctx, choice, glass).bg_canvas
}
