//! `docparse index`: translate arguments, run one pass, print one summary.

use docparse::chunk::ChunkConfig;
use docparse::index::{Index, IndexConfig, IndexOptions};

use crate::stream::default_jobs;
use crate::IndexArgs;

/// The fingerprinted settings, shared with `status` so the two always agree.
pub(crate) fn index_config(extract: &crate::ExtractArgs) -> IndexConfig {
    IndexConfig {
        chunk: ChunkConfig {
            target_chars: extract.chunk,
            max_chars: extract.max_chunk,
            overlap_sentences: extract.overlap,
            cross_page: extract.cross_page,
        },
        ocr: extract.ocr,
        ocr_dpi: extract.ocr_dpi,
    }
}

/// Returns the process exit code: 0 clean, 2 partial, 1 all failed or fatal.
pub(crate) fn run(args: &IndexArgs) -> i32 {
    let config = index_config(&args.extract);
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
    let opts = IndexOptions {
        jobs,
        retry_failed: args.retry_failed,
        reindex: args.reindex,
    };

    let mut index = match Index::open(&args.path, args.index_dir.as_deref()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let report = match index.index_dir(&config, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if let Some(reason) = &report.reason {
        println!("re-indexing all files: {reason}");
    }

    let total = report.parsed
        + report.linked
        + report.touched
        + report.unchanged
        + report.failed
        + report.skipped_failed
        + report.modified_during_run;
    let mut line = format!(
        "indexed {total} files: {} parsed, {} linked, {} unchanged, {} failed, {} pruned",
        report.parsed, report.linked, report.unchanged, report.failed, report.paths_pruned
    );
    if report.touched > 0 {
        line.push_str(&format!(", {} touched", report.touched));
    }
    if report.skipped_failed > 0 {
        line.push_str(&format!(
            ", {} skipped (failed before; --retry-failed to retry)",
            report.skipped_failed
        ));
    }
    if report.modified_during_run > 0 {
        line.push_str(&format!(
            ", {} changed during the run (not recorded)",
            report.modified_during_run
        ));
    }
    if report.walk_errors > 0 {
        line.push_str(&format!(
            ", {} unreadable (nothing pruned, settings not recorded)",
            report.walk_errors
        ));
    }
    println!("{line}");

    let succeeded = report.parsed + report.linked + report.touched + report.unchanged;
    let problems = report.failed + report.modified_during_run + report.walk_errors;
    if problems == 0 {
        0
    } else if succeeded == 0 {
        1
    } else {
        2
    }
}
