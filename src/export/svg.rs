//! SVG export: hayro-svg converts each page's original content, then evo's
//! markup is injected as an SVG group in the same coordinate space.

use std::fmt::Write as _;
use std::path::Path;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro_svg::{RenderCache, SvgRenderSettings, convert};

use crate::doc::Document;
use crate::doc::annotation::{Annotation, AnnotationKind, Color, TextAlign};
use crate::doc::geometry::{ExtraRotation, PdfPoint};
use crate::doc::page_ops::PageList;
use crate::doc::store::AnnotationStore;
use crate::export::pdf::wrap_text;

#[derive(Debug, thiserror::Error)]
pub enum SvgError {
    #[error("could not re-parse the PDF")]
    Parse,
    #[error("could not write file: {0}")]
    Io(#[from] std::io::Error),
}

/// Export all visible pages. A single page goes to `base.svg`; multiple pages
/// go to `base-1.svg`, `base-2.svg`, ...
pub fn export_svg(
    doc: &Document,
    pages: &PageList,
    store: &AnnotationStore,
    path: &Path,
) -> Result<Vec<std::path::PathBuf>, SvgError> {
    let pdf = Pdf::new(doc.source.clone()).map_err(|_| SvgError::Parse)?;
    let cache = RenderCache::new();
    let settings = InterpreterSettings::default();
    let svg_settings = SvgRenderSettings {
        bg_color: [255, 255, 255, 255],
    };

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".to_owned());
    let dir = path.parent().unwrap_or(Path::new("."));
    let single = pages.len() == 1;

    let mut written = Vec::new();
    for (position, &logical) in pages.order.iter().enumerate() {
        let source = pages.source_of(logical);
        let hayro_pages = pdf.pages();
        let page = hayro_pages.get(source).ok_or(SvgError::Parse)?;
        let base_svg = convert(page, &cache, &settings, &svg_settings);

        let info = &doc.pages[source];
        let markup = markup_group(store, logical, info.height);
        let rotation = pages.rotation_of(logical);
        let full = compose(&base_svg, &markup, info.width, info.height, rotation);

        let out_path = if single {
            dir.join(format!("{stem}.svg"))
        } else {
            dir.join(format!("{stem}-{}.svg", position + 1))
        };
        std::fs::write(&out_path, full)?;
        written.push(out_path);
    }
    Ok(written)
}

/// Inject markup (and apply user rotation) into a hayro-svg page.
fn compose(base: &str, markup: &str, page_w: f32, page_h: f32, rotation: ExtraRotation) -> String {
    let Some(root_end) = base.find('>') else {
        return base.to_owned();
    };
    let Some(close) = base.rfind("</svg>") else {
        return base.to_owned();
    };
    let inner = &base[root_end + 1..close];

    let (out_w, out_h) = if rotation.swaps_axes() {
        (page_h, page_w)
    } else {
        (page_w, page_h)
    };
    let transform = match rotation {
        ExtraRotation::None => String::new(),
        ExtraRotation::Cw90 => format!("translate({page_h} 0) rotate(90)"),
        ExtraRotation::Cw180 => format!("translate({page_w} {page_h}) rotate(180)"),
        ExtraRotation::Cw270 => format!("translate(0 {page_w}) rotate(-90)"),
    };

    let mut out = format!(
        "<svg viewBox=\"0 0 {out_w} {out_h}\" width=\"{out_w}\" height=\"{out_h}\" \
         xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         style=\"background-color: rgba(255, 255, 255, 255);\">"
    );
    if transform.is_empty() {
        out.push_str(inner);
        out.push_str(markup);
    } else {
        let _ = write!(out, "<g transform=\"{transform}\">");
        out.push_str(inner);
        out.push_str(markup);
        out.push_str("</g>");
    }
    out.push_str("</svg>");
    out
}

fn css(c: Color) -> String {
    format!("rgb({},{},{})", c.r, c.g, c.b)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Our markup as an SVG group. SVG y is top-down: `svg_y = page_h - pdf_y`.
pub(crate) fn markup_group(store: &AnnotationStore, page: usize, page_h: f32) -> String {
    let mut g = String::from("<g id=\"evo-markup\">");
    for ann in store.on_page(page) {
        let _ = write_annotation(&mut g, ann, page_h);
    }
    g.push_str("</g>");
    g
}

/// The markup of one page as a standalone SVG document, with no page content
/// under it.
///
/// `evo serve` draws a page as a PNG with this laid over the top, so the
/// overlay has to describe the same box the raster does: the viewBox is the
/// page in PDF points, and the browser scales it to whatever size the image
/// ended up. `annotations` are the ones on that page, already selected --
/// this function does not know which page it is being asked about, only how
/// tall it is, because that is all the y-flip needs.
pub(crate) fn svg_overlay(annotations: &[Annotation], page_w: f32, page_h: f32) -> String {
    let mut svg = format!(
        "<svg viewBox=\"0 0 {page_w} {page_h}\" xmlns=\"http://www.w3.org/2000/svg\">\
         <g id=\"evo-markup\">"
    );
    for ann in annotations {
        let _ = write_annotation(&mut svg, ann, page_h);
    }
    svg.push_str("</g></svg>");
    svg
}

pub(crate) fn write_annotation(g: &mut String, ann: &Annotation, page_h: f32) -> std::fmt::Result {
    let style = &ann.style;
    let opacity = style.opacity;
    let sy = |y: f32| page_h - y;
    let r = ann.rect;
    let (x, y, w, h) = (r.min.x, sy(r.max.y), r.width(), r.height());

    let stroke_attrs = if style.stroke.is_visible() && style.stroke_width > 0.0 {
        format!(
            " stroke=\"{}\" stroke-width=\"{}\"",
            css(style.stroke),
            style.stroke_width
        )
    } else {
        " stroke=\"none\"".to_owned()
    };
    let fill_attr = if style.fill.is_visible() {
        format!(" fill=\"{}\"", css(style.fill))
    } else {
        " fill=\"none\"".to_owned()
    };

    match &ann.kind {
        AnnotationKind::Highlight => {
            write!(
                g,
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{}\" opacity=\"{opacity}\"/>",
                css(style.fill)
            )?;
        }
        AnnotationKind::Rect => {
            write!(
                g,
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\"{fill_attr}{stroke_attrs} opacity=\"{opacity}\"/>"
            )?;
        }
        AnnotationKind::Ellipse => {
            write!(
                g,
                "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"{fill_attr}{stroke_attrs} opacity=\"{opacity}\"/>",
                x + w / 2.0,
                y + h / 2.0,
                w / 2.0,
                h / 2.0
            )?;
        }
        AnnotationKind::Line { p1, p2, arrow_end } => {
            write!(
                g,
                "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"{stroke_attrs} opacity=\"{opacity}\"/>",
                p1.x,
                sy(p1.y),
                p2.x,
                sy(p2.y)
            )?;
            if *arrow_end {
                let head = arrowhead(*p1, *p2, style.stroke_width, page_h);
                write!(
                    g,
                    "<polygon points=\"{head}\" fill=\"{}\" opacity=\"{opacity}\"/>",
                    css(style.stroke)
                )?;
            }
        }
        AnnotationKind::Freehand { points } => {
            if points.len() >= 2 {
                let mut d = format!("M {} {}", points[0].x, sy(points[0].y));
                for p in &points[1..] {
                    let _ = write!(d, " L {} {}", p.x, sy(p.y));
                }
                write!(
                    g,
                    "<path d=\"{d}\" fill=\"none\"{stroke_attrs} stroke-linejoin=\"round\" stroke-linecap=\"round\" opacity=\"{opacity}\"/>"
                )?;
            }
        }
        AnnotationKind::TextBox {
            text,
            font_size,
            align,
        } => {
            if style.fill.is_visible() {
                write!(
                    g,
                    "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{}\" opacity=\"{opacity}\"/>",
                    css(style.fill)
                )?;
            }
            let pad = 2.0;
            let leading = font_size * 1.25;
            let lines = wrap_text(text, *font_size, (w - 2.0 * pad).max(*font_size));
            let (anchor, tx) = match align {
                TextAlign::Left => ("start", x + pad),
                TextAlign::Center => ("middle", x + w / 2.0),
                TextAlign::Right => ("end", x + w - pad),
            };
            write!(
                g,
                "<text font-family=\"Helvetica, Liberation Sans, Arial, sans-serif\" font-size=\"{font_size}\" fill=\"{}\" opacity=\"{opacity}\" text-anchor=\"{anchor}\">",
                css(style.stroke)
            )?;
            for (i, line) in lines.iter().enumerate() {
                write!(
                    g,
                    "<tspan x=\"{tx}\" y=\"{}\">{}</tspan>",
                    y + font_size + i as f32 * leading,
                    xml_escape(line)
                )?;
            }
            g.push_str("</text>");
        }
    }
    Ok(())
}

fn arrowhead(p1: PdfPoint, p2: PdfPoint, width: f32, page_h: f32) -> String {
    let (dx, dy) = (p2.x - p1.x, p2.y - p1.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return String::new();
    }
    let (ux, uy) = (dx / len, dy / len);
    let size = (width * 4.0).max(8.0).min(len * 0.5);
    let (px, py) = (-uy, ux);
    let a = (
        p2.x - ux * size + px * size * 0.5,
        p2.y - uy * size + py * size * 0.5,
    );
    let b = (
        p2.x - ux * size - px * size * 0.5,
        p2.y - uy * size - py * size * 0.5,
    );
    format!(
        "{},{} {},{} {},{}",
        p2.x,
        page_h - p2.y,
        a.0,
        page_h - a.1,
        b.0,
        page_h - b.1
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::annotation::Style;
    use crate::doc::geometry::PdfRect;

    #[test]
    fn exports_svg_with_markup() {
        let doc = Document::load_path("tests/fixtures/sample.pdf".into()).unwrap();
        let pages = PageList::new(doc.pages.len());
        let mut store = AnnotationStore::default();
        let id = store.alloc_id();
        store.insert(Annotation {
            id,
            page: 0,
            kind: AnnotationKind::Rect,
            rect: PdfRect::from_points(PdfPoint::new(100.0, 100.0), PdfPoint::new(200.0, 150.0)),
            style: Style::default(),
        });

        let dir = std::env::temp_dir().join("evo-svg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = export_svg(&doc, &pages, &store, &dir.join("out.svg")).unwrap();
        assert_eq!(out.len(), 2);
        let first = std::fs::read_to_string(&out[0]).unwrap();
        assert!(first.contains("evo-markup"));
        assert!(first.ends_with("</svg>"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The overlay `evo serve` lays over a page PNG. Annotations are in PDF
    /// points with y up from the bottom; SVG counts down from the top, so a
    /// highlight 100pt above the bottom of a 792pt page is 692 down from its
    /// top -- and the box the browser stretches has to be the page itself.
    #[test]
    fn an_overlay_is_the_page_box_with_the_markup_flipped_into_it() {
        let annotations = vec![Annotation {
            id: 1,
            page: 0,
            kind: AnnotationKind::Highlight,
            rect: PdfRect::from_points(PdfPoint::new(72.0, 100.0), PdfPoint::new(172.0, 120.0)),
            style: Style {
                fill: Color::rgb(255, 235, 59),
                opacity: 0.35,
                ..Style::default()
            },
        }];

        let svg = svg_overlay(&annotations, 612.0, 792.0);
        assert!(svg.starts_with("<svg viewBox=\"0 0 612 792\""), "{svg}");
        assert!(svg.contains("id=\"evo-markup\""), "{svg}");
        assert!(svg.ends_with("</svg>"), "{svg}");
        // y = 792 - 120 (the *top* edge in PDF space), height 20.
        assert!(
            svg.contains("x=\"72\" y=\"672\" width=\"100\" height=\"20\""),
            "{svg}"
        );
        assert!(svg.contains("opacity=\"0.35\""), "{svg}");

        // No markup is still a well-formed overlay: the viewer always asks for
        // one, and an empty group is cheaper to draw than a special case.
        let empty = svg_overlay(&[], 200.0, 400.0);
        assert!(empty.contains("<g id=\"evo-markup\"></g>"), "{empty}");
    }

    /// The whole trip a highlight drawn on a phone makes, in arithmetic.
    ///
    /// `viewer.js` turns a drag in CSS pixels into PDF points -- `pdf_y =
    /// page_h - css_y / scale` -- and the server turns those points back into
    /// an SVG the browser lays over the very picture that was dragged on. The
    /// two flips have to cancel: whatever the reader dragged over is what ends
    /// up under the yellow. This is the test that says so, because the flip is
    /// written twice, in two languages, and only one of them is compiled.
    #[test]
    fn a_drag_in_css_pixels_lands_back_where_it_started() {
        // A US Letter page shown 306 CSS pixels wide: half a pixel per point.
        let (page_w, page_h) = (612.0f32, 792.0f32);
        let scale = 306.0 / page_w;

        // The drag, in CSS pixels from the top-left corner of the picture.
        let (from_x, from_y) = (36.0f32, 100.0f32);
        let (to_x, to_y) = (86.0f32, 110.0f32);

        // What viewer.js sends: the same two corners, in points, counted up
        // from the bottom of the page.
        let corner = |x: f32, y: f32| PdfPoint::new(x / scale, page_h - y / scale);
        let highlight = Annotation {
            id: 1,
            page: 0,
            kind: AnnotationKind::Highlight,
            rect: crate::doc::geometry::PdfRect::from_points(
                corner(from_x, from_y),
                corner(to_x, to_y),
            ),
            style: Style {
                stroke: Color::TRANSPARENT,
                stroke_width: 0.0,
                fill: Color::rgb(250, 220, 50),
                opacity: 0.35,
            },
        };
        // Bottom-left at (72, 572), top-right at (172, 592) in points.
        assert_eq!(highlight.rect.min.x, 72.0);
        assert_eq!(highlight.rect.min.y, 572.0);
        assert_eq!(highlight.rect.max.y, 592.0);

        let svg = svg_overlay(&[highlight], page_w, page_h);
        // y is measured from the top of the page, so the *higher* PDF edge:
        // 792 - 592 = 200 points down, which at half a pixel per point is the
        // 100 CSS pixels the finger started at.
        assert!(
            svg.contains("x=\"72\" y=\"200\" width=\"100\" height=\"20\""),
            "{svg}"
        );
        for (attribute, css) in [("y", from_y), ("height", to_y - from_y)] {
            let points = attribute_of(&svg, attribute);
            assert!(
                (points * scale - css).abs() < 0.01,
                "{attribute} came back as {points}pt, which is {}px and not {css}px",
                points * scale
            );
        }
    }

    /// The value of one attribute of the first element in an overlay.
    fn attribute_of(svg: &str, name: &str) -> f32 {
        let start = svg.find(&format!("{name}=\"")).expect("the attribute") + name.len() + 2;
        let rest = &svg[start..];
        let end = rest.find('"').expect("a closing quote");
        rest[..end].parse().expect("a number")
    }
}
