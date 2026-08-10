//! Logical page operations (rotate / delete / reorder / duplicate). These only
//! mutate the in-memory page list; lopdf applies them to the real PDF at
//! export time.
//!
//! Pages are addressed by **logical index**: initially identical to the source
//! document's page indices, but duplication appends new logical pages that map
//! back to an existing source page via [`PageList::source_of`].

use serde::{Deserialize, Serialize};

use super::geometry::ExtraRotation;

/// Per-logical-page state.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct PageState {
    pub extra_rotation: ExtraRotation,
}

/// The editable page list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageList {
    /// Per-logical-page state, indexed by logical page index.
    pub states: Vec<PageState>,
    /// Display order: logical page indices, deleted pages omitted.
    pub order: Vec<usize>,
    /// Maps logical page index -> source-document page index. Identity for
    /// original pages; duplicates point at the page they were copied from.
    pub source_of: Vec<usize>,
}

impl PageList {
    pub fn new(page_count: usize) -> Self {
        Self {
            states: vec![PageState::default(); page_count],
            order: (0..page_count).collect(),
            source_of: (0..page_count).collect(),
        }
    }

    /// Number of visible (non-deleted) pages.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether every page has been deleted. A document in this state cannot be
    /// exported: there would be nothing in the file.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Source-document page index behind a logical page.
    pub fn source_of(&self, logical: usize) -> usize {
        self.source_of[logical]
    }

    pub fn rotation_of(&self, logical: usize) -> ExtraRotation {
        self.states[logical].extra_rotation
    }

    pub fn rotate_cw(&mut self, logical: usize) {
        let r = &mut self.states[logical].extra_rotation;
        *r = r.rotated_cw();
    }

    pub fn rotate_ccw(&mut self, logical: usize) {
        let r = &mut self.states[logical].extra_rotation;
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

    /// Duplicate the logical page `logical`, inserting the copy at display
    /// position `at_pos`. Returns the new logical index.
    pub fn duplicate(&mut self, logical: usize, at_pos: usize) -> usize {
        let new_logical = self.states.len();
        self.states.push(self.states[logical]);
        self.source_of.push(self.source_of[logical]);
        let at_pos = at_pos.min(self.order.len());
        self.order.insert(at_pos, new_logical);
        new_logical
    }

    /// Append `count` fresh logical pages mapping to source pages starting at
    /// `first_source` (used after Insert Pages merges new sources in).
    pub fn append_source_pages(&mut self, first_source: usize, count: usize) {
        for i in 0..count {
            let logical = self.states.len();
            self.states.push(PageState::default());
            self.source_of.push(first_source + i);
            self.order.push(logical);
        }
    }

    /// Whether anything differs from the pristine document.
    pub fn is_modified(&self, page_count: usize) -> bool {
        self.source_of.len() != page_count
            || self.order.len() != page_count
            || self.order.iter().enumerate().any(|(i, &p)| i != p)
            || self
                .states
                .iter()
                .any(|s| s.extra_rotation != ExtraRotation::None)
    }

    /// A copy of this list showing only the given display positions, in the
    /// given order (used for print-selected / extract-pages subset export).
    pub fn subset(&self, positions: &[usize]) -> Self {
        Self {
            states: self.states.clone(),
            order: positions.iter().map(|&p| self.order[p]).collect(),
            source_of: self.source_of.clone(),
        }
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

    #[test]
    fn duplicate_maps_to_source() {
        let mut pl = PageList::new(2);
        let new_logical = pl.duplicate(1, 1);
        assert_eq!(new_logical, 2);
        assert_eq!(pl.order, vec![0, 2, 1]);
        assert_eq!(pl.source_of(new_logical), 1);
        assert!(pl.is_modified(2));
    }

    #[test]
    fn subset_keeps_selection_order() {
        let pl = PageList::new(4);
        let sub = pl.subset(&[2, 0]);
        assert_eq!(sub.order, vec![2, 0]);
    }
}
