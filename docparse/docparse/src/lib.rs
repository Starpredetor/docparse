//! Structure-preserving document extraction with a bounded memory budget.
//!
//! No network access, no models, no LLM. See
//! `docs/superpowers/specs/2026-10-04-docparse-design.md`.

pub mod chunk;
pub mod error;
pub mod model;
pub mod normalize;
pub mod source;

#[cfg(feature = "docx")]
pub mod docx;

#[cfg(feature = "pdf")]
pub mod pdf;

#[cfg(feature = "image")]
pub mod image;

#[cfg(feature = "ocr")]
pub mod ocr;

pub use chunk::{split_sentences, ChunkConfig, Chunker};
pub use error::{Error, Result};
pub use model::{Block, BlockKind, BlockRef, Chunk, Page, PageOrigin, Rect};
pub use source::Source;
