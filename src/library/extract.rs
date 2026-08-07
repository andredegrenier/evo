//! Per-page plain-text extraction using hayro's interpreter: a Device that
//! records glyph unicode values + positions, then reassembles reading order
//! (line bucketing by baseline, left-to-right within a line).

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt, interpret_page,
};
use hayro::vello_cpu::kurbo::{Affine, BezPath, Point, Rect};

struct GlyphRun {
    text: String,
    x: f32,
    y: f32,
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
        let full = transform * glyph_transform;
        let origin = full * Point::ZERO;
        self.runs.push(GlyphRun {
            text,
            x: origin.x as f32,
            y: origin.y as f32,
        });
    }
}

pub struct ExtractedPage {
    pub text: String,
    /// Fraction of glyphs without a unicode mapping (0.0 when no glyphs).
    pub unmapped_ratio: f32,
}

/// Extract plain text from one page.
pub fn extract_page_text(page: &Page<'_>, settings: &InterpreterSettings) -> ExtractedPage {
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
    ExtractedPage {
        text: assemble(device.runs),
        unmapped_ratio,
    }
}

/// Group glyph runs into lines by baseline y, order within a line by x, and
/// insert spaces on gaps.
fn assemble(mut runs: Vec<GlyphRun>) -> String {
    if runs.is_empty() {
        return String::new();
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

    let mut out = String::new();
    for mut line in lines {
        line.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        if !out.is_empty() {
            out.push('\n');
        }
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
                    out.push(' ');
                }
            }
            out.push_str(&run.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro::hayro_syntax::Pdf;

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
}
