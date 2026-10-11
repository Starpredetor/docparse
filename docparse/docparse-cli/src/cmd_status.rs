//! `docparse status`: report what `index` would do, without doing it.

use docparse::index::Staleness;
use docparse::index::{Index, StatusReport};
use serde_json::json;

use crate::cmd_index::index_config;
use crate::StatusArgs;

/// Exit code: 0 report produced, 2 report produced but the walk was
/// incomplete (unreadable entries), 1 fatal (no index, unusable index).
pub(crate) fn run(args: &StatusArgs) -> i32 {
    let config = index_config(&args.extract);
    let index = match Index::open_existing(&args.path, args.index_dir.as_deref()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let report = match index.status(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if args.json {
        println!("{}", to_json(&report));
    } else {
        print_human(&report);
    }
    if report.unreadable > 0 {
        2
    } else {
        0
    }
}

fn to_json(r: &StatusReport) -> String {
    let staleness = match &r.staleness {
        Staleness::Current => json!({"state": "current"}),
        Staleness::FirstRun if r.stale_reason.is_some() => {
            json!({"state": "first_run_unfinished", "reason": r.stale_reason})
        }
        Staleness::FirstRun => json!({"state": "first_run"}),
        Staleness::ConfigChanged { from, to } => {
            json!({"state": "config_changed", "from": from, "to": to})
        }
        Staleness::ExtractionChanged { from, to } => {
            json!({"state": "extraction_changed", "from": from, "to": to})
        }
    };
    let failed: Vec<_> = r
        .failed
        .iter()
        .map(|(p, why)| json!({"path": p, "error": why}))
        .collect();
    json!({
        "counts": {
            "unchanged": r.unchanged.len(),
            "modified": r.modified.len(),
            "new": r.new_files.len(),
            "deleted": r.deleted.len(),
            "failed": r.failed.len(),
            "unreadable": r.unreadable,
        },
        "staleness": staleness,
        "modified": r.modified,
        "new": r.new_files,
        "deleted": r.deleted,
        "failed": failed,
    })
    .to_string()
}

fn print_human(r: &StatusReport) {
    if let Some(reason) = &r.stale_reason {
        let total = r.unchanged.len() + r.modified.len() + r.failed.len();
        println!(
            "STALE: {reason}; all {total} indexed files would be re-parsed by `docparse index`"
        );
    }
    section("modified", &r.modified);
    section("new", &r.new_files);
    section("deleted", &r.deleted);
    if !r.failed.is_empty() {
        println!("failed ({}):", r.failed.len());
        for (p, why) in &r.failed {
            println!("  {p}: {why}");
        }
    }
    if r.unreadable > 0 {
        println!(
            "{} entries could not be read; deleted files are not reported for an incomplete walk",
            r.unreadable
        );
    }
    println!(
        "{} unchanged, {} modified, {} new, {} deleted, {} failed",
        r.unchanged.len(),
        r.modified.len(),
        r.new_files.len(),
        r.deleted.len(),
        r.failed.len()
    );
}

fn section(name: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    println!("{name} ({}):", paths.len());
    for p in paths {
        println!("  {p}");
    }
}
