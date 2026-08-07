//! The Scripts window: pick a script, run it over the open document, watch the
//! log, and open what it produced.

use std::path::PathBuf;

use eframe::egui;

use crate::script::{GeneratedDoc, ScriptEngine};

#[derive(Default)]
pub struct ScriptsState {
    pub open: bool,
    scripts: Vec<PathBuf>,
    selected: Option<PathBuf>,
    /// Set once the folder has been listed, so we do not re-read it every frame.
    listed: bool,
    dir_error: Option<String>,
    /// Documents from the last run, offered for opening.
    pub produced: Vec<GeneratedDoc>,
}

/// What the app shell should do after the window is drawn.
pub enum ScriptsAction {
    Run {
        name: String,
        source: String,
    },
    Cancel,
    /// Open a generated document in the editor.
    Open(GeneratedDoc),
    RevealFolder(PathBuf),
}

impl ScriptsState {
    pub fn open(&mut self) {
        self.open = true;
        self.listed = false;
    }

    pub fn refresh(&mut self) {
        self.listed = false;
    }

    fn reload(&mut self) {
        self.listed = true;
        let Some(dir) = crate::script::scripts_dir() else {
            self.dir_error = Some("Could not work out where to keep scripts.".to_owned());
            return;
        };
        match crate::script::ensure_scripts_dir(&dir) {
            Ok(()) => {
                self.dir_error = None;
                self.scripts = crate::script::list_scripts(&dir);
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|s| !self.scripts.contains(s))
                {
                    self.selected = self.scripts.first().cloned();
                }
            }
            Err(e) => self.dir_error = Some(format!("Could not open {}: {e}", dir.display())),
        }
    }
}

pub fn show(
    ctx: &egui::Context,
    st: &mut ScriptsState,
    engine: Option<&ScriptEngine>,
    doc_title: Option<&str>,
) -> Option<ScriptsAction> {
    if !st.open {
        return None;
    }
    if !st.listed {
        st.reload();
    }

    let mut action = None;
    let mut open = st.open;
    egui::Window::new("Scripts")
        .open(&mut open)
        .resizable(true)
        .default_width(560.0)
        .default_height(480.0)
        .show(ctx, |ui| {
            action = body(ui, st, engine, doc_title);
        });
    st.open = open;
    action
}

fn body(
    ui: &mut egui::Ui,
    st: &mut ScriptsState,
    engine: Option<&ScriptEngine>,
    doc_title: Option<&str>,
) -> Option<ScriptsAction> {
    let mut action = None;
    let running = engine.is_some_and(|e| e.is_running());

    if let Some(error) = &st.dir_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
        return None;
    }

    ui.horizontal(|ui| {
        ui.label("Runs against:");
        match doc_title {
            Some(title) => ui.label(egui::RichText::new(title).strong()),
            None => ui.label(egui::RichText::new("no document open").weak()),
        };
    });
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("script-picker")
            .width(300.0)
            .selected_text(
                st.selected
                    .as_ref()
                    .map(|p| display_name(p))
                    .unwrap_or_else(|| "No scripts".to_owned()),
            )
            .show_ui(ui, |ui| {
                let scripts = st.scripts.clone();
                for path in scripts {
                    let name = display_name(&path);
                    ui.selectable_value(&mut st.selected, Some(path), name);
                }
            });

        if running {
            if ui.button("Cancel").clicked() {
                action = Some(ScriptsAction::Cancel);
            }
        } else {
            let can_run = st.selected.is_some() && engine.is_some();
            if ui.add_enabled(can_run, egui::Button::new("Run")).clicked()
                && let Some(path) = st.selected.clone()
            {
                match std::fs::read_to_string(&path) {
                    Ok(source) => {
                        st.produced.clear();
                        action = Some(ScriptsAction::Run {
                            name: display_name(&path),
                            source,
                        });
                    }
                    Err(e) => st.dir_error = Some(format!("Could not read the script: {e}")),
                }
            }
        }

        if ui
            .button("Reload")
            .on_hover_text("Re-read the folder")
            .clicked()
        {
            st.refresh();
        }
        if let Some(dir) = crate::script::scripts_dir()
            && ui
                .button("Open Folder")
                .on_hover_text(dir.display().to_string())
                .clicked()
        {
            action = Some(ScriptsAction::RevealFolder(dir));
        }
    });

    ui.add_space(8.0);
    ui.separator();

    if !st.produced.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Generated").strong());
        for doc in st.produced.clone() {
            ui.horizontal(|ui| {
                ui.label(&doc.title);
                if ui.small_button("Open").clicked() {
                    action = Some(ScriptsAction::Open(doc.clone()));
                }
                ui.label(egui::RichText::new("added to your library").weak());
            });
        }
        ui.add_space(4.0);
        ui.separator();
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Log").strong());
        if running {
            ui.spinner();
        }
    });

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            let Some(engine) = engine else {
                ui.label(egui::RichText::new("Scripting is unavailable.").weak());
                return;
            };
            engine.with_status(|status| {
                if status.log.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Pick a script and press Run. Scripts live in the folder \
                             above — the shipped examples double as documentation for \
                             what they can do.",
                        )
                        .weak(),
                    );
                }
                for line in &status.log {
                    ui.label(egui::RichText::new(line).monospace());
                }
            });
        });

    action
}

fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_is_shown_by_its_filename() {
        assert_eq!(
            display_name(std::path::Path::new("/scripts/summarize.lua")),
            "summarize.lua"
        );
    }
}
