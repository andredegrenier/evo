//! Logical page operations (rotate / delete / reorder). These only mutate the
//! in-memory page list; lopdf applies them to the real PDF at export time.

use super::geometry::ExtraRotation;

/// One entry per page of the ORIGINAL document. `order` below decides display
/// order; deleted pages simply drop out of `order` (kept here so undo can
/// restore them).
#[derive(Clone, Copy, Debug, Default)]
pub struct PageState {
    pub extra_rotation: ExtraRotation,
}

/// The editable page list.
#[derive(Clone, Debug)]
pub struct PageList {
    /// Per-original-page state, indexed by original page index.
    pub states: Vec<PageState>,
    /// Display order: original page indices, deleted pages omitted.
    pub order: Vec<usize>,
}

impl PageList {
    pub fn new(page_count: usize) -> Self {
        Self {
            states: vec![PageState::default(); page_count],
            order: (0..page_count).collect(),
        }
    }

    /// Number of visible (non-deleted) pages.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn rotation_of(&self, original: usize) -> ExtraRotation {
        self.states[original].extra_rotation
    }

    pub fn rotate_cw(&mut self, original: usize) {
        let r = &mut self.states[original].extra_rotation;
        *r = r.rotated_cw();
    }

    pub fn rotate_ccw(&mut self, original: usize) {
        let r = &mut self.states[original].extra_rotation;
        *r = r.rotated_ccw();
    }

    /// Delete the page at display position `pos`.
    pub fn delete_at(&mut self, pos: usize) {
        self.order.remove(pos);
    }

    /// Move the page at display position `from` to display position `to`.
    pub fn reorder(&mut self, from: usize, to: usize) {
        let page = self.order.remove(from);
        let to = to.min(self.order.len());
        self.order.insert(to, page);
    }

    /// Whether anything differs from the pristine document.
    pub fn is_modified(&self, page_count: usize) -> bool {
        self.order.len() != page_count
            || self.order.iter().enumerate().any(|(i, &p)| i != p)
            || self
                .states
                .iter()
                .any(|s| s.extra_rotation != ExtraRotation::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_and_delete() {
        let mut pl = PageList::new(4);
        pl.reorder(0, 2); // 1 2 0 3
        assert_eq!(pl.order, vec![1, 2, 0, 3]);
        pl.delete_at(1); // 1 0 3
        assert_eq!(pl.order, vec![1, 0, 3]);
        assert!(pl.is_modified(4));
    }

    #[test]
    fn pristine_is_unmodified() {
        assert!(!PageList::new(3).is_modified(3));
    }
}
