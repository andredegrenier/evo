//! The modal that asks for a PDF's password.
//!
//! Two things can be waiting on the answer: opening a document to read it, and
//! importing one into the library. They ask differently on purpose. Opening is
//! a private act -- the password is used for this session and forgotten when
//! the document closes. Importing is not: the library stores a copy that
//! search, OCR and the phone can read, and no part of that has anywhere to
//! keep a password or anybody to ask. So the import prompt says what it is
//! about to do before it does it.
//!
//! The typed password lives in this struct and nowhere else. It is cleared
//! when the modal closes, whichever way it closes.

use std::path::PathBuf;

use eframe::egui;

/// What is waiting for a password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    /// A file to open in the editor.
    File(PathBuf),
    /// A library document to open. Rare -- the library stores documents
    /// unlocked -- but a blob written by another tool could still want one.
    Library { id: String, page: Option<usize> },
    /// A file to unlock and import into the library.
    Import(PathBuf),
}

impl Pending {
    /// The name to show, so the person knows which file is being asked about
    /// when several were dropped at once.
    fn subject(&self) -> String {
        match self {
            Pending::File(path) | Pending::Import(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "This PDF".to_owned()),
            Pending::Library { .. } => "This document".to_owned(),
        }
    }

    fn is_import(&self) -> bool {
        matches!(self, Pending::Import(_))
    }
}

/// What the app should do now.
pub enum Action {
    /// Try `password` against the pending document.
    Unlock { pending: Pending, password: String },
    /// The person gave up; the open or import does not happen.
    Cancelled,
}

#[derive(Default)]
pub struct PasswordPrompt {
    pending: Option<Pending>,
    /// Never persisted, never logged, cleared on close.
    password: String,
    /// Set after a rejected attempt, so the second ask reads as a retry.
    wrong: bool,
}

impl PasswordPrompt {
    /// Ask for `pending`'s password.
    pub fn ask(&mut self, pending: Pending) {
        self.pending = Some(pending);
        self.password.clear();
        self.wrong = false;
    }

    /// The password just tried was refused: keep the modal up and say so.
    pub fn rejected(&mut self) {
        self.wrong = true;
        self.password.clear();
    }

    pub fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether this prompt is already about `pending`, so a repeated failure
    /// on the same file does not restart the dialog underneath the person.
    pub fn is_asking_about(&self, pending: &Pending) -> bool {
        self.pending.as_ref() == Some(pending)
    }

    pub fn close(&mut self) {
        self.pending = None;
        self.password.clear();
        self.wrong = false;
    }
}

/// Draw the prompt. Returns what to do once the person has decided.
pub fn show(ctx: &egui::Context, st: &mut PasswordPrompt) -> Option<Action> {
    let pending = st.pending.clone()?;

    let mut submitted = false;
    let mut cancel = false;

    let modal = egui::Modal::new(egui::Id::new("pdf-password")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading(if pending.is_import() {
            "Unlock to add to the library"
        } else {
            "Password required"
        });
        ui.add_space(6.0);
        ui.label(format!(
            "{} is password-protected.",
            truncate(&pending.subject(), 48)
        ));
        if pending.is_import() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "evo stores an unlocked copy so search, OCR and phone access work. \
                     The password itself is not saved.",
                )
                .weak(),
            );
        }
        ui.add_space(10.0);

        // The field takes focus on the frame it appears, so the password can
        // be typed without reaching for the mouse first.
        let field = ui.add(
            egui::TextEdit::singleline(&mut st.password)
                .password(true)
                .hint_text("Password")
                .desired_width(f32::INFINITY),
        );
        if !field.has_focus() && !st.wrong {
            field.request_focus();
        }
        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submitted = true;
        }
        // A refused password stays on screen as a retry, never repeating what
        // was typed.
        if st.wrong {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("That password did not open it. Try again.")
                    .color(ui.visuals().error_fg_color),
            );
            field.request_focus();
        }

        ui.add_space(12.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if pending.is_import() {
                    "Unlock and add"
                } else {
                    "Open"
                };
                if ui
                    .add_enabled(!st.password.is_empty(), egui::Button::new(label))
                    .clicked()
                {
                    submitted = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    });

    if cancel || modal.should_close() {
        st.close();
        return Some(Action::Cancelled);
    }
    if submitted && !st.password.is_empty() {
        let password = std::mem::take(&mut st.password);
        st.wrong = false;
        return Some(Action::Unlock { pending, password });
    }
    None
}

/// A long filename must not stretch the modal off the screen.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subject_is_the_file_name() {
        assert_eq!(
            Pending::File(PathBuf::from("/tmp/Boiler manual.pdf")).subject(),
            "Boiler manual.pdf"
        );
        assert_eq!(
            Pending::Import(PathBuf::from("/tmp/x.pdf")).subject(),
            "x.pdf"
        );
        assert_eq!(
            Pending::Library {
                id: "abc".into(),
                page: None
            }
            .subject(),
            "This document"
        );
    }

    /// Only the library import stores anything, so only it explains itself.
    #[test]
    fn only_an_import_is_the_consent_flavour() {
        assert!(Pending::Import(PathBuf::from("a.pdf")).is_import());
        assert!(!Pending::File(PathBuf::from("a.pdf")).is_import());
    }

    /// Closing must not leave a password sitting in memory, and a rejected
    /// attempt must not leave the refused one in the field.
    #[test]
    fn the_typed_password_never_outlives_the_prompt() {
        let mut st = PasswordPrompt::default();
        st.ask(Pending::File(PathBuf::from("a.pdf")));
        assert!(st.is_open());
        st.password.push_str("hunter2");

        st.rejected();
        assert!(st.password.is_empty());
        assert!(st.wrong);

        st.password.push_str("hunter3");
        st.close();
        assert!(st.password.is_empty());
        assert!(!st.wrong);
        assert!(!st.is_open());
    }

    /// A second failure on the same file is a retry, not a fresh dialog.
    #[test]
    fn the_prompt_knows_which_file_it_is_about() {
        let mut st = PasswordPrompt::default();
        let one = Pending::File(PathBuf::from("a.pdf"));
        st.ask(one.clone());
        assert!(st.is_asking_about(&one));
        assert!(!st.is_asking_about(&Pending::File(PathBuf::from("b.pdf"))));
        assert!(!st.is_asking_about(&Pending::Import(PathBuf::from("a.pdf"))));
    }

    #[test]
    fn a_long_name_is_shortened_rather_than_widening_the_modal() {
        assert_eq!(truncate("short.pdf", 48), "short.pdf");
        let long = "x".repeat(80);
        let cut = truncate(&long, 48);
        assert_eq!(cut.chars().count(), 48);
        assert!(cut.ends_with('…'));
    }
}
