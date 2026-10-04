//! PDF extraction via PDFium.
//!
//! Streams one page at a time: the document handle stays open and each
//! `next_page` call reads exactly one page's text, so peak memory is a
//! function of the largest page rather than the whole document.
//!
//! That bound applies to page *content* only: one page's handles are created
//! and dropped per call. PDFium itself retains roughly 3.4 KB per page of
//! document index (page tree, xref), so total memory grows linearly with page
//! count. Measured peak RSS, debug build: 2 pages 9.3 MB, 3,000 pages 22.5 MB,
//! 12,000 pages 50.7 MB, so about 16-17k simple pages would cross 64 MB.
//!
//! Each block is a text *line* (`BlockKind::Line`). Reconstructing paragraphs
//! or multi-column reading order is a layout heuristic and out of scope.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pdfium_render::prelude::*;

use crate::error::{Error, Result};
use crate::model::{Block, BlockKind, Page, PageOrigin, Rect};
use crate::normalize::normalize_block_text;
use crate::source::Source;

/// The process-wide PDFium instance.
///
/// `PdfDocument<'a>` borrows from `Pdfium`, so a struct owning both would be
/// self-referential and the borrow checker rejects it. Borrowing from a
/// genuinely `'static` instance gives `PdfSource` a `PdfDocument<'static>`
/// with no `unsafe`. It also means the shared library is bound once, not once
/// per document.
static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

/// Held across check-and-bind so only one thread ever calls
/// `bind_to_system_library`.
static BIND_LOCK: Mutex<()> = Mutex::new(());

/// Binds PDFium on first use and returns the shared instance.
///
/// Resolves the system library (on Windows, `pdfium.dll` beside the executable or on `PATH`).
fn pdfium() -> Result<&'static Pdfium> {
    if let Some(p) = PDFIUM.get() {
        return Ok(p);
    }
    // A second bind attempt in the same process is not harmlessly dropped:
    // pdfium-render returns `PdfiumLibraryBindingsAlreadyInitialized` as an
    // error. So the lock makes check-and-bind atomic; a thread that waited
    // re-checks and finds the winner's instance instead of binding again.
    let _guard = BIND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = PDFIUM.get() {
        return Ok(p);
    }
    let bindings = Pdfium::bind_to_system_library().map_err(|e| Error::Backend {
        backend: "pdfium",
        detail: format!(
            "{e} (pdfium.dll must be beside the executable or on PATH, and its build may be              older than the pdfium-render feature compiled in; see the comment in Cargo.toml)"
        ),
    })?;
    Ok(PDFIUM.get_or_init(|| Pdfium::new(bindings)))
}

/// Reads a `.pdf` file lazily, one page per `next_page` call.
pub struct PdfSource {
    path: PathBuf,
    document: PdfDocument<'static>,
    next_index: PdfPageIndex,
    page_count: PdfPageIndex,
}

impl PdfSource {
    /// Opens `path`. Returns `Error::Io` if the file cannot be opened and
    /// `Error::Malformed` if it is not a PDF or PDFium is unavailable.
    pub fn open(path: &Path) -> Result<Self> {
        // Open the file ourselves so a missing file is a real I/O error
        // rather than a stringified PDFium one.
        std::fs::File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let document = pdfium()?
            .load_pdf_from_file(path, None)
            .map_err(|e| Error::Malformed {
                path: path.to_path_buf(),
                detail: format!("could not open as PDF: {e}"),
            })?;

        let page_count = document.pages().len();
        Ok(Self {
            path: path.to_path_buf(),
            document,
            next_index: 0,
            page_count,
        })
    }

    fn read_page(&self, index: PdfPageIndex) -> Result<Page> {
        let number = index.unsigned_abs() + 1;
        let page_err = |detail: String| Error::Page {
            path: self.path.clone(),
            page: number,
            detail,
        };

        let page = self
            .document
            .pages()
            .get(index)
            .map_err(|e| page_err(e.to_string()))?;
        let width = page.width().value;
        let height = page.height().value;

        let text = page
            .text()
            .map_err(|e| page_err(format!("could not read text: {e}")))?;

        let mut blocks: Vec<Block> = Vec::new();
        for segment in text.segments().iter() {
            let cleaned = normalize_block_text(&segment.text());
            if cleaned.is_empty() {
                continue;
            }
            // PDFium uses a bottom-left origin: `bottom` is numerically
            // smaller than `top`, so y0 < y1 holds.
            let bounds = segment.bounds();
            blocks.push(Block {
                id: blocks.len() as u32,
                bbox: Some(Rect {
                    x0: bounds.left().value,
                    y0: bounds.bottom().value,
                    x1: bounds.right().value,
                    y1: bounds.top().value,
                }),
                kind: BlockKind::Line,
                text: cleaned,
            });
        }

        Ok(Page {
            number,
            width,
            height,
            origin: PageOrigin::Native,
            blocks,
        })
    }
}

impl Source for PdfSource {
    fn next_page(&mut self) -> Option<Result<Page>> {
        if self.next_index >= self.page_count {
            return None;
        }
        let index = self.next_index;
        self.next_index += 1;
        // The page and its text handle are dropped when `read_page` returns;
        // only the finished `Page` escapes. This is the streaming guarantee.
        Some(self.read_page(index))
    }
}

impl std::fmt::Debug for PdfSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfSource")
            .field("path", &self.path)
            .field("next_index", &self.next_index)
            .field("page_count", &self.page_count)
            .finish_non_exhaustive()
    }
}
