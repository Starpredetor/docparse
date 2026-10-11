//! The streaming path: bare `docparse <files>` writes JSONL to a sink.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use clap::ValueEnum;
use docparse::chunk::{ChunkConfig, Chunker};
use docparse::Error;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::Args;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Emit {
    /// Sentence-boundary chunks, the default.
    Chunks,
    /// Raw blocks, for callers who want to chunk themselves.
    Blocks,
}

/// Overall result of a batch, mapped to the process exit code.
pub(crate) enum Outcome {
    /// Every file succeeded (exit 0).
    AllOk,
    /// Some files succeeded and some failed (exit 2).
    Partial,
    /// Every file failed, or there was nothing to process (exit 1).
    AllFailed,
}

/// Why one file could not be processed. Separates document failures from
/// failures writing the output, so a broken pipe is not blamed on the input.
#[derive(Debug)]
pub(crate) enum FileError {
    /// The document could not be read or parsed.
    Document(Error),
    /// The output sink rejected a write (for example a closed pipe).
    Output(std::io::Error),
}

impl From<Error> for FileError {
    fn from(e: Error) -> Self {
        FileError::Document(e)
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::Document(e) => write!(f, "{e}"),
            FileError::Output(e) => write!(f, "could not write output: {e}"),
        }
    }
}

/// Process every input, streaming output to `out`. A failing file is
/// reported on stderr and does not stop the batch.
pub(crate) fn run(args: &Args, out: &mut (dyn Write + Send)) -> Outcome {
    let inputs = collect_inputs(&args.paths);

    if inputs.is_empty() {
        eprintln!("error: no supported documents found in the given paths");
        return Outcome::AllFailed;
    }

    let cfg = ChunkConfig {
        target_chars: args.extract.chunk,
        max_chars: args.extract.max_chunk,
        overlap_sentences: args.extract.overlap,
        cross_page: args.extract.cross_page,
    };

    // Deliberately not all cores: on a 2-core machine that is
    // oversubscription, and on an HDD the disk is the bottleneck anyway.
    let jobs = args
        .jobs
        .unwrap_or_else(|| {
            default_jobs(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
            )
        })
        .max(1);

    let results: Vec<std::result::Result<(), FileError>> = if jobs == 1 {
        // Sequential: streams straight to the sink with no buffering, and
        // keeps input order, so output is byte-identical across runs.
        inputs
            .iter()
            .map(|path| {
                process_file(
                    path,
                    args.emit,
                    cfg,
                    args.extract.ocr,
                    args.extract.ocr_dpi,
                    out,
                )
            })
            .collect()
    } else {
        run_parallel(
            &inputs,
            args.emit,
            cfg,
            (args.extract.ocr, args.extract.ocr_dpi),
            jobs,
            out,
        )
    };

    let mut ok = 0usize;
    let mut failed = 0usize;
    for result in results {
        match result {
            Ok(()) => ok += 1,
            Err(e) => {
                // Per-file isolation: report and keep going.
                eprintln!("error: {e}");
                failed += 1;
            }
        }
    }

    match (ok, failed) {
        (_, 0) => Outcome::AllOk,
        (0, _) => Outcome::AllFailed,
        _ => Outcome::Partial,
    }
}

/// Default worker count for a machine with `cores` cores: never all of them,
/// capped at 4 (see the comment in `run`).
pub(crate) fn default_jobs(cores: usize) -> usize {
    cores.clamp(1, 4)
}

/// Process files on `jobs` workers.
///
/// Each worker renders its own file into a private buffer, then takes the
/// sink lock only to write that buffer out. A file's lines therefore stay
/// contiguous and ordered, but files land in completion order. Restoring
/// input order would mean holding every file's output until the end, so
/// memory would scale with the corpus; that contradicts the project's
/// premise, so grouped-but-unordered is deliberate. The buffer is bounded
/// by one document's JSONL and is dropped before the worker takes another.
pub(crate) fn run_parallel(
    inputs: &[PathBuf],
    emit: Emit,
    cfg: ChunkConfig,
    (ocr, ocr_dpi): (bool, u32),
    jobs: usize,
    out: &mut (dyn Write + Send),
) -> Vec<std::result::Result<(), FileError>> {
    let pool = match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
        Ok(pool) => pool,
        Err(e) => {
            // No silent fallback to one thread: report, fail every file.
            eprintln!("error: could not start {jobs} workers: {e}");
            return inputs
                .iter()
                .map(|_| {
                    Err(FileError::Output(std::io::Error::other(
                        "worker pool unavailable",
                    )))
                })
                .collect();
        }
    };

    let sink = Mutex::new(out);

    pool.install(|| {
        inputs
            .par_iter()
            .map(|path| {
                let mut buffer: Vec<u8> = Vec::new();
                let parsed = process_file(path, emit, cfg, ocr, ocr_dpi, &mut buffer);

                // Flush whatever was produced even if the file failed
                // partway, matching the --jobs 1 behaviour in --help.
                // A write failure outranks a document failure: it is the
                // more fundamental problem and must not be blamed on the input.
                let written = sink
                    .lock()
                    .expect("output mutex poisoned")
                    .write_all(&buffer)
                    .map_err(FileError::Output);

                written.and(parsed)
            })
            .collect()
    })
}

/// Open one file and write its chunks or blocks to `out` as they arrive.
pub(crate) fn process_file(
    path: &Path,
    emit: Emit,
    cfg: ChunkConfig,
    ocr: bool,
    ocr_dpi: u32,
    out: &mut (dyn Write + Send),
) -> std::result::Result<(), FileError> {
    let source = docparse::open_source(path, ocr, ocr_dpi)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    match emit {
        Emit::Chunks => {
            for chunk in Chunker::new(source, name, cfg) {
                let chunk = chunk?;
                let line = serde_json::to_string(&chunk).map_err(|e| {
                    FileError::Document(Error::Malformed {
                        path: path.to_path_buf(),
                        detail: format!("could not serialize chunk {}: {e}", chunk.id),
                    })
                })?;
                writeln!(out, "{line}").map_err(FileError::Output)?;
            }
        }
        Emit::Blocks => {
            let mut source = source;
            while let Some(page) = source.next_page() {
                let page = page?;
                for block in &page.blocks {
                    // Flatten the page number into each block so a single
                    // JSONL line is self-describing.
                    let mut value = serde_json::to_value(block).map_err(|e| {
                        FileError::Document(Error::Malformed {
                            path: path.to_path_buf(),
                            detail: format!("could not serialize block: {e}"),
                        })
                    })?;
                    if let Some(map) = value.as_object_mut() {
                        // Provenance: an OCR guess must be distinguishable from
                        // extracted text. Adds `origin` and, for OCR, `confidence`.
                        if let Ok(serde_json::Value::Object(origin)) =
                            serde_json::to_value(page.origin)
                        {
                            map.extend(origin);
                        }
                        map.insert("page".into(), page.number.into());
                        map.insert("source".into(), name.clone().into());
                    }
                    writeln!(out, "{value}").map_err(FileError::Output)?;
                }
            }
        }
    }

    Ok(())
}

/// Expand directories recursively (sorted, supported extensions only) and
/// pass explicitly named files through untouched.
/// `index`, `status` or `export` given as a path that does not exist: almost
/// certainly a subcommand placed after a flag, which clap reads as a path.
fn subcommand_word(path: &Path) -> Option<&'static str> {
    ["index", "status", "export"]
        .into_iter()
        .find(|w| path.as_os_str() == *w && !path.exists())
}

pub(crate) fn collect_inputs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();

    for path in paths {
        if let Some(word) = subcommand_word(path) {
            eprintln!("error: `{word}` is a subcommand - did you mean `docparse {word} <path>`?");
        }
        if path.is_dir() {
            for entry in WalkDir::new(path).sort_by_file_name() {
                let Ok(entry) = entry else { continue };
                if entry.file_type().is_file() && docparse::is_supported(entry.path()) {
                    found.push(entry.path().to_path_buf());
                }
            }
        } else {
            // An explicitly named file is attempted even if the extension
            // is unknown, so the user gets a real error rather than silence.
            found.push(path.clone());
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::default_jobs;

    #[test]
    fn default_workers_are_capped_at_four_and_never_zero() {
        let got: Vec<usize> = [0, 1, 2, 4, 8, 20].into_iter().map(default_jobs).collect();
        assert_eq!(got, [1, 1, 2, 4, 4, 4]);
    }
}
