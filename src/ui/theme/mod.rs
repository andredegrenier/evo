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

#[cfg(test)]
mod tests {
    use super::*;

    /// Distance between two colours, summed over the channels. Crude next to a
    /// contrast ratio, but it answers the only question here: is this ink a
    /// different colour from the surface it lands on?
    fn distance(a: Color32, b: Color32) -> i32 {
        let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).abs();
        d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())
    }

    /// Well below any of the palettes (the closest pair is ~200) and well above
    /// the failure this guards against, where ink and surface are the same
    /// colour and the window paints as one flat rectangle.
    const READABLE: i32 = 90;

    /// A context with the theme installed, as the app installs it.
    fn styled(choice: ThemeChoice, glass: bool) -> egui::Context {
        let ctx = egui::Context::default();
        crate::app::install_fonts(&ctx);
        apply(&ctx, choice, glass);
        ctx
    }

    fn visuals_of(ctx: &egui::Context) -> egui::Visuals {
        ctx.style_of(ctx.theme()).visuals.clone()
    }

    #[test]
    fn every_theme_puts_readable_ink_on_its_panels_and_windows() {
        for choice in ThemeChoice::ALL {
            for glass in [false, true] {
                let ctx = styled(choice, glass);
                let v = visuals_of(&ctx);
                let label = format!("{:?} glass={glass}", choice);

                // Panels are never allowed to disappear either: glass thins
                // them, it does not remove them.
                assert!(
                    v.panel_fill.a() > 200,
                    "{label}: panel fill is see-through ({:?})",
                    v.panel_fill
                );

                let inks = [
                    ("noninteractive", v.widgets.noninteractive.fg_stroke.color),
                    ("inactive", v.widgets.inactive.fg_stroke.color),
                    ("hovered", v.widgets.hovered.fg_stroke.color),
                    ("active", v.widgets.active.fg_stroke.color),
                ];
                for (state, ink) in inks {
                    assert_eq!(ink.a(), 255, "{label}: {state} ink is transparent");
                    for (surface, fill) in [
                        ("panel", v.panel_fill),
                        ("window", v.window_fill),
                        ("extreme", v.extreme_bg_color),
                    ] {
                        assert!(
                            distance(ink, fill) > READABLE,
                            "{label}: {state} ink {ink:?} is invisible on the {surface} fill {fill:?}"
                        );
                    }
                }
                if let Some(over) = v.override_text_color {
                    assert!(
                        distance(over, v.panel_fill) > READABLE,
                        "{label}: the text colour override {over:?} is invisible on the panel"
                    );
                }
            }
        }
    }

    /// The end of the chain the previous test only checks the ingredients of:
    /// lay out the chrome every launch shows -- a menu bar and the empty-library
    /// heading -- and look at what actually got painted. A window that comes up
    /// as one flat rectangle paints no glyphs, or paints them in the colour of
    /// the panel behind them; both fail here.
    #[test]
    fn the_menu_bar_and_the_empty_library_heading_paint_visible_glyphs() {
        for choice in ThemeChoice::ALL {
            for glass in [false, true] {
                let ctx = styled(choice, glass);
                let label = format!("{:?} glass={glass}", choice);
                // Twice: the first pass rasterizes the fonts the second one uses.
                let mut output = None;
                for _ in 0..2 {
                    output = Some(ctx.run_ui(egui::RawInput::default(), |ui| {
                        apply(ui.ctx(), choice, glass);
                        egui::Panel::top("menu").show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.menu_button("File", |_| {});
                                ui.menu_button("View", |_| {});
                            });
                        });
                        egui::CentralPanel::default_margins().show(ui, |ui| {
                            ui.centered_and_justified(|ui| {
                                ui.heading("evo — drop a PDF here");
                            });
                        });
                    }));
                }
                let output = output.expect("the loop ran");
                let mut glyphs = 0;
                let mut inks = Vec::new();
                for shape in &output.shapes {
                    collect_text(&shape.shape, &mut glyphs, &mut inks);
                }
                assert!(
                    glyphs > 0,
                    "{label}: the chrome painted no glyphs at all -- an empty font atlas"
                );
                let panel = visuals_of(&ctx).panel_fill;
                for ink in inks {
                    assert!(ink.a() > 0, "{label}: painted text with no opacity");
                    assert!(
                        distance(ink, panel) > READABLE,
                        "{label}: painted text {ink:?} in the colour of the panel {panel:?}"
                    );
                }
            }
        }
    }

    /// Glyph count and text colours out of a painted shape tree.
    fn collect_text(shape: &egui::Shape, glyphs: &mut usize, inks: &mut Vec<Color32>) {
        match shape {
            egui::Shape::Text(text) => {
                for row in &text.galley.rows {
                    *glyphs += row.glyphs.len();
                }
                for section in &text.galley.job.sections {
                    // A section left `PLACEHOLDER` takes the fallback colour.
                    if section.format.color == Color32::PLACEHOLDER {
                        inks.push(text.fallback_color);
                    } else {
                        inks.push(section.format.color);
                    }
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, glyphs, inks);
                }
            }
            _ => {}
        }
    }
}
