# docparse

A Rust library and command-line tool that turns PDF, DOCX and image files into
structure-preserving JSONL. Each output record says which document, which real
page and which blocks its text came from, and whether that text was extracted
or guessed by OCR.

- **No models.** No LLM, no embeddings, no retrieval, no server.
- **No network access.** No HTTP client crate is linked in, so "offline" is a
  property of the build, not a promise. Check it: `cargo tree` shows none.
- **Bounded memory for PDFs.** Pages stream through the chunker one at a time.
  By default the chunker retains nothing across pages; with `--cross-page` it
  holds at most a partial chunk plus the overlap tail, never the document.
  See [BENCHMARKS.md](BENCHMARKS.md)
  for what was and was not measured, and the DOCX and OCR caveats below.

It is positioned as an **extraction layer to sit in front of a retrieval
tool**, not as a general-purpose document parser. ML-driven parsers that do
layout analysis, table recovery and reading-order correction exist and are far
more capable at those jobs; `docparse` does none of them (see
[What it deliberately does not do](#what-it-deliberately-does-not-do)).

## Quickstart

```bash
cd docparse
cargo build --release --workspace

# PDFium must sit beside the binary (see Deployment). On Windows:
#   copy pdfium.dll target\release\

./target/release/docparse ../corpus/ -o out.jsonl --jobs 4
```

Directories are walked recursively; `.docx`, `.pdf`, `.png`, `.jpg`, `.jpeg`,
`.webp`, `.bmp`, `.tif`, `.tiff` are picked up. Without `-o`, JSONL goes to
stdout and diagnostics to stderr.

One output line (a chunk, from a two-page test PDF):

```json
{"id":0,"source":"two_pages.pdf","page_start":1,"page_end":1,"blocks":[{"page":1,"block":0},{"page":1,"block":1}],"text":"First page first line. First page second line.","origin":"native","hard_split":false}
```

`--emit blocks` writes the raw blocks instead, for callers who want to chunk
themselves:

```json
{"bbox":{"x0":72.9,"x1":163.96,"y0":739.7,"y1":750.0},"id":0,"kind":"line","origin":"native","page":1,"source":"two_pages.pdf","text":"First page first line."}
```

(Coordinates shortened here; real output carries full `f32` precision.)

### Options

| Flag | Default | Meaning |
|---|---|---|
| `-o, --output` | stdout | Write JSONL to a file |
| `--chunk` | 800 | Target chunk size in characters |
| `--max-chunk` | 1200 | Hard ceiling in characters |
| `--overlap` | 1 | Sentences repeated between consecutive chunks |
| `--cross-page` | off | Allow a chunk to span pages (off keeps page attribution exact) |
| `--emit` | `chunks` | `chunks` or `blocks` |
| `-j, --jobs` | min(cores, 4) | Worker threads |
| `--ocr` | off | OCR PDF pages with no extractable text (needs the `ocr` feature) |
| `--ocr-dpi` | 150 | Render DPI for OCR |

**Exit codes:** `0` every file succeeded; `2` some failed, some succeeded; `1`
every file failed or nothing was found. A failing file is reported on stderr
and does not stop the batch. A file that fails partway may leave its earlier
records in the output.

**Ordering:** with `--jobs 1`, output is byte-identical across runs and in input
order. With more jobs, each file's lines stay contiguous and in order, but
files appear in completion order. Restoring input order would mean holding the
whole corpus's output in memory, so it is deliberately not done.

## Output schema

This is the tool's contract. Records are one JSON object per line.

### Chunk record (`--emit chunks`, the default)

| Field | Type | Meaning |
|---|---|---|
| `id` | integer | Sequential per source file, starting at 0 |
| `source` | string | The input file's name (not its path) |
| `page_start`, `page_end` | integer | Inclusive span of **real** 1-based page numbers the text came from. Equal unless `--cross-page` is set |
| `blocks` | array of `{page, block}` | Provenance: every block that contributed text. `page` is the real page number, `block` the block `id` within that page |
| `text` | string | The chunk text, whitespace-normalised. Typographic characters are mapped to ASCII; control characters are dropped |
| `origin` | `"native"`, `"synthesized"` or `"ocr"` | Where the text came from (below) |
| `confidence` | number | **Present only when `origin` is `"ocr"`.** Mean word confidence from tesseract, 0.0 to 1.0 |
| `hard_split` | boolean | `true` when a single sentence exceeded `--max-chunk` and had to be cut mid-sentence. Otherwise a chunk never ends mid-sentence |

**`origin`**
- `native`: text extracted directly from the document (PDF text layer).
- `synthesized`: the format has no real pages; the page boundary was invented.
  This is DOCX, where one "page" is one section (a `sectPr` marks its end). The
  page numbers are therefore section numbers, not rendered pages.
- `ocr`: text is an OCR guess, with its `confidence`.

A chunk is only as trustworthy as its weakest sentence. If a chunk mixes
origins, its `origin` is the weakest of them (`ocr`, with the lowest
confidence, outranks `synthesized`, which outranks `native`), so a partly-OCR
chunk can never claim to be extracted text. OCR output is never merged into
extracted text without being labelled.

**Overlap:** with `--overlap N`, the last N sentences of one chunk repeat at the
start of the next. Overlap is dropped when a chunk consumes no more sentences
than that, because re-queueing them would make no forward progress.

**Sentence splitting** is deliberately simple: it breaks on `.`, `!`, `?`
followed by whitespace, with no abbreviation dictionary and no locale rules.
"Dr. Smith" will be split after "Dr.".

### Block record (`--emit blocks`)

| Field | Type | Meaning |
|---|---|---|
| `id` | integer | Sequential within its page, starting at 0 |
| `page` | integer | Real 1-based page number |
| `source` | string | Input file name |
| `kind` | string | `line`, `paragraph`, `heading`, `list_item`, `table_cell`, `caption`, `other` |
| `level` | integer | Only for `heading` |
| `row`, `col` | integer | Only for `table_cell` |
| `bbox` | `{x0,y0,x1,y1}` | PDF points, origin at the page's bottom-left. **Absent** (not zero, not null) when the format has no geometry, as with DOCX |
| `text` | string | Normalised block text |
| `origin`, `confidence` | | As for chunks, taken from the page |

Which extractor produces which `kind`:
- **PDF** produces `line` only, with a `bbox`. A line is the visual line: PDFium's style-split runs on one baseline are merged, with a space only where the gap is wide (see `merge_segments`). A line-ending soft hyphen is not rejoined across lines.
- **DOCX** produces `paragraph`, `heading`, `list_item` and `table_cell`, never
  with a `bbox`.
- **OCR** (image inputs, and PDF pages OCR'd under `--ocr`) produces
  `kind: "other"` blocks with `origin: "ocr"`, one page per image. So the PDF
  extractor emits `other` whenever `--ocr` fires on a page.
- `caption` exists in the type but no extractor ever emits it.

## Measured numbers

All in [BENCHMARKS.md](BENCHMARKS.md), with the conditions and caveats for each.
Headline, release build: 20 research PDFs (94 MB, 424 pages) produced 2,976
chunks in about 9 seconds at `--jobs 4` (20-core machine, warm cache), with
zero errors. The chunker held a
0.06 MB peak heap over 5,000 synthetic pages. OCR does **not** fit the 64 MB
budget. BENCHMARKS.md also lists what has not been measured; please read it
before drawing conclusions from the speed figures. No comparison against any
other tool has been run.

## What it deliberately does not do

These are decisions, not gaps waiting to be filled.

- **No tables.** PDF tables are not detected. Text in a PDF table comes out as
  `line` blocks in whatever order PDFium reports it, with the structure lost.
  DOCX table cells are tagged `table_cell` with `row`/`col`, but they are not
  assembled into tables, and the chunker treats them as ordinary text.
- **No multi-column reading-order correction.** Lines are reported in the order
  PDFium yields them. On multi-column pages (the corpus is academic papers)
  that order may interleave columns. No multi-column PDF was evaluated for
  correctness; BENCHMARKS.md says so.
- **No paragraph reconstruction from PDF.** PDFium gives text segments, not
  paragraphs. They are reported as `line`, because that is what they are.
  Calling them `paragraph` would imply a reconstruction that was never done.
- **No cleanup of PDF text artefacts.** Whatever PDFium returns is what you get,
  after whitespace normalisation. In the real-corpus output you will see
  the occasional broken word (`competi tion`, once) and line-end soft-hyphen
  splits (`preci sion` x11, `con ditions` x7: about 18 across the 20-PDF
  corpus). These occur where a word was hyphenated across a line break;
  cross-line de-hyphenation is not attempted.
- **No embeddings, retrieval, LLM, or server.** It writes JSONL and exits.
- **No image captioning or description.** An image is OCR'd or it is an error;
  nothing is generated about its content.
- **No silent fallbacks.** An unreadable image is an error, not empty text. A
  page with no text yields no blocks rather than a placeholder. A DOCX without
  `word/document.xml` is a malformed-file error, not an empty success. Image
  input in a build without the `ocr` feature is an error, not a skip.

### Known limits that are not "by design"

- **DOCX is parsed whole.** A DOCX file's `word/document.xml` is parsed into
  all its pages up front, so memory for DOCX scales with the document. Only PDF
  streams page by page. Large-DOCX memory has not been measured.
- **`--jobs N` buffers one document's JSONL per worker**, so a single huge
  document costs that buffer times N.
- **OCR breaks the memory budget** (see below).

## Deployment requirements

**PDFium.** PDF support loads PDFium dynamically at runtime. `pdfium.dll`
(Windows) or `libpdfium` (elsewhere) must sit beside the binary or be on the
library search path. The library is not bundled.

**The `pdfium_NNNN` Cargo feature must match or predate the deployed library.**
`docparse/docparse/Cargo.toml` pins `pdfium_7763`. The DLL used in development
is build 7802; the newer `pdfium_latest` (7881) feature fails at runtime with a
`GetProcAddress` error 127 against it, because it asks for symbols the older
DLL lacks. If you deploy a different PDFium build, set the feature to that
build or an earlier one.

**Tesseract (only for the `ocr` feature).** OCR shells out to a `tesseract`
executable; nothing is linked. It is resolved in this order:
1. the `DOCPARSE_TESSERACT` environment variable (full path to the executable)
2. `tesseract` on `PATH`
3. known install locations (Windows: `Program Files` and `Program Files (x86)`
   under `Tesseract-OCR`; Unix: `/usr/bin` and `/usr/local/bin`)

If none is found the error names every place that was tried.

**Memory under OCR.** The 64 MB budget does **not** hold under OCR, at any job
count. Measured on A4 scans at 150 DPI: about 52 MB in-process plus a separate
~62 MB tesseract process at `--jobs 1`; about 152 MB in-process and ~213 MB in
total at `--jobs 4`. The page bitmap grows with the square of `--ocr-dpi`.
Budget roughly 50 MB plus a comparable tesseract process per worker.

## Cargo features

On the `docparse` library crate (the CLI forwards `default` and `ocr`):

| Feature | Default | Enables |
|---|---|---|
| `pdf` | on | PDF extraction via PDFium (`pdfium-render`) |
| `docx` | on | DOCX extraction (`zip` + `quick-xml`, pure Rust) |
| `image` | on | Decoding/sizing input images (PNG, JPEG, BMP, WebP, TIFF) |
| `ocr` | **off** | OCR of images and of text-less PDF pages, via the `tesseract` executable. Implies `image` |

Backends you turn off are not linked. `cargo build --no-default-features`
builds the chunker and data model alone.

```bash
cargo build --release --workspace                  # pdf + docx + image
cargo build --release --workspace --features docparse-cli/ocr
```

## Tests and benchmarks

```bash
cargo test --workspace --features docparse/pdf
cargo test --workspace --features docparse/pdf,docparse/ocr -- --test-threads=1   # needs tesseract
cargo bench -p docparse --features pdf
```

The tests include a memory-ceiling test, a page-attribution test, a
"no chunk ends mid-sentence" property test, and a "no text lost" property
test. Test fixtures were generated with Python (PyMuPDF, Pillow); Python is
not needed to build or run `docparse`.

## Layout

- `docparse/` the Cargo workspace
  - `docparse/docparse/` the library
  - `docparse/docparse-cli/` the `docparse` binary
- `docs/superpowers/` design spec and implementation plan
- `corpus/` local PDFs used for measurements (gitignored)
