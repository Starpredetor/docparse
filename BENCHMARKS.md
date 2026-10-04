# Benchmarks

Only measurements that were actually taken are recorded here, each with its
conditions. Read the caveats: several of these numbers are misleading on their
own. **No comparison against any other tool has been run.**

The project's stated target is 4 GB RAM, 2-4 cores, a spinning disk, and a
64 MB working-memory budget for extraction. Most measurements below were taken
on a much bigger machine than that, and say so.

## Real corpus run

Release build. Measured by the project controller.

| | |
|---|---|
| Input | 20 research PDFs, 94 MB, 424 pages |
| Output | 2,976 chunks, 2,144,133 characters (about 536k tokens at chars/4), 4.0 MB JSONL |
| Time | about 9 s at `--jobs 4` (two runs: 9.4 s and 9.1 s; 20-core machine, warm cache) |
| Result | exit 0, zero errors |
| Chunk stats | average chunk 720 characters, 0 hard-splits, every origin `native` |

Command: `docparse corpus/ -o out.jsonl --jobs 4`. These figures were re-measured
after `merge_segments` (Task 10) and the word-gap fix, which merge PDFium style-runs
into single `Line` blocks; earlier figures (2,860 chunks, 7.0 MB) predate it. This is one run, not a
statistical sample. The token figure is an estimate (characters / 4), not a
tokenizer count.

## Memory

### Chunker (library level)

Measured with the `peak_alloc` tracking allocator over a synthetic page source,
so no PDF parsing is included.

- 5,000 pages / 90,000 chunks: **0.06 MB peak heap**.
- Flat: 0.07 MB at both 100 and 2,000 pages.
- Both assertions were verified to fail when the chunker is made to retain data
  (they then report 66.1 MB and 28.0 MB), so they do detect a regression.

### PDF, whole process

Peak RSS, **debug build**.

| Pages | Peak RSS |
|---|---|
| 2 | 9.3 MB |
| 3,000 | 22.5 MB |
| 12,000 | 50.7 MB |

About 3.4 KB per page, linear. This growth is PDFium's own page tree and xref
structures; `docparse` does not retain pages. Extrapolating the line, roughly
16-17k simple pages would cross 64 MB. That is an extrapolation, not a
measurement. No release-build PDF memory figures were taken.

### OCR

Release build, A4 scans at 150 DPI, peak working set.

| Configuration | In-process | Including tesseract processes |
|---|---|---|
| `--ocr --jobs 1` | about 52 MB | about 52 MB + a separate ~62 MB tesseract process |
| `--ocr --jobs 4` | about 152 MB | about 213 MB |

At `--jobs 1` memory is flat from 8 to 24 pages. The page bitmap scales with
the square of `--ocr-dpi`.

**The 64 MB budget does not hold under OCR, at any job count.** Even one
worker's in-process figure is near the budget before counting tesseract.

## Parallel speedup

| Workload | `--jobs 1` | `--jobs 4` | Speedup |
|---|---|---|---|
| 120 synthetic DOCX files, 4.8 MB total, release build | about 278 ms | about 120 ms | about 2.3x |

**Do not read this as a general result.** Conditions:

- Measured on a **20-core machine with a warm file cache**, on tiny files.
- **This says nothing about the project's target hardware** (2-4 cores, HDD).
  On 2 cores the ceiling is about 2x. On a spinning disk the disk very likely
  dominates and the speedup may be much smaller. Neither was measured.
- 278 ms is short enough that process startup is a non-trivial share of it.
- Synthetic DOCX only; PDF parallel speedup was not separately measured here
  (the real-corpus run above used `--jobs 4` only).

Memory cost of parallelism: `--jobs N` buffers one document's JSONL per worker
before writing it out, so a single huge document costs that buffer times N.

## Microbenchmarks

Criterion (`cargo bench -p docparse --features pdf`), release profile. Source:
`docparse/docparse/benches/throughput.rs`. These are tiny in-process loops, not
corpus throughput, and should not be combined with the numbers above. Intervals
are criterion's `[low estimate high]` over 100 samples, taken on the
project development machine (CPU details not recorded for this run).

| Benchmark | Time | Throughput |
|---|---|---|
| `chunker/500_synthetic_pages` | [9.1661 ms **9.4793 ms** 9.8019 ms] | [51.011 **52.747** 54.549] Kelem/s (pages) |
| `pdf/two_page_end_to_end` | [1.0745 ms **1.1029 ms** 1.1297 ms] | [1.7704 **1.8134** 1.8613] Kelem/s (pages) |

Notes:
- The chunker input is 500 synthetic pages of 8 blocks each (about 3,200
  characters per page) with no disk or PDF parsing involved.
- The PDF benchmark opens a 2-page, few-line fixture end to end (open PDFium
  document, extract, chunk), so it is dominated by fixed per-document cost, not
  by text volume. Its per-page figure does not predict throughput on real
  papers.
- Both benchmarks wrap results in `black_box`; without it the optimizer can
  delete the work and report an impossibly fast time.

## Not measured

- Any comparison against any other tool, library or pipeline.
- Cold-cache or HDD numbers.
- Behaviour on 2-4 cores.
- Release-build PDF memory (only the debug-build figures above exist).
- Memory for large DOCX files (DOCX is parsed whole, not streamed).
- Multi-column or complex-layout PDFs: extraction order and correctness there
  were not evaluated.
- OCR quality, except on synthetic fixtures.
