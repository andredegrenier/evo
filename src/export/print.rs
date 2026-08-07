//! Printing on macOS: export the current state to a temporary PDF and hand it
//! to the system — either via Preview (full native print dialog) or straight
//! to the default printer with `lpr`.

use std::path::PathBuf;
use std::process::Command;

use crate::doc::Document;
use crate::doc::page_ops::PageList;
use crate::doc::store::AnnotationStore;
use crate::export::pdf::{ExportError, ExportOptions, export_pdf};

#[derive(Debug, thiserror::Error)]
pub enum PrintError {
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error("could not launch the system print handler: {0}")]
    Launch(std::io::Error),
}

fn temp_pdf(doc: &Document) -> PathBuf {
    let stem = doc
        .path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".into());
    std::env::temp_dir().join(format!("evo-print-{stem}-{}.pdf", std::process::id()))
}

/// Open the (flattened) document in the system PDF viewer so the user gets
/// the full native print dialog. Returns the temp file for later cleanup.
pub fn print_via_system_viewer(
    doc: &Document,
    pages: &PageList,
    store: &AnnotationStore,
) -> Result<PathBuf, PrintError> {
    let path = temp_pdf(doc);
    export_pdf(doc, pages, store, ExportOptions { flatten: true }, &path)?;
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(&path).spawn();
    #[cfg(not(target_os = "macos"))]
    let result = Command::new("xdg-open").arg(&path).spawn();
    result.map_err(PrintError::Launch)?;
    Ok(path)
}

/// Print silently to the default printer via CUPS.
pub fn print_direct(
    doc: &Document,
    pages: &PageList,
    store: &AnnotationStore,
) -> Result<PathBuf, PrintError> {
    let path = temp_pdf(doc);
    export_pdf(doc, pages, store, ExportOptions { flatten: true }, &path)?;
    Command::new("lpr")
        .arg(&path)
        .spawn()
        .map_err(PrintError::Launch)?;
    Ok(path)
}
