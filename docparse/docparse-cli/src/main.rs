//! The `docparse` command.
//!
//! JSONL on stdout, diagnostics on stderr, exit code summarising the batch.

mod cmd_export;
mod cmd_index;
mod cmd_status;
mod stream;

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use stream::{Emit, Outcome};

#[derive(Debug, Subcommand)]
enum Commands {
    /// Index a directory, parsing only what changed since the last run.
    Index(IndexArgs),
    /// Report what is new, changed, deleted, failed or unchanged.
    Status(StatusArgs),
    /// Write the indexed chunks out as JSONL.
    Export(ExportArgs),
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// Directory to index. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Put or find `.docparse` somewhere other than inside PATH.
    #[arg(long)]
    index_dir: Option<PathBuf>,
    /// Worker threads. Defaults to min(cores, 4). MEMORY: indexing holds
    /// about 2 x jobs documents in flight; measured 35 MB at --jobs 1 and
    /// 76 MB at --jobs 4 on a 94 MB corpus, so the 64 MB budget applies to
    /// --jobs 1 and the streaming path, not to parallel indexing.
    #[arg(short, long)]
    jobs: Option<usize>,
    /// Reattempt files previously recorded as failed.
    #[arg(long)]
    retry_failed: bool,
    /// Ignore the cache and rebuild from scratch.
    #[arg(long)]
    reindex: bool,
    #[command(flatten)]
    extract: ExtractArgs,
}

#[derive(Debug, clap::Args)]
struct StatusArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    index_dir: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    extract: ExtractArgs,
}

#[derive(Debug, clap::Args)]
struct ExportArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    index_dir: Option<PathBuf>,
    #[arg(short, long)]
    output: Option<PathBuf>,
}

/// The settings that affect extraction output. Flattened into the bare form
/// AND into `index`/`status`, because spec Section 6 lists them under `index`
/// and Task 5 fingerprints them.
#[derive(Debug, Clone, clap::Args)]
struct ExtractArgs {
    /// Target chunk size in characters.
    #[arg(long, default_value_t = 800)]
    chunk: usize,

    /// Hard ceiling in characters; only an oversized sentence is ever cut.
    #[arg(long, default_value_t = 1200)]
    max_chunk: usize,

    /// Sentences repeated between consecutive chunks.
    #[arg(long, default_value_t = 1)]
    overlap: usize,

    /// Allow a chunk to span pages. Off by default so page attribution is exact.
    #[arg(long)]
    cross_page: bool,

    /// OCR PDF pages that yield no extractable text. Off by default: it is
    /// slow and its output is a guess rather than extracted text. Image
    /// files are always OCR'd and need no flag. Needs a build with the `ocr`
    /// feature and the tesseract executable. MEMORY: the 64 MB budget does
    /// NOT hold under OCR, at any job count. Measured
    /// A4 scans at 150 DPI use about 52 MB in-process plus a ~62 MB tesseract
    /// process at --jobs 1, and about 152 MB plus ~213 MB total at --jobs 4.
    /// Budget roughly 50 MB plus a comparable tesseract per worker.
    #[arg(long)]
    ocr: bool,

    /// Render DPI for OCR. Higher is more accurate; the page bitmap grows
    /// with the square of the DPI.
    #[arg(long, default_value_t = 150)]
    ocr_dpi: u32,
}

#[derive(Debug, Parser)]
#[command(
    name = "docparse",
    args_conflicts_with_subcommands = true,
    about = "Extract structure-preserving JSONL from documents. Offline, no models.",
    after_help = "A file that fails partway through may leave its earlier records on the output; the error is reported on stderr and the exit code is non-zero."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Files or directories to process. Directories are walked recursively.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Write JSONL here instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,

    #[command(flatten)]
    extract: ExtractArgs,

    /// What to emit.
    #[arg(long, value_enum, default_value_t = Emit::Chunks)]
    emit: Emit,

    /// Worker threads. Defaults to min(cores, 4). With 1, output is
    /// byte-identical across runs; with more, each file's lines stay
    /// together but files appear in completion order, not input order.
    #[arg(short, long)]
    jobs: Option<usize>,
}

fn main() {
    let args = Args::parse();

    match &args.command {
        Some(Commands::Index(index_args)) => std::process::exit(cmd_index::run(index_args)),
        Some(Commands::Status(status_args)) => std::process::exit(cmd_status::run(status_args)),
        Some(Commands::Export(export_args)) => std::process::exit(cmd_export::run(export_args)),
        None => { /* fall through to the existing streaming path */ }
    }

    let mut sink: Box<dyn Write + Send> = match &args.output {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Box::new(BufWriter::new(file)),
            Err(e) => {
                eprintln!("error: cannot write {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    let outcome = stream::run(&args, &mut sink);

    if let Err(e) = sink.flush() {
        eprintln!("error: could not flush output: {e}");
        std::process::exit(1);
    }

    std::process::exit(match outcome {
        Outcome::AllOk => 0,
        Outcome::AllFailed => 1,
        Outcome::Partial => 2,
    });
}
