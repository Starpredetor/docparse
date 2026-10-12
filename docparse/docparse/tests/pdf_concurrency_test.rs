//! Liveness checks for the lock that serialises PDFium.
//!
//! # What these tests are, and what they are not
//!
//! `PdfSource` holds a process-wide mutex for its whole lifetime, because
//! PDFium has global state and is not thread-safe. That lock is NOT reentrant,
//! so the risk it introduces is a **deadlock**: one thread holding two
//! sources, or a guard that outlives its document. These tests exercise the
//! concurrent path so such a regression hangs here instead of in a user's
//! 11,000-file run.
//!
//! They are deliberately NOT regression tests for the original corruption bug.
//! That symptom could not be reproduced from the fixtures: 2,400 concurrent
//! reads of 1 MB fixtures passed in both debug and release, as did 400 distinct
//! copies of one fixture, while 3,000 *varied* real-world PDFs failed 780
//! times. Reproducing it needs content variety this repo does not ship, so an
//! "assert no errors" test here would pass with or without the lock and prove
//! nothing.
//!
//! The mutual exclusion itself is pinned deterministically by
//! `pdf::tests::only_one_pdf_source_can_exist_at_a_time`, in `src/pdf.rs`,
//! which fails if the guard is removed.

#![cfg(feature = "pdf")]

use std::path::PathBuf;

use docparse::pdf::PdfSource;
use docparse::source::Source;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Opens a PDF and drains every page, returning the first error as a string.
fn read_all(name: &str) -> Result<usize, String> {
    let mut source = PdfSource::open(&fixture(name)).map_err(|e| e.to_string())?;
    let mut pages = 0;
    while let Some(page) = source.next_page() {
        page.map_err(|e| e.to_string())?;
        pages += 1;
    }
    Ok(pages)
}

#[test]
fn concurrent_readers_all_make_progress_and_none_deadlock() {
    const FILES: [&str; 3] = ["two_pages.pdf", "messy_text.pdf", "blank.pdf"];
    const THREADS: usize = 8;
    const ROUNDS: usize = 12;

    let errors: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                scope.spawn(move || {
                    let mut errs = Vec::new();
                    for round in 0..ROUNDS {
                        // Stagger the starting file so threads contend for the
                        // lock while wanting *different* documents, which is
                        // the shape that would expose an ordering deadlock.
                        let name = FILES[(t + round) % FILES.len()];
                        if let Err(e) = read_all(name) {
                            errs.push(format!("thread {t} round {round} {name}: {e}"));
                        }
                    }
                    errs
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });

    assert!(
        errors.is_empty(),
        "{} of {} concurrent reads failed. First few:\n{}",
        errors.len(),
        THREADS * ROUNDS,
        errors
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn concurrent_reads_of_one_file_agree_on_its_page_count() {
    // A torn read can also succeed while returning too few pages, which an
    // error-only assertion would miss. Cheap to check, so check it.
    const THREADS: usize = 8;

    let counts: Vec<usize> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| scope.spawn(|| read_all("two_pages.pdf")))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .expect("worker thread panicked")
                    .expect("read failed")
            })
            .collect()
    });

    assert!(
        counts.iter().all(|&c| c == 2),
        "concurrent reads disagreed on page count: {counts:?}"
    );
}
