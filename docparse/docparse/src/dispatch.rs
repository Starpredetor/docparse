//! Extension-to-backend dispatch: the single point every backend registers at.

use std::path::Path;

use crate::{Error, Result, Source};

/// Extensions we know how to open. Kept in one place so `collect_inputs`
/// and `open_source` cannot drift apart.
pub const SUPPORTED: &[&str] = &[
    "docx", "pdf", "png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff",
];

/// The single dispatch point. Every backend task registers here and
/// nothing else in the CLI needs to know which formats exist.
pub fn open_source(path: &Path, ocr: bool, ocr_dpi: u32) -> Result<Box<dyn Source>> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "docx" => Ok(Box::new(crate::docx::DocxSource::open(path)?)),
        "pdf" => Ok(Box::new(
            crate::pdf::PdfSource::open(path)?.with_ocr(ocr, ocr_dpi),
        )),

        // Always OCR: without the `ocr` feature this is `OcrUnavailable`.
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" => {
            Ok(Box::new(crate::image::ImageSource::open(path)?))
        }

        _ => Err(Error::Unsupported {
            path: path.to_path_buf(),
        }),
    }
}

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .map(|e| SUPPORTED.contains(&e.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}
