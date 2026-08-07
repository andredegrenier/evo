//! Turning generated text into a real PDF.
//!
//! Deliberately plain: US Letter, Helvetica, a title and wrapped body text
//! across as many pages as it takes. The point is that a script's output lands
//! in the library as a document you can open, mark up and search like any
//! other, not that it is beautifully typeset.

use lopdf::content::{Content, Operation};
use lopdf::{Document as LoDocument, Object, Stream, dictionary};

use crate::export::pdf::{escape_pdf_string, helvetica_font_dict, wrap_text};

const PAGE_W: f32 = 612.0;
const PAGE_H: f32 = 792.0;
const MARGIN: f32 = 54.0;
const TITLE_SIZE: f32 = 17.0;
const BODY_SIZE: f32 = 11.0;
const LEADING: f32 = 15.5;
const FONT: &str = "EvoHelv";

/// Lay `text` out as a PDF. Blank lines separate paragraphs; everything else
/// is wrapped to the page width.
pub fn text_to_pdf(title: &str, text: &str) -> Result<Vec<u8>, lopdf::Error> {
    let usable = PAGE_W - 2.0 * MARGIN;
    let mut lines: Vec<String> = Vec::new();

    for (i, paragraph) in text.split("\n\n").enumerate() {
        let paragraph = paragraph.trim();
        if i > 0 {
            lines.push(String::new());
        }
        if paragraph.is_empty() {
            continue;
        }
        // Wrap each source line separately so lists and other deliberate
        // breaks survive; a model's output is full of them.
        for source_line in paragraph.lines() {
            let source_line = source_line.trim_end();
            if source_line.is_empty() {
                lines.push(String::new());
            } else {
                lines.extend(wrap_text(source_line, BODY_SIZE, usable));
            }
        }
    }

    let first_page_rows = rows_per_page(PAGE_H - MARGIN - title_block_height(title, usable));
    let later_page_rows = rows_per_page(PAGE_H - 2.0 * MARGIN);

    let mut doc = LoDocument::with_version("1.7");
    let font_id = doc.add_object(helvetica_font_dict());
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { FONT => font_id },
    });

    let mut page_ids = Vec::new();
    let mut remaining: &[String] = &lines;
    let mut first = true;
    loop {
        let capacity = if first {
            first_page_rows
        } else {
            later_page_rows
        };
        let take = remaining.len().min(capacity.max(1));
        let (page_lines, rest) = remaining.split_at(take);
        page_ids.push(add_page(
            &mut doc,
            resources_id,
            if first { Some(title) } else { None },
            page_lines,
            usable,
        )?);
        remaining = rest;
        first = false;
        if remaining.is_empty() {
            break;
        }
    }

    let pages_id = doc.new_object_id();
    for id in &page_ids {
        if let Ok(page) = doc.get_object_mut(*id)
            && let Object::Dictionary(dict) = page
        {
            dict.set("Parent", pages_id);
        }
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => page_ids.len() as i64,
            "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
            "MediaBox" => vec![0.into(), 0.into(), PAGE_W.into(), PAGE_H.into()],
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

fn rows_per_page(height: f32) -> usize {
    ((height - MARGIN) / LEADING).floor().max(1.0) as usize
}

fn title_block_height(title: &str, usable: f32) -> f32 {
    let rows = wrap_text(title, TITLE_SIZE, usable).len().max(1) as f32;
    rows * (TITLE_SIZE * 1.3) + 14.0
}

fn add_page(
    doc: &mut LoDocument,
    resources_id: lopdf::ObjectId,
    title: Option<&str>,
    lines: &[String],
    usable: f32,
) -> Result<lopdf::ObjectId, lopdf::Error> {
    let mut ops = Vec::new();
    let mut y = PAGE_H - MARGIN;

    if let Some(title) = title {
        for line in wrap_text(title, TITLE_SIZE, usable) {
            y -= TITLE_SIZE * 1.3;
            ops.extend(draw_line(&line, MARGIN, y, TITLE_SIZE));
        }
        y -= 14.0;
    }

    for line in lines {
        y -= LEADING;
        if line.is_empty() {
            continue;
        }
        ops.extend(draw_line(line, MARGIN, y, BODY_SIZE));
    }

    let content = Content { operations: ops };
    let stream_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));
    Ok(doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), PAGE_W.into(), PAGE_H.into()],
        "Resources" => resources_id,
        "Contents" => stream_id,
    }))
}

fn draw_line(text: &str, x: f32, y: f32, size: f32) -> Vec<Operation> {
    vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec![FONT.into(), size.into()]),
        Operation::new("Td", vec![x.into(), y.into()]),
        Operation::new(
            "Tj",
            vec![Object::string_literal(escape_pdf_string(text).as_bytes())],
        ),
        Operation::new("ET", vec![]),
    ]
}

/// True when the text contains characters the standard-14 Helvetica encoding
/// can't represent, which are written as "?". Worth telling the user about
/// rather than silently mangling their document.
pub fn has_unrepresentable_chars(text: &str) -> bool {
    text.chars().any(|c| c as u32 >= 256)
}

/// Rough width of a string, for layout sanity checks.
#[cfg(test)]
fn text_width(text: &str, size: f32) -> f32 {
    text.chars()
        .map(|c| crate::export::pdf::char_width(c, size))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Document;

    #[test]
    fn a_generated_pdf_opens_in_the_editor() {
        let bytes = text_to_pdf("Summary", "Hello there.\n\nSecond paragraph.").expect("generate");
        let doc = Document::load_bytes(bytes, None).expect("load");
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].width.round(), 612.0);
        assert_eq!(doc.pages[0].height.round(), 792.0);
    }

    #[test]
    fn the_text_can_be_read_back_out() {
        let bytes =
            text_to_pdf("Quarterly Report", "Revenue grew by twelve percent.").expect("generate");
        let pdf = hayro::hayro_syntax::Pdf::new(std::sync::Arc::new(bytes)).expect("parse");
        let pages = pdf.pages();
        let extracted = crate::library::extract::extract_page_text(
            pages.first().expect("a page"),
            &Default::default(),
        );
        assert!(
            extracted.text.contains("Quarterly Report"),
            "title missing from {:?}",
            extracted.text
        );
        assert!(
            extracted.text.contains("twelve percent"),
            "body missing from {:?}",
            extracted.text
        );
    }

    #[test]
    fn long_text_runs_onto_further_pages() {
        let body = (1..=400)
            .map(|i| format!("Line number {i} of the generated document."))
            .collect::<Vec<_>>()
            .join("\n");
        let bytes = text_to_pdf("Long", &body).expect("generate");
        let doc = Document::load_bytes(bytes, None).expect("load");
        assert!(doc.pages.len() > 1, "expected several pages");
    }

    #[test]
    fn empty_text_still_produces_a_readable_page() {
        let bytes = text_to_pdf("Nothing", "").expect("generate");
        let doc = Document::load_bytes(bytes, None).expect("load");
        assert_eq!(doc.pages.len(), 1);
    }

    #[test]
    fn a_very_long_word_does_not_hang_the_wrapper() {
        let bytes = text_to_pdf("Wide", &"x".repeat(5000)).expect("generate");
        let doc = Document::load_bytes(bytes, None).expect("load");
        assert!(!doc.pages.is_empty());
    }

    #[test]
    fn wrapped_lines_stay_inside_the_margins() {
        let usable = PAGE_W - 2.0 * MARGIN;
        let text = "The quick brown fox jumps over the lazy dog, repeatedly and \
                    at considerable length, until the line must wrap.";
        for line in wrap_text(text, BODY_SIZE, usable) {
            assert!(
                text_width(&line, BODY_SIZE) <= usable + 0.5,
                "line too wide: {line:?}"
            );
        }
    }

    #[test]
    fn characters_outside_the_font_are_flagged() {
        assert!(!has_unrepresentable_chars("plain ascii and café"));
        assert!(has_unrepresentable_chars("emoji 🙂"));
        assert!(has_unrepresentable_chars("日本語"));
    }

    #[test]
    fn parentheses_and_backslashes_do_not_corrupt_the_stream() {
        // These are PDF string delimiters; unescaped they truncate the text.
        let bytes = text_to_pdf("Odd (title)", r"a (b) c \ d").expect("generate");
        let doc = Document::load_bytes(bytes, None).expect("load");
        assert_eq!(doc.pages.len(), 1);
    }
}
