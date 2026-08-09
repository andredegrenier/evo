//! The library over HTTP: what is in it, what one document is, and how a
//! document gets in or out.
//!
//! Two rules run through every handler here. Document ids are checked before
//! they are used, because an id becomes a path (`pagecache/<id>/…`, `docs/<id>.pdf`)
//! and a sha256 digest has exactly one shape. And the library is behind a
//! `std::sync::Mutex`, so every call into it happens inside `spawn_blocking`
//! with the lock taken and dropped there -- a lock held across an `await` is a
//! deadlock waiting for a busy afternoon.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::doc::Document;
use crate::library::{Library, hex_digest};
use crate::mcp::library_tools;

use super::{Shared, markup_api, pages};

/// One page's size in PDF points, as the viewer needs it: the PNG and the SVG
/// overlay have to be laid out in the same box.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

// ---------------------------------------------------------------------------
// The pieces every handler in the server needs
// ---------------------------------------------------------------------------

/// Is this the id of a document, or is it something else pretending?
///
/// Every id evo issues is the sha256 of the document's bytes in lowercase hex.
/// Anything else -- `..`, an absolute path, a name with a slash in it -- is not
/// an id, and is stopped here rather than somewhere further down where it would
/// be a filename.
pub fn is_doc_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The check every handler that takes an id starts with. `Some` is the refusal
/// to send back, so the whole guard is `if let Some(response) = check_id(&id)`.
pub fn check_id(id: &str) -> Option<Response> {
    (!is_doc_id(id)).then(|| {
        fail(
            StatusCode::BAD_REQUEST,
            "A document id is 64 hexadecimal characters. That is not one.",
        )
    })
}

/// An error as the app reads it: a status and a sentence.
pub fn fail(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// What to say when the id is well-formed but there is no such document.
pub fn no_such_document() -> Response {
    fail(
        StatusCode::NOT_FOUND,
        "evo has no document with that id. It may have been deleted.",
    )
}

/// Run something against the library on a blocking thread.
///
/// The closure gets the library with the lock held; it must not keep it. The
/// `String` error becomes a 500 with that sentence in it, because everything a
/// `LibraryError` says is already a sentence about what went wrong.
pub async fn with_library<T, F>(state: &Shared, work: F) -> Result<T, Response>
where
    F: FnOnce(&Library) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let library = state.library.clone();
    let done = tokio::task::spawn_blocking(move || {
        let library = library.lock().expect("the library lock is never poisoned");
        work(&library)
    })
    .await;
    match done {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(fail(StatusCode::INTERNAL_SERVER_ERROR, &e)),
        Err(_) => Err(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "evo stopped part-way through that. Try again.",
        )),
    }
}

/// The bytes of one document, cloned out of the library so the lock is not
/// held while they are parsed or rendered.
pub async fn document_bytes(state: &Shared, id: &str) -> Result<Arc<Vec<u8>>, Response> {
    let wanted = id.to_owned();
    let bytes = with_library(state, move |lib| match lib.doc(&wanted) {
        Ok(None) => Ok(None),
        Ok(Some(_)) => lib.load_bytes(&wanted).map(Some).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    })
    .await?;
    bytes.map(Arc::new).ok_or_else(no_such_document)
}

/// The page sizes of one document, parsed once and then remembered.
///
/// Parsing a PDF is not free and the answer never changes, so the first ask
/// pays for it and every later one is a map lookup.
pub async fn page_sizes(state: &Shared, id: &str) -> Result<Arc<Vec<PageSize>>, Response> {
    if let Some(sizes) = state
        .page_sizes
        .lock()
        .expect("the page-size lock is never poisoned")
        .get(id)
    {
        return Ok(sizes.clone());
    }

    let bytes = document_bytes(state, id).await?;
    let parsed = tokio::task::spawn_blocking(move || {
        // `load_bytes` is the same loader the desktop app uses, so a document
        // the server accepts is one the app can open.
        Document::load_bytes(bytes.as_ref().clone(), None).map(|doc| {
            doc.pages
                .iter()
                .map(|page| PageSize {
                    width: page.width,
                    height: page.height,
                })
                .collect::<Vec<_>>()
        })
    })
    .await;
    let sizes = match parsed {
        Ok(Ok(sizes)) => Arc::new(sizes),
        Ok(Err(e)) => return Err(fail(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
        Err(_) => {
            return Err(fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "evo could not read that document's pages.",
            ));
        }
    };
    state
        .page_sizes
        .lock()
        .expect("the page-size lock is never poisoned")
        .insert(id.to_owned(), sizes.clone());
    Ok(sizes)
}

// ---------------------------------------------------------------------------
// The documents
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// A search. Absent means "everything".
    #[serde(default)]
    pub q: Option<String>,
}

/// `GET /api/docs` -- the whole library, or the hits for a search.
///
/// Search goes through the same [`library_tools`] the model uses, so what the
/// phone sees and what the assistant sees are the same list.
pub async fn list(State(state): State<Shared>, Query(query): Query<ListQuery>) -> Response {
    let search = query
        .q
        .map(|q| q.trim().to_owned())
        .filter(|q| !q.is_empty());
    let listed = with_library(&state, move |lib| match &search {
        Some(query) => library_tools::search_library(lib, query, 50),
        None => library_tools::list_library(lib),
    })
    .await;
    match listed {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

/// What a header may contribute to a document's name.
///
/// Titles arrive in headers because the body is the PDF itself. A header is
/// bytes from the network: control characters are dropped so nothing can be
/// smuggled into a later response, and the length is capped so a library card
/// stays a library card.
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>()
        .trim()
        .to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// What a phone is told when it uploads a document it cannot unlock here.
///
/// The server has no way to ask for a password and nowhere safe to put one,
/// and the library only stores documents everything can read -- so the answer
/// is where to go instead, not a prompt.
pub const ENCRYPTED_UPLOAD: &str = "This PDF is password-protected. Open it in the desktop app and \
                                    add it to the library there.";

/// `POST /api/docs` -- the body is the PDF.
///
/// No multipart: the phone has one file and a fetch with a `Blob` body is the
/// whole upload. The title and filename ride in headers because there is no
/// room for them anywhere else.
pub async fn upload(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if body.is_empty() {
        return fail(
            StatusCode::BAD_REQUEST,
            "That upload had no content. Send the PDF as the body of the request.",
        );
    }

    let filename = header_text(&headers, "x-evo-filename").unwrap_or_else(|| "document.pdf".into());
    let title = header_text(&headers, "x-evo-title").unwrap_or_else(|| {
        filename
            .strip_suffix(".pdf")
            .unwrap_or(&filename)
            .trim()
            .to_owned()
    });
    let title = if title.is_empty() {
        "Document".to_owned()
    } else {
        title
    };

    let bytes = body.to_vec();
    let imported = with_library(&state, move |lib| {
        // Whether this is new matters to the answer, and the library's own
        // de-duplication is silent about it: the same bytes give the same id
        // and the existing record comes back.
        let id = hex_digest(&bytes);
        let existing = lib.doc(&id).map_err(|e| e.to_string())?.is_some();
        match lib.import_bytes(bytes, &title, &filename) {
            Ok(meta) => Ok(Ok((meta, existing))),
            // Unlocking a document is a decision, not a request parameter: it
            // trades the file's protection for a library copy anything can
            // read, and it belongs where somebody can be told that and say no.
            // The inner `Err` carries its own status past `with_library`,
            // which would otherwise call this a server fault.
            Err(crate::library::LibraryError::Doc(e)) if e.wants_password() => {
                Ok(Err(ENCRYPTED_UPLOAD))
            }
            Err(e) => Err(e.to_string()),
        }
    })
    .await;

    let (meta, existing) = match imported {
        Ok(Ok(pair)) => pair,
        Ok(Err(message)) => return fail(StatusCode::UNPROCESSABLE_ENTITY, message),
        // A PDF evo cannot open is the uploader's problem, not the server's.
        Err(response) => return response,
    };
    let status = if existing {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (
        status,
        Json(json!({
            "id": meta.id,
            "title": meta.title,
            "pages": meta.page_count,
            "duplicate": existing,
        })),
    )
        .into_response()
}

/// `GET /api/docs/{id}` -- one document, and how far evo has got with reading
/// it. The viewer shows "still reading this" from `indexed`.
pub async fn document(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let wanted = id.clone();
    let found = with_library(&state, move |lib| {
        let Some(meta) = lib.doc(&wanted).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let indexed = lib.is_indexed(&wanted).unwrap_or(false);
        Ok(Some(json!({
            "id": meta.id,
            "title": meta.title,
            "filename": meta.original_filename,
            "pages": meta.page_count,
            "size": meta.file_size,
            "imported_at": meta.imported_at,
            "tags": meta.all_tags(),
            "summary": meta.summary,
            "indexed": indexed,
            "index_error": meta.index_error,
        })))
    })
    .await;
    match found {
        Ok(Some(value)) => Json(value).into_response(),
        Ok(None) => no_such_document(),
        Err(response) => response,
    }
}

/// `DELETE /api/docs/{id}` -- the record, the blob, the thumbnail, and the
/// rendered pages. Nothing content-addressed survives the document it came
/// from.
pub async fn delete(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let wanted = id.clone();
    let root = state.paths.library_root.clone();
    let removed = with_library(&state, move |lib| {
        if lib.doc(&wanted).map_err(|e| e.to_string())?.is_none() {
            return Ok(false);
        }
        lib.delete(&wanted).map_err(|e| e.to_string())?;
        // The page cache is keyed by document id, and an id is never reused
        // for different bytes, so this is only about disk space.
        let _ = std::fs::remove_dir_all(pages::cache_dir(&root, &wanted));
        Ok(true)
    })
    .await;
    match removed {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => no_such_document(),
        Err(response) => response,
    }
}

/// `GET /api/docs/{id}/manifest` -- everything the viewer needs before it can
/// draw anything: how many pages there are and how big each one is, which
/// markup it would be drawing, and whether there is a conversation to reopen.
pub async fn manifest(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let sizes = match page_sizes(&state, &id).await {
        Ok(sizes) => sizes,
        Err(response) => return response,
    };

    let wanted = id.clone();
    let details = with_library(&state, move |lib| {
        let Some(meta) = lib.doc(&wanted).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let markup = lib.load_markup(&wanted).map_err(|e| e.to_string())?;
        let chat = lib.load_chat(&wanted).map_err(|e| e.to_string())?;
        let etag = markup_api::etag_of(markup.as_ref(), meta.page_count);
        Ok(Some((meta, etag, chat.len())))
    })
    .await;

    let (meta, etag, chat_len) = match details {
        Ok(Some(details)) => details,
        Ok(None) => return no_such_document(),
        Err(response) => return response,
    };
    Json(json!({
        "id": meta.id,
        "title": meta.title,
        "pages": sizes.as_ref(),
        "markup_etag": etag,
        "chat_len": chat_len,
    }))
    .into_response()
}

/// `GET /api/docs/{id}/thumb.png` -- the library card's picture.
///
/// The desktop app draws these in the background as documents are imported;
/// a server has no idle moment to do that in, so a missing one is drawn on the
/// first request and kept.
pub async fn thumbnail(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let wanted = id.clone();
    let path = match with_library(&state, move |lib| Ok(lib.thumb_path(&wanted))).await {
        Ok(path) => path,
        Err(response) => return response,
    };
    if let Ok(png) = tokio::fs::read(&path).await {
        return pages::png_response(png);
    }

    let bytes = match document_bytes(&state, &id).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let target = path.clone();
    let pref = state.config.engine;
    let drawn = tokio::task::spawn_blocking(move || {
        let png = pages::render_png(bytes, 0, pages::Zoom::FitWidth(pages::THUMB_WIDTH), pref)?;
        pages::write_atomically(&target, &png)?;
        Ok::<Vec<u8>, String>(png)
    })
    .await;
    match drawn {
        Ok(Ok(png)) => pages::png_response(png),
        Ok(Err(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(_) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "evo could not draw that document's thumbnail.",
        ),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// `GET /api/status` -- what the server is busy with. The phone polls this
/// while a freshly uploaded document is being read.
pub async fn status(State(state): State<Shared>) -> Response {
    let counted = with_library(&state, |lib| {
        let documents = lib.list().map_err(|e| e.to_string())?.len();
        let index = lib.index_status().map(|s| {
            json!({
                "pending": s.pending,
                "current": s.current,
                "current_id": s.current_id,
                "ocr_pending": s.ocr_pending,
                "ocr_done": s.ocr_done,
                "ocr_total": s.ocr_total,
                "error": s.last_error,
            })
        });
        let enrich = lib.enrich_status().map(|s| {
            json!({
                "pending": s.pending,
                "current": s.current,
                "current_id": s.current_id,
                "done": s.done,
                "error": s.last_error,
            })
        });
        Ok(json!({ "documents": documents, "index": index, "enrich": enrich }))
    })
    .await;

    let mut body: Value = match counted {
        Ok(value) => value,
        Err(response) => return response,
    };
    let model = &state.config.model;
    body["version"] = json!(env!("CARGO_PKG_VERSION"));
    body["blobs"] = json!(state.config.blobs.name());
    body["model"] = if model.api.is_http() {
        json!({ "kind": "http", "model": model.model, "base_url": model.base_url })
    } else {
        json!({ "kind": "builtin", "model": model.builtin_model })
    };
    body["enrich_enabled"] = json!(state.config.assistant.enrich_enabled);
    body["max_upload_mb"] = json!(state.config.max_upload_mb);
    // Whether a question is being answered right now. The permit is taken for
    // exactly as long as the model is running, so this is also how one watches
    // a closed tab stop a generation: it goes back to false on its own.
    body["generating"] = json!(state.generation.available_permits() == 0);
    Json(body).into_response()
}

// ---------------------------------------------------------------------------
// Cache headers
// ---------------------------------------------------------------------------

/// Everything an id addresses is immutable: the id *is* the bytes. So a page
/// image, once fetched, never has to be fetched again. `private` because the
/// document is somebody's, and a shared cache has no business keeping it.
pub const IMMUTABLE: &str = "private, max-age=31536000, immutable";

#[cfg(test)]
mod tests {
    use super::*;

    /// The check that stands between a URL and a filename.
    #[test]
    fn only_a_sha256_digest_is_a_document_id() {
        let real = hex_digest(b"a document");
        assert_eq!(real.len(), 64);
        assert!(is_doc_id(&real));

        for imposter in [
            "",
            "..",
            "../../etc/passwd",
            "a/b",
            &real[..63],
            &format!("{real}0"),
            &real.to_uppercase(),
            &"g".repeat(64),
            &format!("{}/..", &real[..61]),
        ] {
            assert!(!is_doc_id(imposter), "{imposter}");
        }
    }

    #[test]
    fn a_header_names_a_document_without_carrying_anything_else() {
        let mut headers = HeaderMap::new();
        headers.insert("x-evo-title", "  Boiler manual  ".parse().unwrap());
        assert_eq!(
            header_text(&headers, "x-evo-title").as_deref(),
            Some("Boiler manual")
        );
        assert_eq!(header_text(&headers, "x-evo-filename"), None);

        headers.insert("x-evo-title", "   ".parse().unwrap());
        assert_eq!(
            header_text(&headers, "x-evo-title"),
            None,
            "a blank title is no title"
        );

        // A header value cannot hold a newline, but it can hold other control
        // characters, and none of them belong in a title.
        headers.insert("x-evo-title", "tab\there".parse().unwrap());
        assert_eq!(
            header_text(&headers, "x-evo-title").as_deref(),
            Some("tabhere")
        );

        headers.insert("x-evo-title", "x".repeat(500).parse().unwrap());
        assert_eq!(
            header_text(&headers, "x-evo-title").map(|t| t.len()),
            Some(200)
        );
    }
}
