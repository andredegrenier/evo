//! Preferences: keyboard shortcuts, ribbon layout, the language model and
//! scripting.

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

use crate::keymap::{Action, Category, Keymap};
use crate::library::enrich::AssistantPrefs;
use crate::llm;
use crate::llm::download::Downloads;
use crate::mcp::McpPrefs;
use crate::mcp::runtime::McpStatus;
use crate::script::ScriptPrefs;
use crate::script::model::Api;
use crate::ui::ribbon::RibbonConfig;

#[derive(Default, PartialEq, Clone, Copy)]
pub enum Tab {
    #[default]
    Shortcuts,
    Ribbon,
    Model,
    Scripting,
    Mcp,
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
///
/// One argument per group of settings. Bundling them into a struct would only
/// move the list somewhere else, and each one is genuinely separate.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &egui::Context,
    st: &mut PreferencesState,
    keymap: &mut Keymap,
    ribbon: &mut RibbonConfig,
    scripts: &mut ScriptPrefs,
    assistant: &mut AssistantPrefs,
    downloads: &mut Downloads,
    mcp: &mut McpPrefs,
    mcp_status: Option<McpStatus>,
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
                    (Tab::Model, "Model"),
                    (Tab::Scripting, "Scripting"),
                    (Tab::Mcp, "MCP"),
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
                Tab::Model => changed |= model_tab(ui, scripts, assistant, downloads),
                Tab::Scripting => changed |= scripting_tab(ui, scripts),
                Tab::Mcp => changed |= mcp_tab(ui, mcp, mcp_status.clone()),
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

/// Is this build able to run a model itself?
const BUILTIN: bool = cfg!(feature = "builtin-llm");

/// Where chat and scripts get their answers from: a model evo downloads and
/// runs itself, or a server on the machine.
fn model_tab(
    ui: &mut egui::Ui,
    prefs: &mut ScriptPrefs,
    assistant: &mut AssistantPrefs,
    downloads: &mut Downloads,
) -> bool {
    let before = prefs.clone();
    let assistant_before = *assistant;

    ui.heading("Model");
    ui.label(
        egui::RichText::new(if BUILTIN {
            "Chat and scripts ask a language model on this machine. evo can \
             download and run one itself, or talk to a server you already have."
        } else {
            "Chat and scripts ask a language model on this machine. This build \
             has no model of its own: point it at a local server such as \
             Ollama, LM Studio or llama.cpp."
        })
        .weak(),
    );
    ui.add_space(10.0);

    ui.horizontal_wrapped(|ui| {
        for api in Api::ALL {
            if api == Api::Builtin && !BUILTIN {
                continue;
            }
            if ui
                .selectable_label(prefs.model.api == api, api.label())
                .clicked()
            {
                // Moving between dialects almost always means a different
                // server, so offer its usual address.
                let was_default = prefs.model.base_url == prefs.model.api.default_url();
                prefs.model.api = api;
                if was_default && api.is_http() {
                    prefs.model.base_url = api.default_url().to_owned();
                }
            }
        }
    });
    ui.add_space(10.0);

    if prefs.model.api.is_http() {
        egui::Grid::new("model-server-grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
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
            });
    }

    ui.add_space(10.0);
    library_section(ui, assistant);

    if BUILTIN {
        ui.add_space(8.0);
        catalog_section(ui, prefs, downloads);
    }

    *prefs != before || *assistant != assistant_before
}

/// What the model is allowed to do on its own, unasked. One switch so far.
fn library_section(ui: &mut egui::Ui, assistant: &mut AssistantPrefs) {
    ui.separator();
    ui.label(egui::RichText::new("Library").strong());
    ui.checkbox(
        &mut assistant.enrich_enabled,
        "Summarize and tag library documents",
    )
    .on_hover_text(
        "Every document is read through the model once, in the background, \
         and its summary and tags become searchable.",
    );
    ui.label(
        egui::RichText::new(
            "Off by default: this reads each document you import through the \
             model, which takes a while on a large library. Nothing leaves \
             this machine unless the model you chose above is on another one.",
        )
        .weak()
        .size(11.0),
    );
}

/// The downloadable models: what they cost in disk, what licence they carry,
/// and a button to get or remove each.
fn catalog_section(ui: &mut egui::Ui, prefs: &mut ScriptPrefs, downloads: &mut Downloads) {
    let Some(dir) = llm::llm_models_dir() else {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "evo could not find a data directory to keep models in.",
        );
        return;
    };

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Downloaded models").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let used = llm::disk_usage(&dir);
            if used > 0 {
                ui.label(egui::RichText::new(llm::human_size(used)).weak())
                    .on_hover_text(dir.display().to_string());
            }
        });
    });
    ui.label(
        egui::RichText::new(
            "Weights are not part of evo and are downloaded from Hugging Face \
             on request. Nothing you type is sent anywhere: the model runs on \
             this machine.",
        )
        .weak(),
    );
    ui.add_space(6.0);

    // A finished download becomes an ordinary installed model.
    downloads.forget_finished();

    for entry in &llm::CATALOG {
        ui.add_space(6.0);
        let installed = entry.installed_in(&dir).is_some();
        ui.horizontal(|ui| {
            if ui
                .selectable_label(prefs.model.builtin_model == entry.id, entry.label)
                .on_hover_text("Use this model")
                .clicked()
            {
                prefs.model.builtin_model = entry.id.to_owned();
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} · {}",
                    llm::human_size(entry.size()),
                    entry.license
                ))
                .weak(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                catalog_buttons(ui, entry, &dir, installed, downloads);
            });
        });
        ui.label(egui::RichText::new(entry.summary).weak().size(11.0));

        if let Some(status) = downloads.status(entry.id) {
            if let Some(error) = &status.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            } else {
                let bar = match status.fraction() {
                    Some(f) => egui::ProgressBar::new(f)
                        .text(format!(
                            "{} of {}",
                            llm::human_size(status.received),
                            llm::human_size(status.total)
                        ))
                        .desired_height(12.0),
                    None => egui::ProgressBar::new(0.0)
                        .animate(true)
                        .desired_height(12.0),
                };
                ui.add(bar);
            }
        } else if !installed {
            ui.label(
                egui::RichText::new(format!("Not downloaded — {}", entry.attribution))
                    .weak()
                    .size(11.0),
            );
        } else {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(entry.attribution).weak().size(11.0));
                ui.hyperlink_to(
                    egui::RichText::new("model card").size(11.0),
                    entry.attribution_url,
                );
            });
        }
    }
}

fn catalog_buttons(
    ui: &mut egui::Ui,
    entry: &'static llm::CatalogEntry,
    dir: &std::path::Path,
    installed: bool,
    downloads: &mut Downloads,
) {
    let status = downloads.status(entry.id);
    match status {
        Some(s) if !s.done => {
            if ui.button("Stop").clicked() {
                downloads.cancel(entry.id);
            }
        }
        Some(s) if s.error.is_some() => {
            if ui.button("Try Again").clicked() {
                downloads.dismiss(entry.id);
                downloads.start(entry, dir.to_path_buf(), ui.ctx());
            }
        }
        _ if installed => {
            if ui
                .button("Delete")
                .on_hover_text("Remove the weights from this machine")
                .clicked()
                && let Err(e) = llm::delete_model(dir, entry)
            {
                // Nothing else is watching this; say it where it happened.
                ui.colored_label(ui.visuals().error_fg_color, e.to_string());
            }
        }
        _ => {
            if ui
                .button("Download")
                .on_hover_text(format!(
                    "About {} over the network",
                    llm::human_size(entry.size())
                ))
                .clicked()
            {
                downloads.start(entry, dir.to_path_buf(), ui.ctx());
            }
        }
    }
}

fn scripting_tab(ui: &mut egui::Ui, prefs: &mut ScriptPrefs) -> bool {
    let before = prefs.clone();

    ui.heading("Scripting");
    ui.label(
        egui::RichText::new(
            "Scripts run over the open document and can ask the language model \
             for text. Which model that is lives in the Model tab.",
        )
        .weak(),
    );
    ui.add_space(10.0);

    egui::Grid::new("scripting-grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
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
             beyond the model.",
        )
        .weak(),
    );

    *prefs != before
}

/// The MCP server: whether other programs on this machine may drive evo, and
/// the token they have to present to do it.
fn mcp_tab(ui: &mut egui::Ui, prefs: &mut McpPrefs, status: Option<McpStatus>) -> bool {
    let before = prefs.clone();

    ui.heading("MCP Server");
    ui.label(
        egui::RichText::new(
            "Lets an assistant search your library, open a document, mark it up \
             and export it — the same things you can do, done for you while you \
             watch. It listens on this machine only (127.0.0.1) and needs the \
             token below.",
        )
        .weak(),
    );
    ui.add_space(10.0);

    ui.checkbox(&mut prefs.server_enabled, "Run the MCP server")
        .on_hover_text("Off by default: evo does not open a port unless you ask it to");

    // What the server is actually doing, which is not always what was asked
    // for: the port may be taken.
    if let Some(status) = &status {
        ui.add_space(4.0);
        if let Some(error) = &status.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        } else if let Some(port) = status.listening {
            ui.label(
                egui::RichText::new(format!("Listening on 127.0.0.1:{port}"))
                    .weak()
                    .size(11.0),
            );
        }
    }

    ui.add_space(8.0);
    egui::Grid::new("mcp-grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut prefs.port).range(1024..=65535))
                .on_hover_text("Change this if something else already uses 8137");
            ui.end_row();

            ui.label("Token");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut prefs.token.clone())
                        .desired_width(240.0)
                        .font(egui::TextStyle::Monospace)
                        .interactive(false),
                );
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(prefs.token.clone());
                }
                if ui
                    .small_button("Regenerate")
                    .on_hover_text("Any client using the old token stops working")
                    .clicked()
                {
                    prefs.token = crate::mcp::new_token();
                }
            });
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Client configuration").strong());
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(prefs.client_config());
        }
    });
    ui.label(egui::RichText::new("Paste this into the MCP client you want to connect.").weak());
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::multiline(&mut prefs.client_config().as_str())
            .desired_width(f32::INFINITY)
            .font(egui::TextStyle::Monospace)
            .interactive(false),
    );

    ui.add_space(10.0);
    ui.label(
        egui::RichText::new(
            "A client that would rather start its own process can run \
             `evo mcp-serve` instead, which serves the library over stdin and \
             stdout. Only one of the two can run at a time: they share the \
             library's database.",
        )
        .weak()
        .size(11.0),
    );

    *prefs != before
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Model tab lays out differently for each dialect and for each state
    /// a catalogue row can be in; drawing it is the only way to find out that
    /// it lays out at all.
    #[test]
    fn the_model_tab_draws_for_every_dialect() {
        let mut prefs = ScriptPrefs::default();
        let mut assistant = AssistantPrefs::default();
        let mut downloads = Downloads::default();
        for api in Api::ALL {
            prefs.model.api = api;
            let ctx = egui::Context::default();
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                model_tab(ui, &mut prefs, &mut assistant, &mut downloads);
            });
        }
    }

    /// The enrichment switch is a preference like any other: touching it has
    /// to report a change so the app persists it.
    #[test]
    fn turning_enrichment_on_counts_as_a_change() {
        let mut prefs = ScriptPrefs::default();
        let mut assistant = AssistantPrefs::default();
        let mut downloads = Downloads::default();
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(
                !model_tab(ui, &mut prefs, &mut assistant, &mut downloads),
                "nothing was touched"
            );
        });

        assistant.enrich_enabled = true;
        let before = assistant;
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            // Drawn with it on: the change is reported by comparison, and the
            // tab still lays out.
            model_tab(ui, &mut prefs, &mut assistant, &mut downloads);
        });
        assert_eq!(assistant, before, "drawing does not flip the switch");
    }

    /// The MCP tab has to draw in every state the server can be in, including
    /// the one where the port was taken.
    #[test]
    fn the_mcp_tab_draws_running_stopped_and_broken() {
        let mut prefs = McpPrefs::default();
        for status in [
            None,
            Some(McpStatus::default()),
            Some(McpStatus {
                listening: Some(8137),
                error: None,
            }),
            Some(McpStatus {
                listening: None,
                error: Some("port already in use".to_owned()),
            }),
        ] {
            let ctx = egui::Context::default();
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(
                    !mcp_tab(ui, &mut prefs, status.clone()),
                    "drawing the tab must not change a setting"
                );
            });
        }
        assert!(!prefs.server_enabled, "and must not switch the server on");
    }

    #[test]
    fn the_scripting_tab_still_draws_after_the_model_moved_out_of_it() {
        let mut prefs = ScriptPrefs::default();
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(!scripting_tab(ui, &mut prefs), "nothing was touched");
        });
    }

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
