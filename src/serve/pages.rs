//! Page images: one PNG per page per scale, rendered once and then kept.
//!
//! A phone should not wait for a rasterizer twice for the same page, and it
//! never has to: document ids are content hashes, so `pagecache/<id>/2-3.png`
//! is the third-scale rendering of page two of those exact bytes for ever.
//! That is what lets the answer be `Cache-Control: immutable` and what lets the
//! file be written once and read a hundred times.
//!
//! No rasterizer's document is thread-safe, so parsing and rendering happen
//! inside one `spawn_blocking` closure and nothing about them is shared with a
//! task.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::Shared;
use super::library_api::{IMMUTABLE, check_id, document_bytes, fail, page_sizes};
use crate::render::engine::{self, Engine, EnginePref};

/// How big to draw a page. The engine's own spelling, re-exported because
/// every caller here already speaks it.
pub use crate::render::engine::Zoom;

/// How wide a library thumbnail is drawn, in pixels. The same 320 the desktop
/// app uses, so the two libraries look alike.
pub const THUMB_WIDTH: f32 = 320.0;

/// The scales the viewer may ask for: one CSS pixel per point, and the two
/// steps a retina screen or a pinch needs. Three buckets rather than a
/// continuum because every distinct scale is another file on disk.
pub const SCALES: [u32; 3] = [1, 2, 3];

/// `<library>/pagecache/<id>` -- everything rendered from one document.
pub fn cache_dir(root: &FsPath, id: &str) -> PathBuf {
    root.join("pagecache").join(id)
}

/// The rendered page. `page` is 1-based, as it is in the URL, so what is in
/// the cache directory reads the way the request did.
///
/// The engine is part of the name because two rasterizers do not draw the same
/// pixels: without it, changing `engine` in the configuration would go on
/// serving whatever the old one drew, for ever, with `Cache-Control:
/// immutable` promising it was right.
pub fn cache_path(root: &FsPath, id: &str, page: usize, scale: u32, engine: Engine) -> PathBuf {
    cache_dir(root, id).join(format!("{page}-{scale}-{}.png", engine.tag()))
}

/// Render one page of a PDF to PNG bytes.
///
/// Blocking, and deliberately whole: the parse, the render and the encode are
/// one call because a rasterizer's document cannot outlive the thread that
/// made it.
pub fn render_png(
    source: Arc<Vec<u8>>,
    page: usize,
    zoom: Zoom,
    pref: EnginePref,
) -> Result<Vec<u8>, String> {
    let (drawn, _) =
        engine::render_page(source, None, page, zoom, pref).map_err(|e| e.to_string())?;
    let image = image::RgbaImage::from_raw(drawn.width, drawn.height, drawn.rgba)
        .ok_or_else(|| "evo drew that page but could not make a picture of it.".to_owned())?;

    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("evo could not encode that page as a PNG: {e}"))?;
    Ok(png)
}

/// Write a file that is either wholly there or not there at all.
///
/// A reader is another request for the same page, and half a PNG served from
/// the cache would be a permanent wrong answer -- these files are never
/// rewritten.
pub fn write_atomically(path: &FsPath, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let part = path.with_extension("part");
    std::fs::write(&part, bytes).map_err(|e| format!("could not write {}: {e}", part.display()))?;
    std::fs::rename(&part, path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("could not finish writing {}: {e}", path.display())
    })
}

/// A PNG, cacheable for ever.
pub fn png_response(png: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, IMMUTABLE),
        ],
        png,
    )
        .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    /// 1, 2 or 3 device pixels per CSS pixel.
    #[serde(default)]
    pub scale: Option<u32>,
}

/// `GET /api/docs/{id}/page/{n}.png?scale=2`.
///
/// The `.png` is part of the captured segment rather than the route, because
/// the router matches whole segments only. It is required all the same: the
/// URL a service worker caches should say what it is.
pub async fn page_png(
    State(state): State<Shared>,
    Path((id, file)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let Some(number) = file.strip_suffix(".png") else {
        return fail(
            StatusCode::NOT_FOUND,
            "Page images are addressed as `<page>.png`.",
        );
    };
    let Ok(page) = number.parse::<usize>() else {
        return fail(StatusCode::BAD_REQUEST, "That is not a page number.");
    };
    if page == 0 {
        return fail(StatusCode::BAD_REQUEST, "Pages are numbered from 1.");
    }
    let scale = query.scale.unwrap_or(1);
    if !SCALES.contains(&scale) {
        return fail(
            StatusCode::BAD_REQUEST,
            "Pages are rendered at scale 1, 2 or 3.",
        );
    }

    // Answering from the cache costs one read and no rasterizer at all.
    let pref = state.config.engine;
    let cached = cache_path(
        &state.paths.library_root,
        &id,
        page,
        scale,
        engine::resolve(pref),
    );
    if let Ok(png) = tokio::fs::read(&cached).await {
        return png_response(png);
    }

    // The page count is checked before any work is done, so a request for page
    // 900 of a two-page document is a 404 rather than a render that fails.
    let sizes = match page_sizes(&state, &id).await {
        Ok(sizes) => sizes,
        Err(response) => return response,
    };
    if page > sizes.len() {
        return fail(
            StatusCode::NOT_FOUND,
            "That document does not have that many pages.",
        );
    }

    let bytes = match document_bytes(&state, &id).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let drawn = tokio::task::spawn_blocking(move || {
        let png = render_png(bytes, page - 1, Zoom::Factor(scale as f32), pref)?;
        // A cache that could not be written is a slow server, not a broken
        // one: the picture is already drawn and the reader is waiting for it.
        if let Err(e) = write_atomically(&cached, &png) {
            eprintln!("could not cache a page image: {e}");
        }
        Ok::<Vec<u8>, String>(png)
    })
    .await;

    match drawn {
        Ok(Ok(png)) => png_response(png),
        Ok(Err(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(_) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "evo stopped part-way through drawing that page. Try again.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendered_page_is_cached_under_the_document_it_came_from() {
        let root = FsPath::new("/srv/evo/library");
        let id = "a".repeat(64);
        assert_eq!(
            cache_path(root, &id, 2, 3, Engine::Hayro),
            PathBuf::from(format!("/srv/evo/library/pagecache/{id}/2-3-hayro.png"))
        );
        assert_eq!(
            cache_dir(root, &id),
            cache_path(root, &id, 1, 1, Engine::Hayro).parent().unwrap()
        );
    }

    /// Two engines do not draw the same pixels, and these files are served as
    /// `immutable`. They must never share a name.
    #[test]
    fn each_engine_caches_its_own_pixels() {
        let root = FsPath::new("/srv/evo/library");
        let id = "c".repeat(64);
        assert_ne!(
            cache_path(root, &id, 1, 2, Engine::Hayro),
            cache_path(root, &id, 1, 2, Engine::Pdfium)
        );
    }

    /// The renderer is the slow part of the server, so it gets a test that
    /// actually renders: the answer has to be a PNG of the size that was asked
    /// for, and asking for a page that is not there has to say so.
    #[test]
    fn a_page_comes_back_as_a_png_of_the_size_that_was_asked_for() {
        let bytes = Arc::new(std::fs::read("tests/fixtures/sample.pdf").expect("the fixture"));

        let hayro = EnginePref::Hayro;

        let png = render_png(bytes.clone(), 0, Zoom::Factor(1.0), hayro).expect("page one");
        let decoded = image::load_from_memory(&png).expect("a real PNG");
        // The fixture is US Letter: 612x792 points, so 612x792 pixels at 1.
        assert_eq!(decoded.width(), 612);
        assert_eq!(decoded.height(), 792);

        let bigger =
            render_png(bytes.clone(), 1, Zoom::Factor(2.0), hayro).expect("page two, doubled");
        let decoded = image::load_from_memory(&bigger).expect("a real PNG");
        assert_eq!(decoded.width(), 1224);
        assert_eq!(decoded.height(), 1584);

        let thumb =
            render_png(bytes.clone(), 0, Zoom::FitWidth(THUMB_WIDTH), hayro).expect("a thumbnail");
        let decoded = image::load_from_memory(&thumb).expect("a real PNG");
        assert!(
            (decoded.width() as i64 - 320).abs() <= 1,
            "{}",
            decoded.width()
        );

        let missing = render_png(bytes, 9, Zoom::Factor(1.0), hayro).expect_err("no page ten");
        assert!(missing.contains("no page 10"), "{missing}");
    }

    #[test]
    fn a_half_written_page_image_is_never_left_behind() {
        let dir = std::env::temp_dir().join(format!("evo-pagecache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = cache_path(&dir, &"b".repeat(64), 1, 2, Engine::Hayro);

        write_atomically(&path, b"pretend PNG").expect("writing");
        assert_eq!(std::fs::read(&path).expect("reading"), b"pretend PNG");
        assert!(
            !path.with_extension("part").exists(),
            "the temporary file is renamed, not left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
