//! Undo/redo via a command stack. Interactive gestures mutate the model
//! directly for live feedback and then `record` one command on completion;
//! `undo`/`redo` replay commands against the model.

use super::annotation::Annotation;
use super::page_ops::PageList;
use super::store::AnnotationStore;

#[derive(Clone, Debug)]
pub enum Command {
    AddAnnotation(Annotation),
    RemoveAnnotation(Annotation),
    ModifyAnnotation {
        before: Annotation,
        after: Annotation,
    },
    /// Any page operation (rotate/delete/reorder/duplicate); the page list is
    /// small enough to snapshot whole.
    SetPageList {
        before: PageList,
        after: PageList,
    },
    /// Several commands applied as one undo step (e.g. duplicate pages +
    /// clone their annotations).
    Batch(Vec<Command>),
}

impl Command {
    fn apply(&self, store: &mut AnnotationStore, pages: &mut PageList) {
        match self {
            Command::AddAnnotation(a) => store.replace(a.clone()),
            Command::RemoveAnnotation(a) => {
                store.remove(a.id);
            }
            Command::ModifyAnnotation { after, .. } => store.replace(after.clone()),
            Command::SetPageList { after, .. } => *pages = after.clone(),
            Command::Batch(cmds) => {
                for cmd in cmds {
                    cmd.apply(store, pages);
                }
            }
        }
    }

    fn revert(&self, store: &mut AnnotationStore, pages: &mut PageList) {
        match self {
            Command::AddAnnotation(a) => {
                store.remove(a.id);
            }
            Command::RemoveAnnotation(a) => store.replace(a.clone()),
            Command::ModifyAnnotation { before, .. } => store.replace(before.clone()),
            Command::SetPageList { before, .. } => *pages = before.clone(),
            Command::Batch(cmds) => {
                for cmd in cmds.iter().rev() {
                    cmd.revert(store, pages);
                }
            }
        }
    }
}

#[derive(Default)]
pub struct History {
    undo: Vec<Command>,
    redo: Vec<Command>,
}

impl History {
    /// Record a command whose effect has already been applied to the model.
    pub fn record(&mut self, cmd: Command) {
        self.undo.push(cmd);
        self.redo.clear();
    }

    /// Apply a command to the model and record it.
    pub fn apply(&mut self, cmd: Command, store: &mut AnnotationStore, pages: &mut PageList) {
        cmd.apply(store, pages);
        self.record(cmd);
    }

    pub fn undo(&mut self, store: &mut AnnotationStore, pages: &mut PageList) -> bool {
        if let Some(cmd) = self.undo.pop() {
            cmd.revert(store, pages);
            self.redo.push(cmd);
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self, store: &mut AnnotationStore, pages: &mut PageList) -> bool {
        if let Some(cmd) = self.redo.pop() {
            cmd.apply(store, pages);
            self.undo.push(cmd);
            true
        } else {
            false
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::annotation::{AnnotationKind, Style};
    use crate::doc::geometry::{PdfPoint, PdfRect};

    fn ann(id: u64) -> Annotation {
        Annotation {
            id,
            page: 0,
            kind: AnnotationKind::Rect,
            rect: PdfRect::from_points(PdfPoint::new(0.0, 0.0), PdfPoint::new(10.0, 10.0)),
            style: Style::default(),
        }
    }

    #[test]
    fn undo_redo_round_trip() {
        let mut store = AnnotationStore::default();
        let mut pages = PageList::new(1);
        let mut history = History::default();

        history.apply(Command::AddAnnotation(ann(1)), &mut store, &mut pages);
        assert!(store.get(1).is_some());

        history.undo(&mut store, &mut pages);
        assert!(store.get(1).is_none());

        history.redo(&mut store, &mut pages);
        assert!(store.get(1).is_some());
    }
}
