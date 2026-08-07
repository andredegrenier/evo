//! Preferences: keyboard shortcuts and ribbon layout.

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

use crate::keymap::{Action, Category, Keymap};
use crate::script::ScriptPrefs;
use crate::script::model::Api;
use crate::ui::ribbon::RibbonConfig;

#[derive(Default, PartialEq, Clone, Copy)]
pub enum Tab {
    #[default]
    Shortcuts,
    Ribbon,
    Scripting,
}

#[derive(Default)]
pub struct PreferencesState {
    pub open: bool,
    tab: Tab,
    /// The action whose next key press becomes its new binding.
    pub capturing: Option<Action>,
    /// Set after a capture that landed on a chord already in use.
    conflict: Option<(Action, Vec<Action>)>,
}

impl PreferencesState {
    pub fn open(&mut self) {
        self.open = true;
    }

    /// True while the window is swallowing key presses to record a binding.
    /// The app must not dispatch shortcuts in that state, or recording ⌘S
    /// would also save the document.
    pub fn is_capturing(&self) -> bool {
        self.capturing.is_some()
    }
}

/// Returns true when something changed, so the caller knows to persist.
pub fn show(
    ctx: &egui::Context,
    st: &mut PreferencesState,
    keymap: &mut Keymap,
    ribbon: &mut RibbonConfig,
    scripts: &mut ScriptPrefs,
) -> bool {
    if !st.open {
        st.capturing = None;
        st.conflict = None;
        return false;
    }

    let mut changed = false;
    let mut open = st.open;
    egui::Window::new("Preferences")
        .open(&mut open)
        .resizable(true)
        .default_width(430.0)
        .default_height(520.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Shortcuts, "Shortcuts"),
                    (Tab::Ribbon, "Ribbon"),
                    (Tab::Scripting, "Scripting"),
                ] {
                    if ui.selectable_label(st.tab == tab, label).clicked() {
                        st.tab = tab;
                        st.capturing = None;
                    }
                }
            });
            ui.separator();
            match st.tab {
                Tab::Shortcuts => changed |= shortcuts_tab(ctx, ui, st, keymap),
                Tab::Ribbon => changed |= ribbon_tab(ui, ribbon),
                Tab::Scripting => changed |= scripting_tab(ui, scripts),
            }
        });
    st.open = open;
    if !st.open {
        st.capturing = None;
        st.conflict = None;
    }
    changed
}

fn shortcuts_tab(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    st: &mut PreferencesState,
    keymap: &mut Keymap,
) -> bool {
    let mut changed = false;

    if let Some(action) = st.capturing
        && let Some(outcome) = poll_capture(ctx)
    {
        match outcome {
            Capture::Cancel => {}
            Capture::Unbind => {
                keymap.set(action, None);
                changed = true;
                st.conflict = None;
            }
            Capture::Chord(shortcut) => {
                let clashes = keymap.conflicts(shortcut, action);
                keymap.set(action, Some(shortcut));
                changed = true;
                // Bind it either way and say so, rather than refusing the
                // press: the user can see the clash and decide.
                st.conflict = (!clashes.is_empty()).then_some((action, clashes));
            }
        }
        st.capturing = None;
    }

    ui.horizontal(|ui| {
        ui.heading("Keyboard Shortcuts");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Reset All").clicked() {
                keymap.reset_all();
                st.capturing = None;
                st.conflict = None;
                changed = true;
            }
        });
    });
    ui.label(
        egui::RichText::new("Click a shortcut to change it. Esc cancels, Backspace removes it.")
            .weak(),
    );

    if let Some((action, clashes)) = st.conflict.clone() {
        let names: Vec<_> = clashes.iter().map(|a| a.label()).collect();
        let unbind = ui
            .horizontal_wrapped(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "{} now shares a shortcut with {}.",
                        action.label(),
                        names.join(", ")
                    ),
                );
                ui.button("Unbind the other").clicked()
            })
            .inner;
        if unbind {
            for other in clashes {
                keymap.set(other, None);
            }
            st.conflict = None;
            changed = true;
        }
    }

    ui.separator();

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for category in Category::ALL {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(category.label()).strong());
                egui::Grid::new(("shortcut-grid", category.label()))
                    .num_columns(3)
                    .spacing([12.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for action in Action::ALL {
                            if action.category() != category {
                                continue;
                            }
                            changed |= shortcut_row(ctx, ui, st, keymap, action);
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(12.0);
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "Fixed keys: hold Space to pan, Esc to cancel or deselect, \
                     Enter and Shift+Enter to step through find matches, and the \
                     arrow keys to nudge a selection.",
                )
                .weak(),
            );
        });

    changed
}

fn shortcut_row(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    st: &mut PreferencesState,
    keymap: &mut Keymap,
    action: Action,
) -> bool {
    let mut changed = false;
    ui.label(action.label());

    let capturing = st.capturing == Some(action);
    let text = if capturing {
        "Press keys…".to_owned()
    } else {
        match keymap.shortcut(action) {
            Some(s) => ctx.format_shortcut(&s),
            None => "—".to_owned(),
        }
    };
    let button = egui::Button::new(text)
        .selected(capturing)
        .min_size(egui::Vec2::new(120.0, 0.0));
    if ui.add(button).clicked() {
        st.capturing = Some(action);
        st.conflict = None;
    }

    // Keep the column width stable whether or not the reset button is there.
    ui.allocate_ui(egui::Vec2::new(70.0, 0.0), |ui| {
        if keymap.is_default(action) {
            ui.label("");
        } else if ui
            .small_button("Reset")
            .on_hover_text(default_hint(ctx, action))
            .clicked()
        {
            keymap.reset(action);
            changed = true;
        }
    });
    changed
}

fn default_hint(ctx: &egui::Context, action: Action) -> String {
    match action.default_shortcut() {
        Some(s) => format!("Back to {}", ctx.format_shortcut(&s)),
        None => "Back to unbound".to_owned(),
    }
}

enum Capture {
    Chord(KeyboardShortcut),
    Unbind,
    Cancel,
}

/// Take the next key press for use as a binding, consuming it so it can't also
/// trigger whatever it is currently bound to.
fn poll_capture(ctx: &egui::Context) -> Option<Capture> {
    ctx.input_mut(|i| {
        let mut captured = None;
        i.events.retain(|event| {
            if captured.is_some() {
                return true;
            }
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            captured = Some(match key {
                Key::Escape => Capture::Cancel,
                Key::Backspace | Key::Delete => Capture::Unbind,
                key => Capture::Chord(KeyboardShortcut::new(normalize(*modifiers), *key)),
            });
            false
        });
        captured
    })
}

/// `command` is the portable "⌘ on macOS, Ctrl elsewhere" flag, and it is what
/// the defaults and `format_shortcut` use. Collapse the platform-specific
/// flags into it so a recorded chord looks like a hand-written one.
fn normalize(m: Modifiers) -> Modifiers {
    Modifiers {
        alt: m.alt,
        shift: m.shift,
        command: m.command || m.ctrl || m.mac_cmd,
        ctrl: false,
        mac_cmd: false,
    }
}

fn ribbon_tab(ui: &mut egui::Ui, cfg: &mut RibbonConfig) -> bool {
    let mut changed = false;

    ui.heading("Ribbon");
    ui.label(
        egui::RichText::new(
            "Choose which groups appear. To rearrange them, turn on customizing \
             and drag groups or individual buttons — right-clicking the ribbon \
             itself does the same.",
        )
        .weak(),
    );
    ui.add_space(8.0);

    for i in 0..cfg.groups.len() {
        let label = cfg.groups[i].group.label();
        let mut visible = cfg.groups[i].visible;
        if ui.checkbox(&mut visible, label).changed() {
            cfg.groups[i].visible = visible;
            changed = true;
        }
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let mut customizing = cfg.customizing;
        if ui
            .checkbox(&mut customizing, "Customizing")
            .on_hover_text("Drag groups and buttons on the ribbon to rearrange them")
            .changed()
        {
            cfg.customizing = customizing;
        }
        if ui.button("Reset Layout").clicked() {
            let customizing = cfg.customizing;
            *cfg = RibbonConfig::default();
            cfg.customizing = customizing;
            changed = true;
        }
    });

    if cfg.customizing {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Buttons are inert while customizing so a drag moves them \
                 instead of operating them.",
            )
            .weak(),
        );
    }

    changed
}

fn scripting_tab(ui: &mut egui::Ui, prefs: &mut ScriptPrefs) -> bool {
    let before = prefs.clone();

    ui.heading("Scripting");
    ui.label(
        egui::RichText::new(
            "Scripts talk to a language model running on your own machine. \
             evo does not ship one: point this at a local server such as \
             Ollama, LM Studio or llama.cpp.",
        )
        .weak(),
    );
    ui.add_space(10.0);

    egui::Grid::new("scripting-grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("API");
            ui.horizontal(|ui| {
                for api in Api::ALL {
                    if ui
                        .selectable_label(prefs.model.api == api, api.label())
                        .clicked()
                    {
                        // Moving between dialects almost always means a
                        // different server, so offer its usual address.
                        let was_default = prefs.model.base_url == prefs.model.api.default_url();
                        prefs.model.api = api;
                        if was_default {
                            prefs.model.base_url = api.default_url().to_owned();
                        }
                    }
                }
            });
            ui.end_row();

            ui.label("Server");
            ui.add(
                egui::TextEdit::singleline(&mut prefs.model.base_url)
                    .hint_text(prefs.model.api.default_url())
                    .desired_width(260.0),
            );
            ui.end_row();

            ui.label("Model");
            ui.add(
                egui::TextEdit::singleline(&mut prefs.model.model)
                    .hint_text("llama3.2")
                    .desired_width(260.0),
            );
            ui.end_row();

            ui.label("Reply timeout");
            ui.add(
                egui::DragValue::new(&mut prefs.model.timeout_secs)
                    .range(5..=3600)
                    .suffix(" s"),
            )
            .on_hover_text("How long to wait for the model before giving up");
            ui.end_row();

            ui.label("Script time limit");
            ui.add(
                egui::DragValue::new(&mut prefs.deadline_secs)
                    .range(5..=3600)
                    .suffix(" s"),
            )
            .on_hover_text("A script that runs longer than this is stopped");
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.separator();
    ui.label(
        egui::RichText::new(
            "Scripts run sandboxed: no filesystem, no processes, and no network \
             beyond the server above.",
        )
        .weak(),
    );

    *prefs != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_modifiers_collapse_into_command() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(normalize(ctrl), Modifiers::COMMAND);

        let mac = Modifiers {
            mac_cmd: true,
            command: true,
            ..Default::default()
        };
        assert_eq!(normalize(mac), Modifiers::COMMAND);
    }

    #[test]
    fn alt_and_shift_are_preserved() {
        let m = Modifiers {
            alt: true,
            shift: true,
            ctrl: true,
            ..Default::default()
        };
        let n = normalize(m);
        assert!(n.alt && n.shift && n.command);
    }

    #[test]
    fn capture_reads_a_chord_and_swallows_the_key() {
        let ctx = egui::Context::default();
        let press = egui::Event::Key {
            key: Key::J,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        };
        let _ = ctx.run_ui(
            egui::RawInput {
                events: vec![press],
                ..Default::default()
            },
            |ui| {
                let got = poll_capture(ui.ctx());
                assert!(
                    matches!(
                        got,
                        Some(Capture::Chord(s))
                            if s == KeyboardShortcut::new(Modifiers::COMMAND, Key::J)
                    ),
                    "expected to capture ⌘J"
                );
                // Swallowed, so it can't also fire whatever ⌘J is bound to.
                assert!(ui.ctx().input(|i| i.events.is_empty()));
            },
        );
    }

    #[test]
    fn escape_cancels_and_backspace_unbinds() {
        for (key, want_cancel) in [(Key::Escape, true), (Key::Backspace, false)] {
            let ctx = egui::Context::default();
            let press = egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            };
            let _ = ctx.run_ui(
                egui::RawInput {
                    events: vec![press],
                    ..Default::default()
                },
                |ui| {
                    let got = poll_capture(ui.ctx());
                    if want_cancel {
                        assert!(matches!(got, Some(Capture::Cancel)));
                    } else {
                        assert!(matches!(got, Some(Capture::Unbind)));
                    }
                },
            );
        }
    }
}
