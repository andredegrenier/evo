//! The Combine / Insert PDFs wizard.
//!
//! One dialog for what used to be two menu items with no UI at all: "Insert
//! Pages from PDF" and "Combine PDFs" both went straight from a file picker to
//! a merge, in whatever order the picker happened to return. Here the user sees
//! the list, reorders it, and chooses where the result goes.

use std::path::{Path, PathBuf};

use eframe::egui::{self, CornerRadius, Sense, Stroke, StrokeKind};

use crate::doc::Document;
use crate::ui::theme::ACCENT;

/// Where the merged document ends up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Destination {
    /// Append to the open document, keeping its markup and undo history.
    AppendToCurrent,
    /// Start a new untitled document.
    NewDocument,
}

/// One PDF queued for merging, validated when it was added.
pub struct Entry {
    pub path: PathBuf,
    pub pages: usize,
    pub size: u64,
    /// Set when the file could not be read or parsed; blocks the merge.
    pub error: Option<String>,
}

impl Entry {
    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    fn detail(&self) -> String {
        match &self.error {
            Some(e) => e.clone(),
            None => format!("{} · {}", plural_pages(self.pages), human_size(self.size)),
        }
    }
}

#[derive(Default)]
pub struct MergeWizardState {
    pub open: bool,
    pub entries: Vec<Entry>,
    dest: Option<Destination>,
}

/// What the app should do when the user confirms.
pub struct Confirm {
    pub files: Vec<PathBuf>,
    pub dest: Destination,
}

impl MergeWizardState {
    /// Open the wizard, defaulting to appending when something is open.
    pub fn open_for(&mut self, has_doc: bool) {
        self.open = true;
        self.entries.clear();
        self.dest = Some(if has_doc {
            Destination::AppendToCurrent
        } else {
            Destination::NewDocument
        });
    }

    pub fn close(&mut self) {
        self.open = false;
        self.entries.clear();
        self.dest = None;
    }

    /// Validate and queue files, skipping ones already listed.
    ///
    /// Reading and parsing up front means a broken or encrypted PDF is caught
    /// while the user can still remove it, rather than failing the whole merge
    /// with an error that doesn't say which file was at fault.
    pub fn add_files(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if self.entries.iter().any(|e| e.path == path) {
                continue;
            }
            self.entries.push(validate(&path));
        }
    }

    pub fn move_entry(&mut self, from: usize, to: usize) {
        if from == to || from >= self.entries.len() || to >= self.entries.len() {
            return;
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
    }

    fn destination(&self, has_doc: bool) -> Destination {
        match self.dest {
            Some(Destination::AppendToCurrent) if has_doc => Destination::AppendToCurrent,
            Some(d) if d != Destination::AppendToCurrent => d,
            // The document was closed under us.
            _ => Destination::NewDocument,
        }
    }

    fn blocked(&self, dest: Destination) -> Option<&'static str> {
        if self.entries.iter().any(|e| e.error.is_some()) {
            return Some("Remove the files that could not be read.");
        }
        match dest {
            Destination::AppendToCurrent if self.entries.is_empty() => {
                Some("Add at least one PDF to insert.")
            }
            Destination::NewDocument if self.entries.len() < 2 => {
                Some("Add at least two PDFs to combine.")
            }
            _ => None,
        }
    }
}

fn validate(path: &Path) -> Entry {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            Document::load_bytes(bytes, Some(path.to_path_buf())).map_err(|e| e.to_string())
        }) {
        Ok(doc) => Entry {
            path: path.to_path_buf(),
            pages: doc.pages.len(),
            size,
            error: None,
        },
        Err(e) => Entry {
            path: path.to_path_buf(),
            pages: 0,
            size,
            error: Some(e),
        },
    }
}

/// Draw the wizard. Returns the merge to perform once the user confirms.
pub fn show(
    ctx: &egui::Context,
    st: &mut MergeWizardState,
    has_doc: bool,
    current_title: Option<&str>,
    current_pages: usize,
) -> Option<Confirm> {
    if !st.open {
        return None;
    }

    let dest = st.destination(has_doc);
    let mut confirmed = None;
    let mut cancel = false;

    let modal = egui::Modal::new(egui::Id::new("merge-wizard")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.heading("Combine / Insert PDFs");
        ui.label(
            egui::RichText::new("Pages are merged in the order listed. Drag to reorder.").weak(),
        );
        ui.add_space(8.0);

        destination_picker(ui, st, has_doc, dest);
        ui.add_space(8.0);

        file_list(ui, st, dest, current_title, current_pages);
        ui.add_space(8.0);

        if ui.button("Add PDFs…").clicked()
            && let Some(files) = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .pick_files()
        {
            st.add_files(files);
        }

        ui.add_space(12.0);
        ui.separator();

        let blocked = st.blocked(dest);
        ui.horizontal(|ui| {
            if let Some(reason) = blocked {
                ui.label(egui::RichText::new(reason).weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match dest {
                    Destination::AppendToCurrent => {
                        format!("Insert {}", plural_files(st.entries.len()))
                    }
                    Destination::NewDocument => {
                        format!("Combine {}", plural_files(st.entries.len()))
                    }
                };
                if ui
                    .add_enabled(blocked.is_none(), egui::Button::new(label))
                    .clicked()
                {
                    confirmed = Some(Confirm {
                        files: st.entries.iter().map(|e| e.path.clone()).collect(),
                        dest,
                    });
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    });

    if cancel || modal.should_close() {
        st.close();
        return None;
    }
    if confirmed.is_some() {
        st.close();
    }
    confirmed
}

fn destination_picker(
    ui: &mut egui::Ui,
    st: &mut MergeWizardState,
    has_doc: bool,
    dest: Destination,
) {
    let mut chosen = dest;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(has_doc, |ui| {
            ui.radio_value(
                &mut chosen,
                Destination::AppendToCurrent,
                "Add to the open document",
            )
            .on_disabled_hover_text("Nothing is open");
        });
        ui.radio_value(
            &mut chosen,
            Destination::NewDocument,
            "Create a new document",
        );
    });
    st.dest = Some(chosen);
}

/// A row's edit, applied after the list is drawn so the borrow ends first.
enum RowAction {
    Remove(usize),
    Move(usize, usize),
}

/// The index of the row being dragged. A newtype because egui matches
/// drag payloads by type, and a bare `usize` would collide with any other
/// payload in the app.
#[derive(Clone, Copy)]
struct DragFile(usize);

fn file_list(
    ui: &mut egui::Ui,
    st: &mut MergeWizardState,
    dest: Destination,
    current_title: Option<&str>,
    current_pages: usize,
) {
    let mut action: Option<RowAction> = None;

    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(6)
        .inner_margin(6.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    // The open document is always first in the merge, which is
                    // what keeps its page indices -- and so its markup and undo
                    // history -- valid. Show it, but don't let it be moved.
                    if dest == Destination::AppendToCurrent {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(current_title.unwrap_or("Untitled")).strong(),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "current document · {}",
                                    plural_pages(current_pages)
                                ))
                                .weak(),
                            );
                        });
                        ui.separator();
                    }

                    if st.entries.is_empty() {
                        ui.label(egui::RichText::new("No PDFs added yet.").weak());
                        return;
                    }

                    for index in 0..st.entries.len() {
                        let id = ui.id().with("merge-row").with(index);
                        let resp = ui
                            .push_id(index, |ui| row(ui, st, index, id, &mut action))
                            .inner;

                        if let Some(from) = resp.dnd_release_payload::<DragFile>()
                            && from.0 != index
                        {
                            action = Some(RowAction::Move(from.0, index));
                        }
                        if resp.dnd_hover_payload::<DragFile>().is_some() {
                            ui.painter().rect_stroke(
                                resp.rect,
                                CornerRadius::same(4),
                                Stroke::new(2.0, ACCENT),
                                StrokeKind::Outside,
                            );
                        }
                    }
                });
        });

    match action {
        Some(RowAction::Remove(index)) => {
            st.entries.remove(index);
        }
        Some(RowAction::Move(from, to)) => st.move_entry(from, to),
        None => {}
    }
}

fn row(
    ui: &mut egui::Ui,
    st: &MergeWizardState,
    index: usize,
    id: egui::Id,
    action: &mut Option<RowAction>,
) -> egui::Response {
    let entry = &st.entries[index];
    let last = index + 1 == st.entries.len();
    let failed = entry.error.is_some();

    let inner = ui.horizontal(|ui| {
        grip(ui, id, index);
        ui.label(format!("{}.", index + 1));
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(entry.name()).strong());
            let detail = egui::RichText::new(entry.detail());
            ui.label(if failed {
                detail.color(ui.visuals().error_fg_color)
            } else {
                detail.weak()
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("✕").on_hover_text("Remove").clicked() {
                *action = Some(RowAction::Remove(index));
            }
            // Buttons as well as dragging: reordering by drag alone is fiddly
            // inside a scroll area.
            if ui
                .add_enabled(!last, egui::Button::new("▼").small())
                .on_hover_text("Move down")
                .clicked()
            {
                *action = Some(RowAction::Move(index, index + 1));
            }
            if ui
                .add_enabled(index > 0, egui::Button::new("▲").small())
                .on_hover_text("Move up")
                .clicked()
            {
                *action = Some(RowAction::Move(index, index - 1));
            }
        });
    });

    // Drop target only. Sensing clicks here would take them from the remove
    // and reorder buttons above: this covers them and is registered after
    // them, and egui gives a tie to whichever came last. Dragging the row is
    // the grip's job. A drop target needs only `contains_pointer`, which hover
    // sense provides.
    ui.interact(inner.response.rect, id.with("drop"), Sense::hover())
}

/// The handle that drags a row, at its leading edge. Registered before the
/// row's buttons and overlapping nothing, so it competes with neither.
fn grip(ui: &mut egui::Ui, id: egui::Id, index: usize) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(10.0, 22.0), Sense::hover());
    let handle = ui.interact(rect, id.with("grip"), Sense::click_and_drag());
    handle.dnd_set_drag_payload(DragFile(index));

    let painter = ui.painter();
    let colour = if handle.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let centre = rect.center();
    for row in -1..=1 {
        for col in [-1.0f32, 1.0] {
            let dot = egui::Rect::from_center_size(
                egui::Pos2::new(centre.x + col * 2.0, centre.y + row as f32 * 5.0),
                egui::Vec2::splat(2.0),
            );
            painter.rect_filled(dot, CornerRadius::same(1), colour);
        }
    }
    handle.on_hover_text("Drag to reorder");
}

fn plural_pages(n: usize) -> String {
    if n == 1 {
        "1 page".to_owned()
    } else {
        format!("{n} pages")
    }
}

fn plural_files(n: usize) -> String {
    if n == 1 {
        "1 PDF".to_owned()
    } else {
        format!("{n} PDFs")
    }
}

fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(paths: &[&str]) -> MergeWizardState {
        MergeWizardState {
            open: true,
            entries: paths
                .iter()
                .map(|p| Entry {
                    path: PathBuf::from(p),
                    pages: 1,
                    size: 0,
                    error: None,
                })
                .collect(),
            dest: Some(Destination::NewDocument),
        }
    }

    /// The tests below this one call `move_entry` and friends directly, which
    /// is exactly why the row's buttons could be dead without anything
    /// noticing. These drive the real widgets.
    mod live {
        use super::*;
        use eframe::egui::{Pos2, Rect, Vec2};

        const W: f32 = 700.0;
        const H: f32 = 400.0;

        struct Harness {
            ctx: egui::Context,
            st: MergeWizardState,
            base: egui::RawInput,
        }

        impl Harness {
            fn new(rows: &[&str]) -> Self {
                Self {
                    ctx: egui::Context::default(),
                    st: state(rows),
                    base: egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(W, H))),
                        focused: true,
                        ..Default::default()
                    },
                }
            }

            fn frame(&mut self, events: Vec<egui::Event>) {
                let st = &mut self.st;
                let _ = self.ctx.run_ui(
                    egui::RawInput {
                        events,
                        ..self.base.clone()
                    },
                    |ui| file_list(ui, st, Destination::NewDocument, None, 0),
                );
            }

            fn click(&mut self, pos: Pos2) {
                self.frame(vec![]);
                self.frame(vec![egui::Event::PointerMoved(pos)]);
                self.frame(vec![button(pos, true)]);
                self.frame(vec![button(pos, false)]);
            }
        }

        fn button(pos: Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }

        /// Sweeps the first row looking for the ✕, rather than hard-coding
        /// where it lands.
        #[test]
        fn clicking_the_remove_button_removes_that_row() {
            let mut removed = false;
            let mut x = W - 4.0;
            while x > W / 2.0 && !removed {
                let mut h = Harness::new(&["a.pdf", "b.pdf", "c.pdf"]);
                h.click(Pos2::new(x, 30.0));
                if h.st.entries.len() == 2 {
                    removed = true;
                    assert_eq!(names(&h.st), ["b.pdf", "c.pdf"], "the wrong row went");
                }
                x -= 3.0;
            }
            assert!(
                removed,
                "no position on the first row removed it; the row's buttons \
                 are not receiving clicks"
            );
        }

        #[test]
        fn the_reorder_buttons_move_a_row() {
            let mut moved = false;
            let mut x = W - 4.0;
            while x > W / 2.0 && !moved {
                let mut h = Harness::new(&["a.pdf", "b.pdf"]);
                h.click(Pos2::new(x, 30.0));
                moved = names(&h.st) == ["b.pdf", "a.pdf"];
                x -= 3.0;
            }
            assert!(
                moved,
                "nothing on the first row moved it down; the reorder buttons \
                 are not receiving clicks"
            );
        }

        #[test]
        fn the_grip_still_starts_a_drag() {
            let mut h = Harness::new(&["a.pdf", "b.pdf"]);
            h.frame(vec![]);
            // The grip is the first thing in the row, at its leading edge.
            let pos = Pos2::new(14.0, 30.0);
            h.frame(vec![egui::Event::PointerMoved(pos)]);
            h.frame(vec![button(pos, true)]);
            h.frame(vec![egui::Event::PointerMoved(pos + Vec2::new(0.0, 40.0))]);
            assert!(
                egui::DragAndDrop::payload::<DragFile>(&h.ctx).is_some(),
                "dragging the grip must still pick the row up"
            );
        }
    }

    fn names(st: &MergeWizardState) -> Vec<String> {
        st.entries.iter().map(|e| e.name()).collect()
    }

    #[test]
    fn moving_an_entry_down_shifts_the_rest_up() {
        let mut st = state(&["a.pdf", "b.pdf", "c.pdf"]);
        st.move_entry(0, 2);
        assert_eq!(names(&st), ["b.pdf", "c.pdf", "a.pdf"]);
    }

    #[test]
    fn moving_an_entry_up_shifts_the_rest_down() {
        let mut st = state(&["a.pdf", "b.pdf", "c.pdf"]);
        st.move_entry(2, 0);
        assert_eq!(names(&st), ["c.pdf", "a.pdf", "b.pdf"]);
    }

    #[test]
    fn moving_out_of_bounds_or_onto_itself_is_a_no_op() {
        let mut st = state(&["a.pdf", "b.pdf"]);
        st.move_entry(1, 1);
        st.move_entry(0, 9);
        st.move_entry(9, 0);
        assert_eq!(names(&st), ["a.pdf", "b.pdf"]);
    }

    #[test]
    fn the_same_file_is_not_queued_twice() {
        let mut st = state(&[]);
        // These paths don't exist, so they queue as errored entries -- which is
        // itself the behavior we want for a file that can't be read.
        st.add_files([PathBuf::from("/nope/a.pdf")]);
        st.add_files([PathBuf::from("/nope/a.pdf")]);
        assert_eq!(st.entries.len(), 1);
        assert!(st.entries[0].error.is_some());
    }

    #[test]
    fn a_bad_file_blocks_the_merge() {
        let mut st = state(&["a.pdf", "b.pdf"]);
        assert!(st.blocked(Destination::NewDocument).is_none());
        st.entries[0].error = Some("nope".to_owned());
        assert!(st.blocked(Destination::NewDocument).is_some());
    }

    #[test]
    fn combining_needs_two_files_but_inserting_needs_one() {
        let st = state(&["a.pdf"]);
        assert!(st.blocked(Destination::NewDocument).is_some());
        assert!(st.blocked(Destination::AppendToCurrent).is_none());
    }

    #[test]
    fn inserting_falls_back_to_a_new_document_when_nothing_is_open() {
        let mut st = state(&["a.pdf"]);
        st.dest = Some(Destination::AppendToCurrent);
        assert_eq!(st.destination(true), Destination::AppendToCurrent);
        assert_eq!(st.destination(false), Destination::NewDocument);
    }

    #[test]
    fn opening_picks_the_destination_that_fits() {
        let mut st = MergeWizardState::default();
        st.open_for(true);
        assert_eq!(st.destination(true), Destination::AppendToCurrent);
        st.open_for(false);
        assert_eq!(st.destination(false), Destination::NewDocument);
    }

    #[test]
    fn sizes_read_sensibly() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
