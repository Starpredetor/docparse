# docparse Stage 1: Incremental Index and Tracking — Design

**Date:** 2026-10-11
**Status:** Awaiting review
**Predecessor spec:** `2026-10-04-docparse-design.md` (the extraction core, complete — 88/95 tests)

## Intended outcome

Make `docparse` an incremental indexer for a document directory, so that a
corpus far larger than any context window can be tracked, queried and fed to
an LLM in selected pieces.

`docparse index .` walks a directory, parses what has changed since the last
run, and stores the results in `.docparse/index.db`. A second run over an
unchanged directory does almost no work.

**Who it is for:** initially the author; ultimately non-technical users who
install a packaged bundle and never type a command. That second audience is
not served by this stage, but it constrains it (see Forward compatibility).

**Success criteria:**

1. Re-running `index` over an unchanged directory re-parses **zero** files.
2. Modifying one file in a 20-document corpus re-parses **exactly one**.
3. Renaming, moving or duplicating a file re-parses **zero** — content, not
   path, is the identity.
4. An interrupted run leaves a consistent index with some files stale, never
   a corrupt one.
5. `docparse status` reports what is new, changed, deleted, failed and
   unchanged without parsing anything.

## Decisions already settled in discussion

Recorded here so they are not relitigated during implementation.

- **SQLite with FTS5, not JSONL, as the index.** FTS5 supplies BM25 scoring,
  phrase and prefix queries, snippets and highlighting — all tested, none of
  which is worth hand-writing. The later structural queries ("which papers
  define term X inside a Methods section") are relational joins, which is
  SQL's home ground. And transactions make a half-written index impossible,
  which matters for a tool whose job is tracking.
- **JSONL changes role from storage to export.** It remains the interchange
  format — `docparse export`, and later search results — because feeding an
  LLM and inspecting what the tool found are both output concerns.
- **Content-addressed file identity.** A `files` row is keyed by content hash;
  paths point at it. Renames cost a path update, and duplicates are stored
  once. This is not hypothetical: the corpus this was developed against held
  all 20 PDFs copied into two directories, 94 MB where 47 would do.
- **`.docparse/` lives in the indexed directory**, so the index travels with
  the documents. `--index-dir` relocates it for read-only, shared or
  cloud-synced directories.
- **Git-style subcommands.** `docparse index`, `docparse status`,
  `docparse export`. Bare `docparse <files>` keeps streaming JSONL to stdout,
  because that is pipe-friendly and already documented.
- **OCR stays default-off**, per the extraction spec.

## Scope

### In scope

- `docparse index [PATH]` — walk, detect changes, parse only what changed
- `docparse status [PATH]` — report state without parsing
- `docparse export [PATH]` — emit JSONL from the index
- The SQLite schema, including the FTS5 table (see Section 3 for why now)
- Three-tier change detection: path, then size/mtime, then content hash
- Rename, move and duplicate handling via content hash
- Deletion pruning
- Extraction-version and config-fingerprint invalidation
- Per-file atomicity
- Failure recording, so a corrupt file is not re-attempted every run

### Explicit non-goals

Each of these is deliberately excluded, not forgotten:

- **No search command.** `chunks_fts` is populated but unqueried. Stage 2.
- **No MCP server.** Stage 3.
- **No structural extraction** — no term index, section hierarchy, captions
  or citations. Stage 4, and the schema leaves room for it.
- **No embeddings, at any stage.** The project's position is that it needs no
  models.
- **No document history.** The index records the current state of each file,
  not its revisions. "What changed between two versions of this paper" is a
  different and much larger feature.
- **No file watching or daemon.** `index` is a command that runs and exits.
- **No concurrent writers.** A second indexer against the same index is
  detected and refused, not merged.
- **No new input formats.** `.txt` and `.md` support is a parser feature,
  addable later without touching the index, and is recorded as a follow-up
  rather than bundled here.

## Section 1 — Architecture

Three new modules in the library, and one new module in the CLI:

```
docparse/
  docparse/src/
    index/
      mod.rs        # public API: Index::open, index_dir, status, export
      schema.rs     # DDL, schema_version, migrations
      detect.rs     # change detection as PURE functions
      store.rs      # the SQLite read/write layer
  docparse-cli/src/
    main.rs         # subcommand dispatch
    cmd_index.rs
    cmd_status.rs
    cmd_export.rs
```

`detect.rs` holds no I/O. It decides, from facts about a file and what the
index already knows, which of a small set of actions applies. That makes the
heart of the feature unit-testable without a filesystem or a database, which
is where the mutation testing will aim.

The existing extraction path is untouched. `index_dir` calls the same
`open_source` dispatch and `Chunker` the streaming CLI already uses; it only
changes where the chunks go.

## Section 2 — Change detection

A pure function over three inputs — the path as walked, the `paths` row if
any, and (lazily) the content hash:

```rust
pub enum Action {
    /// Path known, size and mtime unchanged. Do not read the file.
    Unchanged,
    /// Size or mtime moved but content hash is identical. Refresh mtime only.
    TouchOnly,
    /// Content differs, or the file is new content. Parse it.
    Parse,
    /// This content is already indexed under another path. Link, do not parse.
    LinkExisting { file_id: i64 },
    /// Previously failed and unchanged. Skip unless --retry-failed.
    SkipFailed,
}
```

Tiers, cheapest first:

1. **Path known, `byte_size` and `mtime_ns` both match** → `Unchanged`. No
   read, no hash. This is the common case and the reason a no-op run is fast.
2. **Path known, size or mtime differs** → hash the file. Hash equal to the
   stored one means a touched-but-unmodified file (common with sync clients
   and checkouts) → `TouchOnly`. Otherwise → `Parse`.
3. **Path unknown** → hash the file. If that hash already exists in `files`,
   this is a rename, move or copy → `LinkExisting`. Otherwise → `Parse`.

Paths present in the index but absent from the walk are deleted: remove the
`paths` row, and when a `files` row has no paths left, remove it, cascading
its chunks.

**mtime is a hint, never authority.** Coarse-granularity filesystems, network
shares and archive extraction all produce misleading timestamps, so the
content hash decides whenever the fast path does not hold.

**Hash: BLAKE3.** Roughly an order of magnitude faster than SHA-256 on large
inputs, which matters when tier 2 or 3 must read 94 MB. Cryptographic
strength is not required here — only collision resistance for content
identity — and BLAKE3 provides considerably more than enough.

## Section 3 — Schema

```sql
PRAGMA journal_mode = WAL;      -- crash resilience, reader/writer separation
PRAGMA foreign_keys = ON;

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- keys: schema_version, extraction_version, config_fingerprint,
--       docparse_version, root_path, created_at, updated_at

-- One row per distinct CONTENT. Identity is the hash, not the path.
CREATE TABLE files (
    id            INTEGER PRIMARY KEY,
    content_hash  TEXT    NOT NULL UNIQUE,
    byte_size     INTEGER NOT NULL,
    page_count    INTEGER,              -- NULL when parsing failed
    chunk_count   INTEGER NOT NULL DEFAULT 0,
    status        TEXT    NOT NULL,     -- 'indexed' | 'failed'
    error         TEXT,                 -- set iff status='failed'
    indexed_at    TEXT    NOT NULL
);

-- One row per PATH. Several paths may share one files row (duplicates).
CREATE TABLE paths (
    id        INTEGER PRIMARY KEY,
    file_id   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    rel_path  TEXT    NOT NULL UNIQUE,  -- relative to root, '/' separators
    mtime_ns  INTEGER NOT NULL,
    byte_size INTEGER NOT NULL,
    seen_at   TEXT    NOT NULL
);
CREATE INDEX idx_paths_file ON paths(file_id);

CREATE TABLE chunks (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,       -- Chunk.id within its document
    page_start  INTEGER NOT NULL,
    page_end    INTEGER NOT NULL,
    origin      TEXT    NOT NULL,       -- 'native'|'synthesized'|'ocr'
    confidence  REAL,                   -- set iff origin='ocr'
    hard_split  INTEGER NOT NULL,       -- 0 | 1
    blocks      TEXT    NOT NULL,       -- JSON [{page, block}, ...]
    text        TEXT    NOT NULL,
    UNIQUE(file_id, ordinal)
);
CREATE INDEX idx_chunks_file ON chunks(file_id);

CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text,
    content      = 'chunks',
    content_rowid = 'id',
    tokenize     = 'unicode61 remove_diacritics 2'
);
-- Triggers keep the external-content FTS index in step with `chunks`.
```

**Why the `files`/`paths` split.** It is what makes content addressing real.
Two paths with identical bytes share one `files` row and one set of chunks.
A rename is an `UPDATE paths SET rel_path = ?`, with no parse.

**Why FTS5 now, when search is Stage 2.** This is the one part of the schema
that is expensive to add retroactively: populating an external-content FTS
index later means re-reading every chunk's text. Creating it now costs three
statements plus triggers and is kept current by the same transaction that
writes a chunk. This is justified forward-compatibility, not speculative
generality — and it is the only such concession in the schema.

**Room for Stage 4.** Future `sections`, `terms` and `captions` tables will
reference `files(id)` and `chunks(id)`. Nothing above needs to change to add
them.

`origin` and `confidence` are stored as separate columns rather than the
flattened JSON the extraction layer emits, because a later query will want
`WHERE origin = 'ocr' AND confidence < 0.7`. The export path reassembles the
documented JSON shape, so the public contract is unchanged.

## Section 4 — Invalidation

Two kinds of staleness, both recorded in `meta`:

**`extraction_version`** — a `u32` constant in the library, bumped by hand
when a change alters extraction output for identical input. When the stored
value differs from the binary's, every file is stale and `index` re-parses
everything.

This is not theoretical. Task 10's segment-merging fix changed the output of
unchanged PDFs — 2,860 chunks became 2,976 — which is exactly the case a
deliberate version bump must catch. A plain crate-version comparison is
rejected as too aggressive: a DOCX bug fix should not re-index a PDF corpus.

**`config_fingerprint`** — a hash over the settings that affect output:
`target_chars`, `max_chars`, `overlap_sentences`, `cross_page`, whether OCR
is enabled, and `ocr_dpi`. The fingerprint *and* the underlying values are
stored, so `status` can say which setting changed rather than only that
something did.

Both comparisons happen once, before the walk. A mismatch is reported, not
silently acted on: `status` shows it, and `index` states that it is
re-indexing everything and why.

## Section 5 — Atomicity and failure

**One transaction per file.** Deleting the old chunks, inserting the new,
and updating `files` and `paths` commit together. An interrupted run
therefore leaves an index where some files are current and some are stale —
queryable and internally consistent, never torn. Success criterion 4 is a
property of this structure rather than of careful coding.

**WAL mode**, so a crash mid-write rolls back cleanly and a reader is not
blocked by the writer.

**Single writer.** Concurrent `index` runs against one index are refused with
a clear message naming the other process's lock, rather than interleaved.

**Failure handling** preserves the extraction layer's per-file isolation: a
file that fails to parse is recorded with `status='failed'` and its error
message, the walk continues, and the exit code is 2 for a partial run. On a
later run an unchanged failed file yields `SkipFailed`, so a corrupt PDF does
not cost time on every invocation. `--retry-failed` forces reattempts, and
`status` lists them so they are visible rather than quietly dropped.

**Refusals, each with an actionable message:**

| Condition | Behaviour |
|---|---|
| `schema_version` newer than the binary understands | Refuse. Never write to a format from the future. |
| `schema_version` older, migration exists | Migrate inside a transaction. |
| `schema_version` older, no migration | Refuse, naming `--reindex`. |
| Target directory not writable | Refuse, naming `--index-dir`. |
| Index file corrupt | Refuse, naming `--reindex`. Do not silently rebuild. |
| `meta.root_path` differs from the root being indexed | Refuse. |

**An index belongs to exactly one root.** `paths.rel_path` is relative to
that root and unique, so pointing a second directory at the same
`--index-dir` would silently mix two corpora and make relative paths
meaningless. The root is recorded at creation and checked on every open. This
also gives `status` and `export` a basis for resolving paths back to real
files.

Silent rebuilding is excluded deliberately: discarding an index without being
asked is the same class of error as a silent fallback.

## Section 6 — CLI surface

```
docparse index [PATH]            PATH defaults to "."
    --index-dir <DIR>            put/find .docparse elsewhere
    --jobs <N>                   as today: min(cores, 4)
    --retry-failed               reattempt files recorded as failed
    --reindex                    ignore the cache, rebuild from scratch
    --ocr, --ocr-dpi             as today
    --chunk, --max-chunk, --overlap, --cross-page   as today

docparse status [PATH]
    --index-dir <DIR>
    --json                       machine-readable

docparse export [PATH]
    --index-dir <DIR>
    -o <FILE>                    default stdout

docparse <FILES...>              unchanged: stream JSONL, no index written
```

**`export` emits chunks only.** The schema stores chunks, not blocks, so
there is no `--emit blocks` here — exporting blocks would mean storing a
second, larger representation that nothing in Stages 1 to 4 reads, since each
chunk already carries its contributing `(page, block)` references. Blocks
mode remains available through the streaming path, which parses on demand.

**Unsupported extensions are skipped during the walk**, exactly as the
existing directory walker skips them today; they never reach the index. This
follows from the "no new input formats" non-goal — a directory of PDFs and
Markdown indexes the PDFs and ignores the Markdown until a text backend
exists.

`index` reports a one-line summary: how many files were parsed, linked,
touched, unchanged, failed and pruned. Those counts are the testable
observable for success criteria 1 to 3.

Exit codes follow the existing convention: `0` all succeeded, `1` all failed
or a fatal error, `2` partial.

`.docparse/.gitignore` containing `*` is written at creation, so an index
inside a git repository never pollutes it.

## Section 7 — Testing

**Unit, on `detect.rs`.** The action table is a pure function, so it gets
table-driven tests over every tier and boundary: unchanged, mtime-only
change, size change with identical hash, new content, known content at a new
path, previously failed. Mutation testing targets this module specifically,
since it is where a wrong answer silently corrupts the index.

**Integration, against a temporary directory.** Each success criterion
becomes a test asserting the reported counts:

| Scenario | Expectation |
|---|---|
| First index of N files | N parsed |
| Immediate re-run | **0 parsed**, N unchanged |
| Modify one file | **1 parsed**, N-1 unchanged |
| Rename a file | **0 parsed**, 1 path updated |
| Copy a file | **0 parsed**, 2 paths sharing 1 file row |
| Delete a file | pruned; its chunks gone; other files untouched |
| Touch mtime without editing | 0 parsed, 1 touched |
| Change `--chunk` | all parsed, reason reported |
| Bump `extraction_version` | all parsed, reason reported |
| Corrupt file in the set | status failed, batch completes, exit 2 |
| Re-run with a failed file | 0 parsed, failure still listed |
| `--retry-failed` | the failed file is reattempted |
| Read-only directory | refusal naming `--index-dir` |
| Future `schema_version` | refusal, index untouched |

**Round-trip.** `index` then `export` must produce JSONL byte-identical to
what the streaming path produces for the same inputs and settings. This pins
the storage layer as lossless and is the strongest single check available.

**Memory.** Indexing must not accumulate: chunks are written per file and
dropped, so peak heap stays bounded by one document as it is today. The
existing `peak_alloc` harness is extended to cover the index path.

## Section 8 — Forward compatibility

Constraints this stage must honour so later stages are additive:

- The index is locatable from a **root directory passed as an argument**,
  because the packaged installer supplies it from a native directory picker.
- **Subcommands exist from day one**, so `docparse mcp` slots in beside
  `index` without restructuring the CLI.
- **No interactive prompts anywhere in the index path.** A server process
  cannot answer them.
- `chunks_fts` is populated, so Stage 2's search is purely additive.
- `meta.schema_version` exists from the first release, so migrations are a
  designed path rather than a retrofit.

## Open questions

- **PDFium for `cargo install` users.** The DLL must sit beside the binary.
  A packaged bundle solves this by shipping it; `cargo install` does not.
  Static linking versus documentation is unresolved and affects only the
  engineer install path.
- **Index size.** Storing chunk text in SQLite roughly doubles on-disk
  footprint versus the source PDFs for text-heavy corpora. Acceptable, but
  unmeasured until there is something to measure.

## Known limitations

- Content identity means an edited file is wholly re-parsed; there is no
  partial or page-level reuse.
- Two files with identical content but meaningfully different names collapse
  to one `files` row. Both paths are retained, so nothing is lost, but
  per-path metadata beyond path and mtime has nowhere to live yet.
- The walk is single-threaded; parsing is parallel across files, as today.
