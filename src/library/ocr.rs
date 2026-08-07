//! OCR for scanned pages via the pure-Rust `ocrs` engine.
//!
//! The neural models (~10.5 MB total) are licensed CC-BY-SA-4.0 by Robert
//! Knight and are therefore downloaded into the library's `models/` directory
//! on first use rather than bundled with the (MIT/Apache) binary. Offline
//! installs can place `text-detection.rten` and `text-recognition.rten`
//! there manually.

use std::path::{Path, PathBuf};

use ocrs::{ImageSource, OcrEngine, OcrEngineParams};

const DETECTION: &str = "text-detection.rten";
const RECOGNITION: &str = "text-recognition.rten";

/// Mirrors, tried in order.
const MODEL_BASE_URLS: [&str; 2] = [
    "https://ocrs-models.s3-accelerate.amazonaws.com",
    "https://huggingface.co/robertknight/ocrs/resolve/main",
];

/// Maximum long-edge of the rasterized page fed to OCR.
const MAX_OCR_PIXELS: f32 = 2200.0;

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("could not download OCR models: {0}")]
    Download(String),
    #[error("could not initialize the OCR engine: {0}")]
    Init(String),
    #[error("OCR failed: {0}")]
    Run(String),
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let mut response = ureq::get(url).call().map_err(|e| e.to_string())?;
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    if bytes.len() < 1024 {
        return Err(format!("suspiciously small download from {url}"));
    }
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// Make sure both model files exist in `dir`, downloading if needed.
pub fn ensure_models(dir: &Path) -> Result<(PathBuf, PathBuf), OcrError> {
    std::fs::create_dir_all(dir).map_err(|e| OcrError::Download(e.to_string()))?;
    let mut paths = Vec::with_capacity(2);
    for name in [DETECTION, RECOGNITION] {
        let dest = dir.join(name);
        if !dest.exists() {
            let mut last_err = String::new();
            let mut ok = false;
            for base in MODEL_BASE_URLS {
                match download(&format!("{base}/{name}"), &dest) {
                    Ok(()) => {
                        ok = true;
                        break;
                    }
                    Err(e) => last_err = e,
                }
            }
            if !ok {
                return Err(OcrError::Download(last_err));
            }
        }
        paths.push(dest);
    }
    Ok((paths[0].clone(), paths[1].clone()))
}

pub struct Ocr {
    engine: OcrEngine,
}

impl Ocr {
    /// Load models from `dir` (downloading them first if missing).
    pub fn load(dir: &Path) -> Result<Self, OcrError> {
        let (detection, recognition) = ensure_models(dir)?;
        let detection_model =
            rten::Model::load_file(detection).map_err(|e| OcrError::Init(e.to_string()))?;
        let recognition_model =
            rten::Model::load_file(recognition).map_err(|e| OcrError::Init(e.to_string()))?;
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(detection_model),
            recognition_model: Some(recognition_model),
            ..Default::default()
        })
        .map_err(|e| OcrError::Init(e.to_string()))?;
        Ok(Self { engine })
    }

    /// Recognize text in an RGBA bitmap.
    pub fn recognize_rgba(&self, rgba: &[u8], width: u32, height: u32) -> Result<String, OcrError> {
        let source = ImageSource::from_bytes(rgba, (width, height))
            .map_err(|e| OcrError::Run(e.to_string()))?;
        let input = self
            .engine
            .prepare_input(source)
            .map_err(|e| OcrError::Run(e.to_string()))?;
        self.engine
            .get_text(&input)
            .map_err(|e| OcrError::Run(e.to_string()))
    }
}

/// Rasterize one page for OCR and recognize its text.
pub fn ocr_page(
    ocr: &Ocr,
    page: &hayro::hayro_syntax::page::Page<'_>,
    settings: &hayro::hayro_interpret::InterpreterSettings,
) -> Result<String, OcrError> {
    use hayro::vello_cpu::color::AlphaColor;
    use hayro::{RenderCache, RenderSettings};

    let (w, h) = page.render_dimensions();
    let scale = (MAX_OCR_PIXELS / w.max(h).max(1.0)).clamp(0.5, 4.0);
    let pixmap = hayro::render(
        page,
        &RenderCache::new(),
        settings,
        &RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: None,
            height: None,
            bg_color: AlphaColor::WHITE,
        },
    );
    // Fully opaque white background => premultiplied == straight RGBA.
    ocr.recognize_rgba(
        pixmap.data_as_u8_slice(),
        pixmap.width() as u32,
        pixmap.height() as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document as LoDocument, Object, Stream, dictionary};

    /// Build an image-only (scanned-style) PDF from a hayro render of the
    /// text fixture, so OCR is the only way to read it.
    fn scanned_pdf() -> Vec<u8> {
        use hayro::vello_cpu::color::AlphaColor;
        use hayro::{RenderCache, RenderSettings};

        let bytes = std::fs::read("tests/fixtures/sample.pdf").unwrap();
        let pdf = hayro::hayro_syntax::Pdf::new(bytes).unwrap();
        let pages = pdf.pages();
        let pixmap = hayro::render(
            &pages[0],
            &RenderCache::new(),
            &hayro::hayro_interpret::InterpreterSettings::default(),
            &RenderSettings {
                x_scale: 2.0,
                y_scale: 2.0,
                width: None,
                height: None,
                bg_color: AlphaColor::WHITE,
            },
        );
        let (w, h) = (pixmap.width() as i64, pixmap.height() as i64);
        let rgb: Vec<u8> = pixmap
            .data_as_u8_slice()
            .chunks(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();

        let mut doc = LoDocument::with_version("1.7");
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => w,
                "Height" => h,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            rgb,
        ));
        let content = format!("q {w2} 0 0 {h2} 0 0 cm /Im0 Do Q", w2 = 612, h2 = 792);
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(content_id),
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => Object::Reference(image_id) },
            },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Full pipeline: image-only page yields no embedded text, then OCR
    /// recovers it. Downloads ~10 MB of models on first run.
    #[test]
    #[ignore = "downloads OCR models; run with --ignored"]
    fn ocr_recovers_scanned_text() {
        let bytes = scanned_pdf();
        let pdf = hayro::hayro_syntax::Pdf::new(bytes).unwrap();
        let settings = hayro::hayro_interpret::InterpreterSettings::default();
        let pages = pdf.pages();

        let extracted = crate::library::extract::extract_page_text(&pages[0], &settings);
        assert!(
            extracted.text.trim().len() < 32,
            "should have no text layer"
        );

        let models_dir = std::env::temp_dir().join("evo-ocr-models");
        let ocr = Ocr::load(&models_dir).expect("model download + init");
        let text = ocr_page(&ocr, &pages[0], &settings).unwrap();
        let lower = text.to_lowercase();
        assert!(
            lower.contains("quick") || lower.contains("fixture"),
            "OCR text: {text}"
        );
    }
}
