//! Markup: the annotation layer, read as JSON and written back the same way,
//! and drawn as an SVG overlay for the viewer.
//!
//! Two people can have this document open -- the phone and, later, an agent
//! acting on the phone's behalf -- so writes are conditional. A client says
//! which version it edited (`If-Match`) and a write against a version that has
//! moved on is refused with the current one, rather than quietly throwing
//! somebody's highlight away.
//!
//! The blob itself is never touched. Markup is a sidecar record, exactly as it
//! is in the desktop app, which is what will let a document annotated on a
//! phone open annotated on a Mac.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::doc::annotation::Annotation;
use crate::doc::page_ops::PageList;
use crate::export::svg::svg_overlay;
use crate::library::SavedMarkup;

use super::Shared;
use super::library_api::{check_id, fail, no_such_document, page_sizes, with_library};

/// The markup format this server writes. The desktop app's sidecars carry the
/// same number.
const VERSION: u32 = 1;

/// A document with no markup yet, so that "there is nothing here" and "there is
/// something here" are the same shape to a client.
pub fn empty_markup(page_count: usize) -> SavedMarkup {
    SavedMarkup {
        version: VERSION,
        annotations: Vec::new(),
        pages: PageList::new(page_count),
    }
}

/// The version tag for a markup layer: the sha256 of exactly the bytes a GET
/// would return.
///
/// Deriving it from the content rather than from a counter means two servers,
/// or a server and a restart, agree about what a version is without keeping a
/// number anywhere.
pub fn etag(markup: &SavedMarkup) -> String {
    let bytes = serde_json::to_vec(markup).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

/// The tag for a document that may have no markup at all.
pub fn etag_of(markup: Option<&SavedMarkup>, page_count: usize) -> String {
    match markup {
        Some(markup) => etag(markup),
        None => etag(&empty_markup(page_count)),
    }
}

/// Does the `If-Match` header the client sent name the version on disk?
///
/// `*` means "whatever is current", per RFC 9110 §13.1.1 -- a client that only
/// wants to know the document still exists. Quotes are optional here because
/// hand-written requests and curl leave them off more often than not.
fn matches(if_match: &str, current: &str) -> bool {
    let strip = |tag: &str| {
        tag.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_owned()
    };
    if_match.trim() == "*" || if_match.split(',').any(|tag| strip(tag) == strip(current))
}

/// The markup and its tag, or the empty one if none has been saved.
async fn load(state: &Shared, id: &str) -> Result<Option<(SavedMarkup, String)>, Response> {
    let wanted = id.to_owned();
    with_library(state, move |lib| {
        let Some(meta) = lib.doc(&wanted).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let markup = lib
            .load_markup(&wanted)
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| empty_markup(meta.page_count));
        let tag = etag(&markup);
        Ok(Some((markup, tag)))
    })
    .await
}

/// `GET /api/docs/{id}/markup` -- the annotation layer, and the tag to quote
/// back when writing it.
pub async fn get_markup(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    match load(&state, &id).await {
        Ok(Some((markup, tag))) => (
            [
                (header::ETAG, tag),
                // Markup changes; the pages it is drawn over do not. The
                // client must ask every time, and gets a tag to ask with.
                (header::CACHE_CONTROL, "no-cache".to_owned()),
            ],
            Json(markup),
        )
            .into_response(),
        Ok(None) => no_such_document(),
        Err(response) => response,
    }
}

/// What a client may send. Everything but the annotations is optional: a
/// viewer that only draws highlights should not have to understand -- or be
/// able to destroy -- the page order.
#[derive(Debug, Deserialize)]
pub struct MarkupBody {
    #[serde(default)]
    pub version: Option<u32>,
    pub annotations: Vec<Annotation>,
    /// Page order and rotation. Absent means "leave what is there".
    #[serde(default)]
    pub pages: Option<PageList>,
}

/// `PUT /api/docs/{id}/markup` -- replace the annotation layer, if it is still
/// the one the client edited.
pub async fn put_markup(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MarkupBody>,
) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let Some(if_match) = headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        // 428 would be the precise code, but 412 is what a conditional-write
        // client already handles, and the sentence says the rest.
        return fail(
            StatusCode::PRECONDITION_FAILED,
            "Saving markup needs an If-Match header naming the version you edited. GET the \
             markup first and quote its ETag.",
        );
    };

    let (current, tag) = match load(&state, &id).await {
        Ok(Some(pair)) => pair,
        Ok(None) => return no_such_document(),
        Err(response) => return response,
    };
    if !matches(&if_match, &tag) {
        // The current layer goes back with the refusal so the client can
        // re-apply its edit without a second round trip.
        return (
            StatusCode::CONFLICT,
            [(header::ETAG, tag.clone())],
            Json(json!({
                "error": "Somebody else changed this markup while you were editing it.",
                "etag": tag,
                "markup": current,
            })),
        )
            .into_response();
    }

    let saved = SavedMarkup {
        version: body.version.unwrap_or(VERSION),
        annotations: body.annotations,
        // A client that did not mention the page order does not get to change
        // it. This is the difference between a phone drawing a highlight and a
        // phone silently un-rotating a page.
        pages: body.pages.unwrap_or(current.pages),
    };
    let new_tag = etag(&saved);
    let wanted = id.clone();
    let written = with_library(&state, move |lib| {
        lib.save_markup(&wanted, &saved).map_err(|e| e.to_string())
    })
    .await;
    if let Err(response) = written {
        return response;
    }
    (
        [(header::ETAG, new_tag.clone())],
        Json(json!({ "ok": true, "etag": new_tag })),
    )
        .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub struct OverlayQuery {
    /// Which page, 1-based. The overlay is per page because it is drawn over
    /// one page image.
    #[serde(default)]
    pub page: Option<usize>,
}

/// `GET /api/docs/{id}/markup.svg?page=2` -- the markup of one page as an SVG
/// the browser can lay straight over the page image.
///
/// The viewBox is the page in PDF points, so the browser does the scaling and
/// the overlay lines up at any zoom without anything being re-fetched.
pub async fn markup_svg(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(query): Query<OverlayQuery>,
) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let page = query.page.unwrap_or(1);
    if page == 0 {
        return fail(StatusCode::BAD_REQUEST, "Pages are numbered from 1.");
    }
    let sizes = match page_sizes(&state, &id).await {
        Ok(sizes) => sizes,
        Err(response) => return response,
    };
    let Some(size) = sizes.get(page - 1) else {
        return fail(
            StatusCode::NOT_FOUND,
            "That document does not have that many pages.",
        );
    };

    let (markup, tag) = match load(&state, &id).await {
        Ok(Some(pair)) => pair,
        Ok(None) => return no_such_document(),
        Err(response) => return response,
    };
    // Annotations carry the index of the page in the ORIGINAL document, which
    // is what the page images are numbered by too.
    let on_page: Vec<Annotation> = markup
        .annotations
        .iter()
        .filter(|a| a.page == page - 1)
        .cloned()
        .collect();

    (
        [
            (header::CONTENT_TYPE, "image/svg+xml".to_owned()),
            (header::ETAG, tag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        svg_overlay(&on_page, size.width, size.height),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::annotation::{AnnotationKind, Style};
    use crate::doc::geometry::{PdfPoint, PdfRect};

    fn highlight(page: usize) -> Annotation {
        Annotation {
            id: 1,
            page,
            kind: AnnotationKind::Highlight,
            rect: PdfRect::from_points(PdfPoint::new(10.0, 20.0), PdfPoint::new(30.0, 40.0)),
            style: Style::default(),
        }
    }

    /// The tag is a fact about the content, so the same markup has the same
    /// tag in another process, and one changed number is a different tag.
    #[test]
    fn the_version_tag_follows_the_content_and_nothing_else() {
        let empty = empty_markup(3);
        let tag = etag(&empty);
        assert!(tag.starts_with('"') && tag.ends_with('"'), "{tag}");
        assert_eq!(tag.len(), 66, "a quoted sha256: {tag}");
        assert_eq!(tag, etag(&empty_markup(3)), "the same markup, the same tag");
        assert_ne!(
            tag,
            etag(&empty_markup(4)),
            "a different document is a different tag"
        );
        assert_eq!(etag_of(None, 3), tag, "no markup is the empty markup");

        let mut drawn = empty_markup(3);
        drawn.annotations.push(highlight(0));
        assert_ne!(etag(&drawn), tag);
    }

    #[test]
    fn if_match_accepts_the_current_tag_quoted_or_not_and_nothing_else() {
        let current = "\"abc123\"";
        assert!(matches(current, current));
        assert!(matches("abc123", current), "curl leaves the quotes off");
        assert!(matches("*", current), "any version at all");
        assert!(matches("W/\"abc123\"", current));
        assert!(matches("\"other\", \"abc123\"", current), "a list of tags");

        assert!(!matches("\"other\"", current));
        assert!(!matches("", current));
        assert!(!matches("abc12", current));
    }

    /// The overlay is what the viewer draws, so it has to be the page's own
    /// annotations in the page's own box.
    #[test]
    fn an_overlay_holds_only_the_page_it_was_asked_for() {
        let annotations = [highlight(0), highlight(1)];
        let first: Vec<Annotation> = annotations
            .iter()
            .filter(|a| a.page == 0)
            .cloned()
            .collect();
        let svg = svg_overlay(&first, 612.0, 792.0);
        assert!(svg.contains("<rect"), "{svg}");
        assert_eq!(svg.matches("<rect").count(), 1, "one page, one highlight");
        assert!(svg.contains("viewBox=\"0 0 612 792\""), "{svg}");
    }
}
