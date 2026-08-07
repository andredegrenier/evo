//! Combine several PDFs into one at the lopdf level. Used by both
//! "Combine PDFs…" and "Insert Pages from PDF…" — the merged bytes are then
//! reloaded through the normal document-open path, so the rest of the editor
//! never has to know about multiple sources.

use std::collections::BTreeMap;

use lopdf::{Document as LoDocument, Object, ObjectId, dictionary};

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("could not read one of the PDFs: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("could not write file: {0}")]
    Io(#[from] std::io::Error),
    #[error("one of the PDFs contains no pages")]
    Empty,
}

/// Merge the given PDFs in order into a single document. Page order is the
/// concatenation of each source's page order.
pub fn merge_pdfs(sources: &[&[u8]]) -> Result<Vec<u8>, MergeError> {
    let mut max_id = 1u32;
    let mut page_ids: Vec<ObjectId> = Vec::new();
    let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();

    for bytes in sources {
        let mut doc = LoDocument::load_mem(bytes)?;
        push_down_inherited(&mut doc)?;
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;

        let pages = doc.get_pages();
        if pages.is_empty() {
            return Err(MergeError::Empty);
        }
        page_ids.extend(pages.into_values());
        objects.extend(doc.objects);
    }

    let mut merged = LoDocument::with_version("1.7");

    // Copy everything except the structural roots we rebuild (and outlines,
    // whose destinations we don't remap).
    for (id, object) in objects {
        match object.type_name().unwrap_or(b"") {
            b"Catalog" | b"Pages" | b"Outlines" | b"Outline" => {}
            _ => {
                merged.objects.insert(id, object);
            }
        }
    }

    let pages_id: ObjectId = (max_id, 0);
    let catalog_id: ObjectId = (max_id + 1, 0);

    for id in &page_ids {
        if let Some(Object::Dictionary(dict)) = merged.objects.get_mut(id) {
            dict.set("Parent", Object::Reference(pages_id));
        }
    }

    let kids: Vec<Object> = page_ids.iter().map(|&id| Object::Reference(id)).collect();
    merged.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => kids.len() as i64,
            "Kids" => kids,
        }),
    );
    merged.objects.insert(
        catalog_id,
        Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        }),
    );
    merged.trailer.set("Root", Object::Reference(catalog_id));
    merged.max_id = catalog_id.0;
    merged.renumber_objects();

    let mut buf = Vec::new();
    merged.save_to(&mut buf)?;
    Ok(buf)
}

/// Page attributes may be inherited from ancestor Pages nodes; the merged
/// document flattens the tree, so copy them onto each page dict first.
fn push_down_inherited(doc: &mut LoDocument) -> Result<(), MergeError> {
    const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for page_id in pages {
        for key in INHERITABLE {
            let missing = doc.get_dictionary(page_id)?.get(key).is_err();
            if !missing {
                continue;
            }
            // Walk up the Parent chain looking for the attribute.
            let mut current = page_id;
            let mut found: Option<Object> = None;
            for _ in 0..32 {
                let dict = doc.get_dictionary(current)?;
                match dict.get(b"Parent") {
                    Ok(Object::Reference(parent)) => {
                        current = *parent;
                        if let Ok(parent_dict) = doc.get_dictionary(current)
                            && let Ok(value) = parent_dict.get(key)
                        {
                            found = Some(value.clone());
                            break;
                        }
                    }
                    _ => break,
                }
            }
            if let Some(value) = found {
                doc.get_dictionary_mut(page_id)?.set(key, value);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_bytes() -> Vec<u8> {
        std::fs::read("tests/fixtures/sample.pdf").unwrap()
    }

    #[test]
    fn merges_two_documents() {
        let a = fixture_bytes();
        let b = fixture_bytes();
        let merged = merge_pdfs(&[&a, &b]).unwrap();

        let lo = LoDocument::load_mem(&merged).unwrap();
        assert_eq!(lo.get_pages().len(), 4);

        // hayro can open and measure the merged document.
        let doc = crate::doc::Document::load_bytes(merged, None).unwrap();
        assert_eq!(doc.pages.len(), 4);
        assert!((doc.pages[2].width - 612.0).abs() < 0.5);
    }

    #[test]
    fn merged_output_survives_export_pipeline() {
        let a = fixture_bytes();
        let merged = merge_pdfs(&[&a, &a]).unwrap();
        let doc = crate::doc::Document::load_bytes(merged, None).unwrap();
        let pages = crate::doc::page_ops::PageList::new(doc.pages.len());
        let store = crate::doc::store::AnnotationStore::default();
        let out = crate::export::pdf::export_pdf_bytes(
            &doc,
            &pages,
            &store,
            crate::export::pdf::ExportOptions::default(),
        )
        .unwrap();
        assert_eq!(LoDocument::load_mem(&out).unwrap().get_pages().len(), 4);
    }
}
