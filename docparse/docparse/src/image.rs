//! Image input. An image is always a single page, and always OCR.

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::Page;
use crate::source::Source;

#[cfg(feature = "ocr")]
use crate::model::PageOrigin;

#[derive(Debug)]
pub struct ImageSource {
    page: Option<Page>,
}

impl ImageSource {
    pub fn open(path: &Path) -> Result<Self> {
        // Open the file ourselves so a missing file is a real I/O error
        // rather than a "malformed document", as the other backends do.
        std::fs::File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Decode first, so a corrupt file fails here with a clear error
        // rather than inside the OCR subprocess.
        let decoded = ::image::open(path).map_err(|e| Error::Malformed {
            path: path.to_path_buf(),
            detail: format!("could not decode image: {e}"),
        })?;
        let (width, height) = (decoded.width() as f32, decoded.height() as f32);
        drop(decoded); // tesseract re-reads the file itself

        let page = Self::ocr_page(path, width, height)?;
        Ok(Self { page: Some(page) })
    }

    #[cfg(feature = "ocr")]
    fn ocr_page(path: &Path, width: f32, height: f32) -> Result<Page> {
        let (text, confidence) = crate::ocr::ocr_image_file(path)?;
        Ok(Page {
            number: 1,
            width,
            height,
            origin: PageOrigin::Ocr { confidence },
            blocks: crate::ocr::blocks_from_ocr_text(&text),
        })
    }

    #[cfg(not(feature = "ocr"))]
    fn ocr_page(path: &Path, _width: f32, _height: f32) -> Result<Page> {
        // An image has no text without OCR; an empty page would be a silent lie.
        Err(Error::OcrUnavailable {
            path: path.to_path_buf(),
        })
    }
}

impl Source for ImageSource {
    fn next_page(&mut self) -> Option<Result<Page>> {
        self.page.take().map(Ok)
    }
}
