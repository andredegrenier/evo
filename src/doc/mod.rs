//! The document model: source PDF bytes plus everything the editor layers on
//! top (markup annotations, page operations, undo history). The source bytes
//! are never mutated; hayro renders from them and lopdf re-reads them at
//! export time.

pub mod annotation;
pub mod geometry;
pub mod history;
pub mod page_ops;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;

use hayro::hayro_syntax::Pdf;

/// Static per-page metadata extracted at load time.
#[derive(Clone, Copy, Debug)]
pub struct PageInfo {
    /// Displayed width in PDF points (intrinsic `/Rotate` already applied).
    pub width: f32,
    /// Displayed height in PDF points.
    pub height: f32,
    /// The page's intrinsic `/Rotate` in clockwise degrees (0/90/180/270).
    pub intrinsic_rotation: i64,
    /// Bottom-left corner of the intersected crop box in raw PDF user space.
    pub crop_origin: (f32, f32),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("could not read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("this PDF is encrypted; evo does not support encrypted PDFs yet")]
    Encrypted,
    #[error("not a valid PDF file")]
    Invalid,
    #[error("this PDF contains no pages")]
    Empty,
}

/// An opened PDF document.
pub struct Document {
    pub source: Arc<Vec<u8>>,
    pub path: Option<PathBuf>,
    pub pages: Vec<PageInfo>,
}

impl Document {
    pub fn load_path(path: PathBuf) -> Result<Self, LoadError> {
        let bytes = std::fs::read(&path)?;
        Self::load_bytes(bytes, Some(path))
    }

    pub fn load_bytes(bytes: Vec<u8>, path: Option<PathBuf>) -> Result<Self, LoadError> {
        let source = Arc::new(bytes);
        let pdf = Pdf::new(source.clone()).map_err(|e| match e {
            hayro::hayro_syntax::LoadPdfError::Decryption(_) => LoadError::Encrypted,
            hayro::hayro_syntax::LoadPdfError::Invalid => LoadError::Invalid,
        })?;

        let pages: Vec<PageInfo> = pdf
            .pages()
            .iter()
            .map(|page| {
                let (width, height) = page.render_dimensions();
                let crop = page.intersected_crop_box();
                PageInfo {
                    width,
                    height,
                    intrinsic_rotation: rotation_degrees(page.rotation()),
                    crop_origin: (crop.x0 as f32, crop.y0 as f32),
                }
            })
            .collect();

        if pages.is_empty() {
            return Err(LoadError::Empty);
        }

        Ok(Self {
            source,
            path,
            pages,
        })
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned())
    }
}

fn rotation_degrees(r: hayro::hayro_syntax::page::Rotation) -> i64 {
    use hayro::hayro_syntax::page::Rotation;
    match r {
        Rotation::None => 0,
        Rotation::Horizontal => 90,
        Rotation::Flipped => 180,
        Rotation::FlippedHorizontal => 270,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_fixture() {
        let doc = Document::load_path("tests/fixtures/sample.pdf".into()).unwrap();
        assert_eq!(doc.pages.len(), 2);
        assert!((doc.pages[0].width - 612.0).abs() < 0.5);
        assert!((doc.pages[0].height - 792.0).abs() < 0.5);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Document::load_bytes(b"not a pdf at all".to_vec(), None),
            Err(LoadError::Invalid)
        ));
    }
}
