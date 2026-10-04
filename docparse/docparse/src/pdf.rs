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
//! With `PdfSource::with_ocr`, a page with no text is rendered and OCR'd. That
//! breaks the memory figures above: see the Memory section of `crate::ocr`.
//!
//! Each block is a text *line* (`BlockKind::Line`). PDFium reports text in
//! runs split at font and style changes, often mid-word (ligatures,
//! superscripts, hyperlinks), so consecutive runs on one visual line are merged
//! into a single block by `merge_segments`, which decides per boundary whether
//! a space belongs. Reconstructing paragraphs or multi-column reading order is
//! a layout heuristic and out of scope.

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
            "{e} (pdfium.dll must be beside the executable or on PATH, and its build may be older than the pdfium-render feature compiled in; see the comment in Cargo.toml)"
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
    /// When true, a page with no extractable text is rendered and OCR'd.
    ocr_fallback: bool,
    ocr_dpi: u32,
}

impl PdfSource {
    /// Opens `path`. Returns `Error::Io` if the file cannot be opened and
    /// `Error::Malformed` if it is not a PDF; returns `Error::Backend` if PDFium
    /// is unavailable.
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
            ocr_fallback: false,
            ocr_dpi: 150,
        })
    }

    /// Enables OCR for pages that yield no text. Off by default: implicit
    /// OCR is slow and its output is a guess, so the caller must ask.
    /// Without the `ocr` feature, a text-free page then fails with
    /// `Error::OcrUnavailable` instead of silently yielding nothing.
    pub fn with_ocr(mut self, enabled: bool, dpi: u32) -> Self {
        self.ocr_fallback = enabled;
        self.ocr_dpi = dpi;
        self
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

        // `merge_segments` consumes one page's segments and emits finished
        // lines; the only buffer is the single line being built (`Line`).
        let segments: Vec<Segment> = text
            .segments()
            .iter()
            .map(|segment| {
                // PDFium uses a bottom-left origin: `bottom` is numerically
                // smaller than `top`, so y0 < y1 holds.
                let bounds = segment.bounds();
                Segment {
                    text: segment.text(),
                    bbox: Rect {
                        x0: bounds.left().value,
                        y0: bounds.bottom().value,
                        x1: bounds.right().value,
                        y1: bounds.top().value,
                    },
                }
            })
            .collect();
        let blocks = merge_segments(segments);

        if blocks.is_empty() && self.ocr_fallback {
            #[cfg(feature = "ocr")]
            return self.ocr_page(&page, number, width, height);
            #[cfg(not(feature = "ocr"))]
            return Err(Error::OcrUnavailable {
                path: self.path.clone(),
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

    /// Renders the page, stages it as a PPM for tesseract, drops the bitmap,
    /// then OCRs. The bitmap is the largest allocation in the program, so it
    /// must not be alive while OCR output is processed. PPM is written row by
    /// row from the BGRA buffer, so no second full-size copy is made.
    #[cfg(feature = "ocr")]
    fn ocr_page(&self, page: &PdfPage<'_>, number: u32, width: f32, height: f32) -> Result<Page> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};

        static STAGE_ID: AtomicU64 = AtomicU64::new(0);

        let page_err = |detail: String| Error::Page {
            path: self.path.clone(),
            page: number,
            detail,
        };

        let config = PdfRenderConfig::new().scale_page_by_factor(self.ocr_dpi as f32 / 72.0);
        let bitmap = page
            .render_with_config(&config)
            .map_err(|e| page_err(format!("could not render page for OCR: {e}")))?;

        let temp = std::env::temp_dir().join(format!(
            "docparse-ocr-{}-{}.ppm",
            std::process::id(),
            STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let staged = (|| -> std::io::Result<()> {
            let (w, h) = (bitmap.width() as usize, bitmap.height() as usize);
            let bytes = bitmap.as_raw_bytes();
            let mut out = std::io::BufWriter::new(std::fs::File::create(&temp)?);
            write!(
                out,
                "P6
{w} {h}
255
"
            )?;
            let stride = bytes.len() / h.max(1);
            let mut row = Vec::with_capacity(w * 3);
            for y in 0..h {
                row.clear();
                for px in bytes[y * stride..y * stride + w * 4].as_chunks::<4>().0 {
                    row.extend_from_slice(&[px[2], px[1], px[0]]); // BGRA -> RGB
                }
                out.write_all(&row)?;
            }
            out.flush()
        })();
        drop(bitmap);
        if let Err(e) = staged {
            let _ = std::fs::remove_file(&temp);
            return Err(page_err(format!("could not stage page image for OCR: {e}")));
        }

        let result = crate::ocr::ocr_image_file(&temp);
        let _ = std::fs::remove_file(&temp);
        let (text, confidence) = result?;

        Ok(Page {
            number,
            width,
            height,
            origin: PageOrigin::Ocr { confidence },
            blocks: crate::ocr::blocks_from_ocr_text(&text),
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

/// One text run as PDFium reports it, before merging.
struct Segment {
    text: String,
    bbox: Rect,
}

/// Two runs are on the same line when their vertical centres differ by at most
/// this fraction of the taller run's height. Half a line height keeps
/// superscripts and subscripts (offset, but overlapping) on their line while
/// separating real consecutive lines (offset by about 1.2 heights).
const LINE_TOLERANCE: f32 = 0.5;

/// A horizontal gap wider than this fraction of text height is a word break.
///
/// Measured on a 20-PDF corpus: intra-word run boundaries (ligatures, trailing
/// punctuation, superscript markers, URL fragments) had gaps of 0.27 to 1.11 pt
/// on 9-10 pt body text, whose bbox height is about 11-12 pt; a real space is
/// typically 2-3 pt. 0.2 x 11.5 pt = 2.3 pt sits between the two populations.
/// It is a heuristic: PDFium exposes no per-font space width here.
const SPACE_GAP: f32 = 0.2;

/// The line currently being built. Only this one line is buffered.
struct Line {
    text: String,
    bbox: Rect,
    /// Whether the last merged run ended in whitespace of its own.
    trailing_space: bool,
}

/// Rotated text (table headers, margin stamps) reports a tall, narrow box
/// per word, and neighbouring words stack side by side with small
/// horizontal gaps. Treating those as one line welds separate words
/// ("ApplicationDomain"), so a run of 3+ characters whose box is more than
/// twice as tall as wide is never merged with its neighbours.
fn is_vertical(text: &str, r: &Rect) -> bool {
    text.chars().count() >= 3 && height(r) > 2.0 * (r.x1 - r.x0)
}

fn height(r: &Rect) -> f32 {
    r.y1 - r.y0
}

fn same_line(prev: &Rect, next: &Rect) -> bool {
    let tall = height(prev).max(height(next));
    let dy = ((prev.y0 + prev.y1) - (next.y0 + next.y1)).abs() / 2.0;
    // Going backwards (next starts left of where prev starts) is a new
    // column or a new line, never a continuation.
    dy <= LINE_TOLERANCE * tall && next.x0 >= prev.x0
}

/// Merges runs that belong to the same visual line, inserting a space only
/// where the horizontal gap is wide enough to be a real word break.
///
/// PDFium splits runs at font and style changes, which happens mid-word for
/// ligatures and around punctuation, so joining every run with a space
/// corrupts roughly 1.7% of words on real documents.
fn merge_segments(segments: Vec<Segment>) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Line> = None;
    // Bbox of the previous run alone (not the line union): the gap is
    // measured between neighbours.
    let mut last_run = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    };

    let mut last_vertical = false;

    let flush = |line: Option<Line>, blocks: &mut Vec<Block>| {
        if let Some(line) = line {
            blocks.push(Block {
                id: blocks.len() as u32,
                bbox: Some(line.bbox),
                kind: BlockKind::Line,
                text: line.text,
            });
        }
    };

    for segment in segments {
        let cleaned = normalize_block_text(&segment.text);
        if cleaned.is_empty() {
            continue;
        }
        let leading_ws = segment.text.starts_with(char::is_whitespace);
        let trailing_ws = segment.text.ends_with(char::is_whitespace);
        let bbox = segment.bbox;
        let vertical = is_vertical(&cleaned, &bbox);

        match current.as_mut() {
            Some(line) if !vertical && !last_vertical && same_line(&last_run, &bbox) => {
                let gap = bbox.x0 - last_run.x1;
                // The word space belongs to the smaller text; the taller run
                // would swallow real gaps (LINE_TOLERANCE still uses max).
                let small = height(&last_run).min(height(&bbox));
                if line.trailing_space || leading_ws || gap > SPACE_GAP * small {
                    line.text.push(' ');
                }
                line.text.push_str(&cleaned);
                line.bbox = Rect {
                    x0: line.bbox.x0.min(bbox.x0),
                    y0: line.bbox.y0.min(bbox.y0),
                    x1: line.bbox.x1.max(bbox.x1),
                    y1: line.bbox.y1.max(bbox.y1),
                };
                line.trailing_space = trailing_ws;
            }
            _ => {
                flush(current.take(), &mut blocks);
                current = Some(Line {
                    text: cleaned,
                    bbox,
                    trailing_space: trailing_ws,
                });
            }
        }
        last_run = bbox;
        last_vertical = vertical;
    }
    flush(current, &mut blocks);
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, x0: f32, x1: f32, y0: f32) -> Segment {
        Segment {
            text: text.into(),
            bbox: Rect {
                x0,
                y0,
                x1,
                y1: y0 + 11.0,
            },
        }
    }

    #[test]
    fn word_gap_is_judged_by_the_smaller_run_when_heights_differ() {
        // A 25 pt inline glyph run followed by 11.5 pt body text, with a
        // genuine 3 pt word gap. The gap belongs to the smaller text.
        let big = Segment {
            text: "SUM".into(),
            bbox: Rect {
                x0: 100.0,
                y0: 100.0,
                x1: 120.0,
                y1: 125.0,
            },
        };
        let small = Segment {
            text: "and".into(),
            bbox: Rect {
                x0: 123.0,
                y0: 106.0,
                x1: 140.0,
                y1: 117.5,
            },
        };
        let b = merge_segments(vec![big, small]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "SUM and");
    }

    #[test]
    fn ligature_split_mid_word_is_joined_without_space() {
        let b = merge_segments(vec![
            seg("fi", 10.0, 16.0, 100.0),
            seg("eld", 16.3, 30.0, 100.0),
        ]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "field");
    }

    #[test]
    fn real_word_gap_gets_one_space() {
        let b = merge_segments(vec![
            seg("hello", 10.0, 40.0, 100.0),
            seg("world", 42.8, 70.0, 100.0),
        ]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "hello world");
    }

    #[test]
    fn abutting_punctuation_has_no_space() {
        let b = merge_segments(vec![
            seg("2025", 10.0, 30.0, 100.0),
            seg(",", 30.31, 33.0, 100.0),
        ]);
        assert_eq!(b[0].text, "2025,");
    }

    #[test]
    fn vertical_change_starts_a_new_block() {
        let b = merge_segments(vec![
            seg("one", 10.0, 30.0, 100.0),
            seg("two", 10.0, 30.0, 86.0),
        ]);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].id, b[1].id), (0, 1));
        assert_eq!((b[0].text.as_str(), b[1].text.as_str()), ("one", "two"));
    }

    #[test]
    fn overlapping_runs_have_no_space() {
        let b = merge_segments(vec![
            seg("ab", 10.0, 20.0, 100.0),
            seg("cd", 19.5, 30.0, 100.0),
        ]);
        assert_eq!(b[0].text, "abcd");
    }

    #[test]
    fn empty_input_yields_no_blocks() {
        assert!(merge_segments(vec![]).is_empty());
    }

    #[test]
    fn single_segment_is_unchanged() {
        let b = merge_segments(vec![seg("solo", 10.0, 30.0, 100.0)]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "solo");
        assert_eq!(b[0].kind, BlockKind::Line);
    }

    #[test]
    fn merged_bbox_is_the_union() {
        let b = merge_segments(vec![
            seg("a", 10.0, 20.0, 100.0),
            seg("b", 20.2, 35.0, 101.0),
        ]);
        let r = b[0].bbox.unwrap();
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (10.0, 100.0, 35.0, 112.0));
    }

    #[test]
    fn explicit_whitespace_in_a_run_is_honoured() {
        let b = merge_segments(vec![
            seg("a ", 10.0, 20.0, 100.0),
            seg("b", 20.0, 30.0, 100.0),
        ]);
        assert_eq!(b[0].text, "a b");
    }

    #[test]
    fn rotated_words_are_not_welded_together() {
        // Two rotated header words: tall, narrow boxes side by side.
        let tall = |t: &str, x0: f32| Segment {
            text: t.into(),
            bbox: Rect {
                x0,
                y0: 114.0,
                x1: x0 + 5.0,
                y1: 147.0,
            },
        };
        let b = merge_segments(vec![tall("Application", 61.0), tall("Domain", 66.4)]);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn a_run_that_jumps_backwards_starts_a_new_block() {
        let b = merge_segments(vec![
            seg("right", 200.0, 230.0, 100.0),
            seg("left", 10.0, 40.0, 100.0),
        ]);
        assert_eq!(b.len(), 2);
    }
}
