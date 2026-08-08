//! Per-page text extraction using hayro's interpreter: a Device that records
//! glyph unicode values + positions, then reassembles reading order (line
//! bucketing by baseline, left-to-right within a line).
//!
//! Two views of the same work: [`extract_page_text`] for the search indexer
//! (plain text) and [`extract_page_layout`] for find-in-document, which keeps
//! a box per character in canonical display space (PDF points, y-up).

use std::ops::Range;

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt, interpret_page,
};
use hayro::vello_cpu::kurbo::{Affine, BezPath, Point, Rect};

use crate::doc::geometry::{PdfPoint, PdfRect};

/// Fallback advance for glyphs whose font reports no width, in 1000/em units.
const FALLBACK_ADVANCE: f32 = 500.0;
/// Glyph space is 1000 units per em; the em box top is one em above baseline.
const EM_UNITS: f64 = 1000.0;

/// How the text of one page was obtained.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextSource {
    /// From the PDF's own text layer.
    Embedded,
    /// Recovered by OCR from the rendered page.
    Ocr,
}

/// One glyph run (usually a single character) and where it sits on the page.
#[derive(Clone, Debug)]
pub struct CharBox {
    /// Byte range inside the owning [`LineLayout::text`].
    pub byte_range: Range<usize>,
    /// Bounds in canonical display space (PDF points, y-up).
    pub rect: PdfRect,
}

/// One line of text with a box per character.
#[derive(Clone, Debug, Default)]
pub struct LineLayout {
    pub text: String,
    pub chars: Vec<CharBox>,
}

/// Positioned text for one page.
#[derive(Clone, Debug, Default)]
pub struct PageTextLayout {
    pub lines: Vec<LineLayout>,
    /// `None` when the page yielded no text at all.
    pub source: Option<TextSource>,
}

struct GlyphRun {
    text: String,
    /// Baseline origin in device space (y-down).
    x: f32,
    y: f32,
    /// Right edge (baseline origin + advance) in device space.
    x_end: f32,
    /// Em-box extent in device space, `top <= bottom`.
    y_top: f32,
    y_bottom: f32,
}

impl GlyphRun {
    /// Device-space (y-down) box flipped into canonical display space.
    fn rect(&self, page_h: f32) -> PdfRect {
        PdfRect::from_points(
            PdfPoint::new(self.x, page_h - self.y_bottom),
            PdfPoint::new(self.x_end, page_h - self.y_top),
        )
    }
}

/// Thin box bridging the gap between two runs, for an inserted space.
fn gap_rect(prev: &GlyphRun, next: &GlyphRun, page_h: f32) -> PdfRect {
    PdfRect::from_points(
        PdfPoint::new(prev.x_end, page_h - prev.y_bottom),
        PdfPoint::new(next.x, page_h - prev.y_top),
    )
}

#[derive(Default)]
struct TextDevice {
    runs: Vec<GlyphRun>,
    /// Glyphs whose unicode mapping failed (OCR heuristic input).
    unmapped: usize,
}

impl<'a> Device<'a> for TextDevice {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        _paint: &Paint<'a>,
        _draw_mode: &GlyphDrawMode,
    ) {
        let text = match glyph.as_unicode() {
            Some(bf) => match bf {
                BfString::Char(c) => c.to_string(),
                BfString::String(s) => s,
            },
            None => {
                self.unmapped += 1;
                return;
            }
        };
        // `full` maps 1000-units/em glyph space to device space (y-down PDF
        // points), so the glyph box comes out of three probe points.
        let full = transform * glyph_transform;
        let advance = match glyph {
            Glyph::Outline(outline) => outline
                .advance_width()
                .filter(|w| *w > 0.0)
                .unwrap_or(FALLBACK_ADVANCE),
            Glyph::Type3(_) => FALLBACK_ADVANCE,
        };
        let origin = full * Point::ZERO;
        let end = full * Point::new(advance as f64, 0.0);
        let em_top = full * Point::new(0.0, EM_UNITS);
        self.runs.push(GlyphRun {
            text,
            x: origin.x as f32,
            y: origin.y as f32,
            x_end: end.x as f32,
            y_top: (origin.y.min(em_top.y)) as f32,
            y_bottom: (origin.y.max(em_top.y)) as f32,
        });
    }
}

pub struct ExtractedPage {
    pub text: String,
    /// Fraction of glyphs without a unicode mapping (0.0 when no glyphs).
    pub unmapped_ratio: f32,
}

/// Plain text for every page of a whole PDF, in source order.
///
/// A document that will not parse yields no pages rather than an error: the
/// callers (scripts, chat) are asking what the document says, and "nothing"
/// is the honest answer for something we cannot read.
pub fn extract_all_pages(source: &std::sync::Arc<Vec<u8>>) -> Vec<String> {
    let Ok(pdf) = hayro::hayro_syntax::Pdf::new(source.clone()) else {
        return Vec::new();
    };
    pdf.pages()
        .iter()
        .map(|page| extract_page_text(page, &Default::default()).text)
        .collect()
}

/// Extract plain text from one page.
pub fn extract_page_text(page: &Page<'_>, settings: &InterpreterSettings) -> ExtractedPage {
    let (layout, unmapped_ratio) = extract_page_layout(page, settings);
    ExtractedPage {
        text: join_lines(&layout.lines),
        unmapped_ratio,
    }
}

/// Extract positioned text from one page, plus the unmapped-glyph ratio.
pub fn extract_page_layout(
    page: &Page<'_>,
    settings: &InterpreterSettings,
) -> (PageTextLayout, f32) {
    let (w, h) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        page.initial_transform(true).to_kurbo(),
        Rect::new(0.0, 0.0, w as f64, h as f64),
        &cache,
        page.xref(),
        settings.clone(),
    );
    let mut device = TextDevice::default();
    interpret_page(page, &mut ctx, &mut device);

    let total = device.runs.len() + device.unmapped;
    let unmapped_ratio = if total == 0 {
        0.0
    } else {
        device.unmapped as f32 / total as f32
    };
    let lines = assemble_lines(device.runs, h);
    let source = (!lines.is_empty()).then_some(TextSource::Embedded);
    (PageTextLayout { lines, source }, unmapped_ratio)
}

/// Flatten lines back into the plain-text form the search index stores.
pub fn join_lines(lines: &[LineLayout]) -> String {
    let mut out = String::new();
    for line in lines {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line.text);
    }
    out
}

/// Group glyph runs into lines by baseline y, order within a line by x, and
/// insert spaces on gaps, recording a box per emitted character.
fn assemble_lines(mut runs: Vec<GlyphRun>, page_h: f32) -> Vec<LineLayout> {
    if runs.is_empty() {
        return Vec::new();
    }
    // Sort by y (top first: device space is y-down), then x.
    runs.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut lines: Vec<Vec<GlyphRun>> = Vec::new();
    const LINE_TOLERANCE: f32 = 2.5;
    for run in runs {
        match lines.last_mut() {
            Some(line) if (line[0].y - run.y).abs() <= LINE_TOLERANCE => line.push(run),
            _ => lines.push(vec![run]),
        }
    }

    // `out` is the exact plain-text output (see `join_lines`); building it as
    // we go keeps the word-gap rule looking at the same string it always did,
    // and each line's text is just the tail slice added for that line.
    let mut out = String::new();
    let mut result: Vec<LineLayout> = Vec::with_capacity(lines.len());
    for mut line in lines {
        line.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        if !out.is_empty() {
            out.push('\n');
        }
        let line_start = out.len();
        let mut chars: Vec<CharBox> = Vec::with_capacity(line.len());

        // Calibrate the word-gap threshold from this line's own glyph
        // spacing: the median origin-to-origin delta approximates the char
        // advance, and word gaps are noticeably wider.
        let mut deltas: Vec<f32> = line
            .windows(2)
            .map(|w| w[1].x - w[0].x)
            .filter(|d| *d > 0.01)
            .collect();
        deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = deltas.get(deltas.len() / 2).copied().unwrap_or(0.0);

        for (i, run) in line.iter().enumerate() {
            if i > 0 && median > 0.0 {
                let delta = run.x - line[i - 1].x;
                if delta > median * 1.6 && !out.ends_with(' ') {
                    let start = out.len();
                    out.push(' ');
                    chars.push(CharBox {
                        byte_range: start - line_start..out.len() - line_start,
                        rect: gap_rect(&line[i - 1], run, page_h),
                    });
                }
            }
            let start = out.len();
            out.push_str(&run.text);
            if out.len() > start {
                chars.push(CharBox {
                    byte_range: start - line_start..out.len() - line_start,
                    rect: run.rect(page_h),
                });
            }
        }
        result.push(LineLayout {
            text: out[line_start..].to_string(),
            chars,
        });
    }
    result
}

/// Case-insensitive occurrences of `query` in `text`, as byte ranges into
/// `text`.
///
/// Matching happens on a lowercase shadow copy; because case folding can
/// change a character's byte length (`ß` -> `ss`, `İ` -> `i̇`), every shadow
/// byte carries the byte span of the original character that produced it, and
/// the returned ranges are widened to those char boundaries.
pub fn find_in_line(text: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut lower = String::with_capacity(text.len());
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    for (i, ch) in text.char_indices() {
        let end = i + ch.len_utf8();
        for lc in ch.to_lowercase() {
            lower.push(lc);
        }
        spans.resize(lower.len(), (i, end));
    }
    let needle: String = query.chars().flat_map(|c| c.to_lowercase()).collect();
    if needle.is_empty() {
        return Vec::new();
    }

    lower
        .match_indices(&needle)
        .filter_map(|(start, hit)| {
            let end = start + hit.len();
            let (from, _) = *spans.get(start)?;
            let (_, to) = *spans.get(end.checked_sub(1)?)?;
            (from < to).then_some(from..to)
        })
        .collect()
}

/// Union of the boxes of every character overlapping `range`.
pub fn rect_for_range(line: &LineLayout, range: Range<usize>) -> Option<PdfRect> {
    let mut acc: Option<PdfRect> = None;
    for cb in &line.chars {
        if cb.byte_range.start < range.end && range.start < cb.byte_range.end {
            acc = Some(match acc {
                Some(r) => r.union(cb.rect),
                None => cb.rect,
            });
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro::hayro_syntax::Pdf;

    fn fixture_pdf() -> Pdf {
        let bytes = std::fs::read("tests/fixtures/sample.pdf").unwrap();
        Pdf::new(bytes).unwrap()
    }

    #[test]
    fn extracts_fixture_text() {
        let bytes = std::fs::read("tests/fixtures/sample.pdf").unwrap();
        let pdf = Pdf::new(bytes).unwrap();
        let settings = InterpreterSettings::default();
        let pages = pdf.pages();

        let page1 = extract_page_text(&pages[0], &settings);
        assert!(
            page1.text.contains("quick brown fox"),
            "got: {}",
            page1.text
        );
        assert!(page1.unmapped_ratio < 0.1);

        let page2 = extract_page_text(&pages[1], &settings);
        assert!(page2.text.contains("page 2"), "got: {}", page2.text);
    }

    #[test]
    fn layout_matches_plain_text_and_stays_in_bounds() {
        let pdf = fixture_pdf();
        let settings = InterpreterSettings::default();
        let pages = pdf.pages();
        let (w, h) = pages[0].render_dimensions();

        let (layout, _) = extract_page_layout(&pages[0], &settings);
        assert_eq!(layout.source, Some(TextSource::Embedded));
        assert_eq!(
            join_lines(&layout.lines),
            extract_page_text(&pages[0], &settings).text
        );

        for line in &layout.lines {
            let mut prev_end = 0usize;
            for cb in &line.chars {
                // Ranges are monotonic, non-overlapping and inside the line.
                assert!(cb.byte_range.start >= prev_end);
                assert!(cb.byte_range.end <= line.text.len());
                assert!(line.text.is_char_boundary(cb.byte_range.start));
                assert!(line.text.is_char_boundary(cb.byte_range.end));
                prev_end = cb.byte_range.end;

                // Boxes land on the page (small slack for glyph overhang).
                assert!(cb.rect.min.x > -8.0 && cb.rect.max.x < w + 8.0, "{cb:?}");
                assert!(cb.rect.min.y > -8.0 && cb.rect.max.y < h + 8.0, "{cb:?}");
            }
        }
    }

    #[test]
    fn rect_for_range_unions_the_covered_chars() {
        let pdf = fixture_pdf();
        let settings = InterpreterSettings::default();
        let pages = pdf.pages();
        let (layout, _) = extract_page_layout(&pages[0], &settings);

        let (line, range) = layout
            .lines
            .iter()
            .find_map(|l| find_in_line(&l.text, "quick").pop().map(|r| (l, r)))
            .expect("fixture contains 'quick'");
        let rect = rect_for_range(line, range.clone()).unwrap();
        assert!(rect.width() > 0.0 && rect.height() > 0.0);

        // A single character's box is contained in the whole word's box.
        let first = rect_for_range(line, range.start..range.start + 1).unwrap();
        assert!(first.width() <= rect.width() + 1e-3);
        assert!(first.min.x >= rect.min.x - 1e-3);
    }

    #[test]
    fn find_in_line_is_case_insensitive() {
        assert_eq!(find_in_line("Hello World", "world"), vec![6..11]);
        assert_eq!(find_in_line("Hello World", "HELLO"), vec![0..5]);
        assert_eq!(find_in_line("aaa", "aa"), vec![0..2]);
        assert!(find_in_line("Hello", "").is_empty());
        assert!(find_in_line("", "x").is_empty());
    }

    #[test]
    fn find_in_line_handles_multibyte_case_folding() {
        // 'ß' lowercases to itself but 'SS' folds to "ss": the match must be
        // reported on char boundaries of the original string, never panic.
        for (text, query) in [
            ("Straße", "STRASSE"),
            ("STRASSE", "straße"),
            ("Straße", "straße"),
            ("İstanbul", "istanbul"),
            ("ÅNGSTRÖM unit", "ångström"),
            ("naïve café", "CAFÉ"),
        ] {
            for range in find_in_line(text, query) {
                assert!(text.is_char_boundary(range.start), "{text} / {query}");
                assert!(text.is_char_boundary(range.end), "{text} / {query}");
                assert!(range.start < range.end);
                let _ = &text[range];
            }
        }
        assert_eq!(find_in_line("Straße", "straße"), vec![0..7]);
        assert_eq!(find_in_line("naïve café", "CAFÉ"), vec![7..12]);
    }
}
