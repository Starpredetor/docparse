use std::path::PathBuf;

use thiserror::Error;

/// Every variant carries the file it came from, so a batch failure is
/// always attributable to one input.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{path}: could not read file: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: unsupported file type")]
    Unsupported { path: PathBuf },

    #[error("{path}: malformed document: {detail}")]
    Malformed { path: PathBuf, detail: String },

    #[error("{path} page {page}: {detail}")]
    Page {
        path: PathBuf,
        page: u32,
        detail: String,
    },

    /// An external library or tool the backend needs is missing or
    /// unusable. This is an environment fault, not a bad input file.
    #[error("{backend} backend unavailable: {detail}")]
    Backend {
        backend: &'static str,
        detail: String,
    },

    #[error("{path}: page has no extractable text and OCR is not enabled")]
    OcrUnavailable { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, Error>;
