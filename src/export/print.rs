//! Printing: export the current state to a temporary PDF and hand it to the
//! operating system — either the default PDF viewer (which provides the
//! native print dialog) or directly to the default printer.

use std::path::{Path, PathBuf};
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
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    #[error("direct printing is not supported on this platform")]
    Unsupported,
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

/// Open a file with the platform's default handler.
fn open_with_default_app(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // `start` is a cmd.exe builtin; the empty string is the window title.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd.spawn().map(|_| ())
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
    open_with_default_app(&path).map_err(PrintError::Launch)?;
    Ok(path)
}

/// Print to the default printer without a dialog: CUPS `lpr` on macOS/Linux,
/// the shell `Print` verb on Windows.
pub fn print_direct(
    doc: &Document,
    pages: &PageList,
    store: &AnnotationStore,
) -> Result<PathBuf, PrintError> {
    let path = temp_pdf(doc);
    export_pdf(doc, pages, store, ExportOptions { flatten: true }, &path)?;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Command::new("lpr")
            .arg(&path)
            .spawn()
            .map_err(PrintError::Launch)?;
        Ok(path)
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Process", "-Verb", "Print"])
            .arg(&path)
            .spawn()
            .map_err(PrintError::Launch)?;
        Ok(path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
        Err(PrintError::Unsupported)
    }
}
