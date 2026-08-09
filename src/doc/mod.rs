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
    /// The document is password-protected and nobody has offered a password
    /// yet. The desktop app answers this one by asking.
    #[error("this PDF is password-protected; a password is needed to open it")]
    NeedsPassword,
    /// A password was offered and the document rejected it.
    #[error("that password does not open this PDF")]
    WrongPassword,
    /// The file is encrypted in a way evo's reader does not implement. No
    /// password will help, so the message must not invite one.
    #[error("this PDF uses an encryption evo does not support yet")]
    UnsupportedEncryption,
    #[error("not a valid PDF file")]
    Invalid,
    #[error("this PDF contains no pages")]
    Empty,
}

impl LoadError {
    /// Whether asking the person for a password is worth doing. True for both
    /// "none supplied" and "the one supplied was wrong" -- the second is a
    /// retry, not a dead end.
    pub fn wants_password(&self) -> bool {
        matches!(self, LoadError::NeedsPassword | LoadError::WrongPassword)
    }
}

/// An opened PDF document.
pub struct Document {
    pub source: Arc<Vec<u8>>,
    pub path: Option<PathBuf>,
    pub pages: Vec<PageInfo>,
    /// The password this document was opened with, kept only for as long as
    /// the document is open: everything that re-parses the source bytes --
    /// rendering, text extraction, export -- has to present it again. It is
    /// never written to disk, never logged, and never put in an error message.
    pub password: Option<String>,
}

impl Document {
    /// Read and open a file, offering `password` if it wants one.
    pub fn load_path(path: PathBuf, password: Option<&str>) -> Result<Self, LoadError> {
        let bytes = std::fs::read(&path)?;
        Self::load_bytes_with_password(bytes, Some(path), password)
    }

    pub fn load_bytes(bytes: Vec<u8>, path: Option<PathBuf>) -> Result<Self, LoadError> {
        Self::load_bytes_with_password(bytes, path, None)
    }

    /// Open `bytes`, offering `password` if the document asks for one.
    ///
    /// hayro cannot tell "no password was given" from "the wrong password was
    /// given" -- both are `PasswordProtected` -- so the distinction is drawn
    /// here, from whether this caller actually supplied one. That is the only
    /// place the two cases are still distinguishable, and the difference
    /// matters: one opens a dialog, the other says try again.
    pub fn load_bytes_with_password(
        bytes: Vec<u8>,
        path: Option<PathBuf>,
        password: Option<&str>,
    ) -> Result<Self, LoadError> {
        use hayro::hayro_syntax::{DecryptionError, LoadPdfError};

        let source = Arc::new(bytes);
        let pdf =
            Pdf::new_with_password(source.clone(), password.unwrap_or_default()).map_err(|e| {
                match e {
                    LoadPdfError::Decryption(DecryptionError::PasswordProtected) => {
                        if password.is_some() {
                            LoadError::WrongPassword
                        } else {
                            LoadError::NeedsPassword
                        }
                    }
                    LoadPdfError::Decryption(_) => LoadError::UnsupportedEncryption,
                    LoadPdfError::Invalid => LoadError::Invalid,
                }
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
            password: password.map(str::to_owned),
        })
    }

    /// The password to present when re-parsing [`Self::source`].
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
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
pub(crate) mod tests {
    use super::*;

    #[test]
    fn loads_fixture() {
        let doc = Document::load_path("tests/fixtures/sample.pdf".into(), None).unwrap();
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

    /// The three password-protected fixtures, each encrypted a different way.
    /// The user password on all of them is `evo`; the owner password is
    /// `evo-owner`.
    pub(crate) const PROTECTED: [&str; 3] = [
        "tests/fixtures/encrypted-rc4.pdf",
        "tests/fixtures/encrypted-aes128.pdf",
        "tests/fixtures/encrypted-aes256.pdf",
    ];

    /// A fixture's bytes.
    pub(crate) fn encrypted(path: &str) -> Vec<u8> {
        std::fs::read(path).expect("the fixture")
    }

    /// RC4 (R3), AES-128 (R4) and AES-256 (R6) all open with the right
    /// password. This pins hayro's password support, which is real but
    /// undocumented: an upgrade that dropped it would fail here rather than in
    /// somebody's hands.
    #[test]
    fn every_encryption_evo_ships_a_fixture_for_opens_with_its_password() {
        for path in PROTECTED {
            let doc = Document::load_bytes_with_password(encrypted(path), None, Some("evo"))
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(doc.pages.len(), 2, "{path}");
            assert!((doc.pages[0].width - 612.0).abs() < 0.5, "{path}");
            assert_eq!(doc.password(), Some("evo"), "{path}");
        }
    }

    /// No password offered: the app has to know to ask for one.
    #[test]
    fn a_protected_pdf_with_no_password_asks_for_one() {
        for path in PROTECTED {
            let err = Document::load_bytes(encrypted(path), None)
                .map(|d| d.pages.len())
                .expect_err(path);
            assert!(matches!(err, LoadError::NeedsPassword), "{path}: {err:?}");
            assert!(err.wants_password(), "{path}");
            assert!(err.to_string().contains("password-protected"), "{err}");
        }
    }

    /// A password was offered and refused: that is a retry, not a first ask,
    /// and hayro reports both identically -- so this is the test that proves
    /// the distinction is drawn from the caller's side.
    #[test]
    fn a_rejected_password_is_told_apart_from_no_password() {
        for path in PROTECTED {
            let err = Document::load_bytes_with_password(encrypted(path), None, Some("not-it"))
                .map(|d| d.pages.len())
                .expect_err(path);
            assert!(matches!(err, LoadError::WrongPassword), "{path}: {err:?}");
            assert!(err.wants_password(), "{path}");
            // The password the person typed must not come back at them.
            assert!(!err.to_string().contains("not-it"), "{err}");
        }
    }

    /// A document encrypted with an empty user password -- permissions-only
    /// protection, common in published documents -- opens with no password at
    /// all and no dialog, which is what every other reader does. Pinning it
    /// here so the desktop flow can rely on never prompting for these.
    #[test]
    fn an_empty_user_password_opens_without_being_asked() {
        let doc = Document::load_bytes(encrypted("tests/fixtures/encrypted-empty-user.pdf"), None)
            .expect("empty user password needs no password");
        assert_eq!(doc.pages.len(), 2);
        // Nothing was supplied, so nothing is remembered.
        assert_eq!(doc.password(), None);
    }

    /// The same file with a password that is not its (empty) one is still a
    /// refusal -- an empty user password does not mean "any password works".
    #[test]
    fn an_empty_user_password_still_refuses_a_wrong_one() {
        let err = Document::load_bytes_with_password(
            encrypted("tests/fixtures/encrypted-empty-user.pdf"),
            None,
            Some("not-it"),
        )
        .map(|d| d.pages.len())
        .expect_err("wrong password");
        assert!(matches!(err, LoadError::WrongPassword), "{err:?}");
    }

    /// Only the two password errors invite a dialog; an unsupported cipher
    /// must not send somebody typing passwords at a file no password opens.
    #[test]
    fn only_password_errors_ask_for_a_password() {
        assert!(LoadError::NeedsPassword.wants_password());
        assert!(LoadError::WrongPassword.wants_password());
        assert!(!LoadError::UnsupportedEncryption.wants_password());
        assert!(!LoadError::Invalid.wants_password());
        assert!(
            LoadError::UnsupportedEncryption
                .to_string()
                .contains("does not support")
        );
    }
}
