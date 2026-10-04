# docparse — Design

**Date:** 2026-10-04
**Status:** Approved (sections 1–2 reviewed; sections 3–8 pending review)

## Purpose

A Rust library and CLI that turns PDF, DOCX and image files into
structure-preserving JSONL, with a hard memory ceiling and no network
access.

This replaces the extraction core of the existing Python
`offline_llm_based_document_parsing` project. That project's slowness
is architectural, not a consequence of Python: it captions every image
with BLIP, calls Mistral once per 3,500 characters to build a
knowledge graph, rewrites the entire FAISS index and metadata JSON on
every upload, and imports torch solely to check for CUDA. The design
below drops all four.

### Target hardware

4 GB RAM, 2–4 cores, HDD or slow SSD. This floor drives two decisions
that would otherwise look arbitrary: the pull-based page iterator
(section 2) and parallelism across files rather than within them
(section 2).

## Section 1 — Scope

In scope:

- PDF text extraction preserving page, block and bounding-box structure
- DOCX extraction (paragraphs, headings, list items, table cells)
- Image input, OCR only
- OCR as an explicit opt-in fallback for pages with no extractable text
- Structure-aware chunking with honest page attribution
- Streaming, deterministic JSONL output
- A CLI that processes a file or a directory tree

Non-goals, each corresponding to something the current project does:

- No embeddings, no vector index, no retrieval
- No LLM calls, therefore no knowledge-graph extraction
- No image captioning, therefore no BLIP and no torch
- No HTTP server, no web UI
- No network access at runtime. This is an invariant, not a flag: no
  HTTP client is linked into the binary, so "offline" is a property
  rather than a promise.

The existing Python application stays in the repository untouched, as
a reference implementation and a benchmarking baseline. Its removal is
a separate decision to be made after `docparse` works.

## Section 2 — Architecture

A two-crate Cargo workspace. The split that matters at this size is
library versus binary; anything finer is ceremony.

```
docparse/
  Cargo.toml          # workspace
  docparse/           # library
    src/
      model.rs        # Document, Page, Block, Chunk, BlockKind
      source.rs       # trait Source
      pdf.rs          # feature = "pdf"    -> pdfium
      docx.rs         # feature = "docx"   -> zip + quick-xml
      image.rs        # feature = "image"
      ocr.rs          # feature = "ocr"    -> tesseract
      chunk.rs        # streaming chunker
      normalize.rs    # per-block text cleanup
      error.rs
  docparse-cli/       # binary
```

Backends sit behind Cargo features, all default-on except `ocr`. A
consumer who needs only DOCX gets a binary that links neither PDFium
nor Tesseract.

The central abstraction:

```rust
pub trait Source {
    fn next_page(&mut self) -> Option<Result<Page, Error>>;
}
```

Every format reduces to "yields pages, one at a time." The chunker,
normalizer, CLI and writer consume `Source` and remain ignorant of the
input format. DOCX has no inherent pages, so it synthesizes them at
section breaks and records that fact in the output rather than
pretending otherwise.

**Why pull-based rather than parsing into a returned `Document`:**
returning a whole document forces peak memory to scale with input
size. It is the structural reason the current code cannot process a
large PDF on a small machine. Pulling one page at a time makes the
memory ceiling a property of the architecture rather than a tuning
parameter.

**Parallelism.** `rayon` across files at the top level, since files
are independent. Within a single file, pages are processed
sequentially, with one exception: OCR is CPU-bound enough to justify a
bounded `par_iter` over a small window of pages. Decoding a PDF page
is fast; seeking for it on a spinning disk is not, so parallelizing
page reads within one file would mostly produce disk thrash. Worker
count defaults to `min(cores, 4)`, not all cores.

## Section 3 — Data model

```rust
pub struct Page {
    pub number: u32,            // 1-based, true source page number
    pub width: f32,             // points; 0.0 when not applicable
    pub height: f32,
    pub origin: PageOrigin,
    pub blocks: Vec<Block>,
}

pub struct Block {
    pub id: u32,                // sequential within the page
    pub bbox: Option<Rect>,     // None when the format has no geometry
    pub kind: BlockKind,
    pub text: String,
}

pub enum BlockKind {
    Paragraph,
    Heading(u8),
    ListItem,
    TableCell { row: u32, col: u32 },
    Caption,
    Other,
}

pub enum PageOrigin {
    Native,                     // text extracted directly
    Synthesized,                // DOCX section break, no real page
    Ocr { confidence: f32 },
}

pub struct Chunk {
    pub id: u64,
    pub source: String,
    pub pages: (u32, u32),          // inclusive span, real page numbers
    pub blocks: Vec<(u32, u32)>,    // (page, block_id) provenance
    pub text: String,
    pub origin: PageOrigin,
}
```

`bbox` is `Option<Rect>` because DOCX genuinely has no geometry.
Fabricating coordinates there would be the same category of error as
the existing code's `md5 -> random vector` embedding fallback:
producing plausible-looking output in place of an honest absence.

`PageOrigin` exists so a consumer can tell extracted text from OCR
guesses. The current code concatenates a BLIP caption with OCR output
into one undifferentiated string.

**Output format.** One JSON object per line. Default emits chunks;
`--emit blocks` emits raw blocks instead, for callers that want to do
their own chunking.

## Section 4 — Normalization and chunking

This is the part the current implementation gets wrong, and the main
source of its retrieval quality problems.

**Normalization is per-block only.** Within a block: collapse runs of
spaces and tabs, map smart quotes and dashes to ASCII, strip control
characters. Block and page boundaries are never crossed. The current
`normalize_text` applies `re.sub(r"\s+", " ")` to the entire document
before chunking, flattening it to a single line and destroying all
layout and page structure that PyMuPDF was used to obtain.

**Chunk assembly:**

- Accumulate whole sentences from consecutive blocks until the target
  size is reached. A sentence is never split.
- A single sentence longer than the configured maximum is hard-split,
  and the resulting chunks are marked as such.
- Overlap is measured in sentences (default: 1), not characters.
- Chunks do not span pages by default; `--cross-page` permits it.
  Because of this, page attribution is exact.

The current code chunks on raw character offsets (`range(0, len(text),
step)`) and then attributes pages with `min(idx, len(base_meta) - 1)`,
which is not a page lookup but a clamp — every chunk past the first
few is attributed to the last page.

**Determinism:** with `--jobs 1`, identical input bytes produce
byte-identical JSONL. With more workers, each file's lines remain
contiguous and ordered, but files appear in completion order. Restoring
input order would require buffering the whole corpus, which contradicts
the memory ceiling.

## Section 5 — Error handling

- **Per-file isolation.** A corrupt or unreadable file produces an
  error record; the batch continues.
- **No silent fallbacks.** If OCR is required but unavailable, that is
  an error, not a placeholder string. The existing
  `_hash_fallback` — returning a seeded random vector when the
  embedding model is missing — is the anti-pattern this rule exists to
  forbid: it degrades to meaningless output while reporting success.
- `Error` is a `thiserror` enum. Every variant carries the file path,
  and page-level variants carry the page number.
- Exit codes: `0` all files succeeded, `1` all failed, `2` partial
  failure.

## Section 6 — Memory ceiling

Target: peak RSS below 64 MB for any single input file.

**Measured caveat, recorded 2026-10-04.** The original wording of this
section said "regardless of that file's size." That is true of page
*content* but false of total memory for large PDFs, so it has been
corrected rather than defended. `docparse`'s own allocation is genuinely
flat — 0.06 MB of peak heap for a synthetic 5,000-page source producing
90,000 chunks, and unchanged between 100 and 2,000 pages. But PDFium
retains roughly 3.4 KB per page of document index (page tree, xref) when
it opens a file, which it does not release. Measured peak RSS, debug
build: 9.3 MB at 2 pages, 22.5 MB at 3,000, 50.7 MB at 12,000. By
extrapolation a PDF of roughly 16,000-17,000 simple pages would cross
64 MB. Reaching constant memory on documents that large would require
splitting the PDF before parsing, which is out of scope.

Enforced structurally:

- One page in flight per worker.
- The chunker holds at most one chunk plus its overlap tail.
- The writer streams; no output is buffered to completion.
- Rendered page images for OCR are the one large allocation. Render at
  a configurable DPI (default 150; the current code hardcodes 180) and
  drop the pixmap before processing OCR output.

A test asserts this ceiling rather than leaving it as an aspiration.

## Section 7 — Testing

- **Golden corpus:** small fixture files with expected JSONL committed
  alongside them.
- **Property test:** concatenating a document's chunks, minus overlap,
  reproduces every block's text — no text lost, none duplicated beyond
  the declared overlap.
- **Determinism test:** two `--jobs 1` runs over the same input produce
  identical bytes.
- **Memory test:** a large synthetic multi-page source, asserting peak
  *heap* (via a tracking global allocator, not RSS) stays under the
  ceiling. It therefore excludes the binary, thread stacks and anything
  PDFium allocates internally.
- **Error isolation test:** a corrupt file inside a batch; the batch
  completes and the exit code is 2.
- **Benchmarks** (criterion): pages/second and MB/second, to be
  compared against the existing Python pipeline on the same corpus.

## Section 8 — Milestones

1. Workspace, `model.rs`, `error.rs`, `Source` trait. No backends.
2. DOCX backend — pure Rust, no C dependency, simplest first.
3. Normalizer and streaming chunker, with the property test.
4. CLI with JSONL output and per-file error isolation.
5. PDF backend via PDFium bindings.
6. `rayon` parallelism across files, plus the memory-ceiling test.
7. Image and OCR backends behind the `ocr` feature.
8. Benchmarks against the Python baseline.

DOCX precedes PDF deliberately: it exercises the entire pipeline —
model, chunker, CLI, output — without introducing a C dependency or a
binding layer, so the first working end-to-end slice has no external
build requirements.

## Known limitations

- The binary depends on PDFium as a shared library, and on Tesseract
  when the `ocr` feature is enabled. The deployment story is far
  better than a 1.4 GB virtualenv, but it is not zero-dependency.
- Table reconstruction is limited to what the backends report.
  Recovering table structure from PDF ruling lines is out of scope.
- Reading order for multi-column PDF layouts follows what PDFium
  reports and is not independently corrected.
