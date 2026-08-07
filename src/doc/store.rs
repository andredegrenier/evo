//! Holds all markup annotations for a document, in z-order per page
//! (last = topmost).

use super::annotation::{Annotation, AnnotationId};

#[derive(Default)]
pub struct AnnotationStore {
    annotations: Vec<Annotation>,
    next_id: AnnotationId,
}

impl AnnotationStore {
    pub fn alloc_id(&mut self) -> AnnotationId {
        self.next_id += 1;
        self.next_id
    }

    pub fn insert(&mut self, ann: Annotation) {
        self.annotations.push(ann);
    }

    pub fn remove(&mut self, id: AnnotationId) -> Option<Annotation> {
        let idx = self.annotations.iter().position(|a| a.id == id)?;
        Some(self.annotations.remove(idx))
    }

    pub fn get(&self, id: AnnotationId) -> Option<&Annotation> {
        self.annotations.iter().find(|a| a.id == id)
    }

    pub fn get_mut(&mut self, id: AnnotationId) -> Option<&mut Annotation> {
        self.annotations.iter_mut().find(|a| a.id == id)
    }

    pub fn replace(&mut self, ann: Annotation) {
        if let Some(slot) = self.get_mut(ann.id) {
            *slot = ann;
        } else {
            self.insert(ann);
        }
    }

    /// Annotations on `page` in z-order, bottom to top.
    pub fn on_page(&self, page: usize) -> impl Iterator<Item = &Annotation> {
        self.annotations.iter().filter(move |a| a.page == page)
    }

    pub fn is_empty(&self) -> bool {
        self.annotations.is_empty()
    }
}
