//! Asserts the spec's 64 MB budget. This measures peak *heap allocation*
//! via a tracking global allocator, not RSS: it excludes the binary, thread
//! stacks and any C-library allocations PDFium makes internally. That makes
//! it a tight, deterministic proxy rather than the whole truth, which is
//! the right trade for a test that must not flake.
//!
//! Run with `--test-threads=1`: a shared global allocator counts
//! allocations from every concurrently running test.

use docparse::chunk::{ChunkConfig, Chunker};
use docparse::model::{Block, BlockKind, Page, PageOrigin};
use docparse::{Result, Source};
use peak_alloc::PeakAlloc;
use std::sync::Mutex;

#[global_allocator]
static ALLOC: PeakAlloc = PeakAlloc;

/// Serialises the tests in this file even if someone forgets
/// `--test-threads=1`, since the allocator counter is process-wide.
static SERIAL: Mutex<()> = Mutex::new(());

const CEILING_BYTES: f32 = 64.0 * 1024.0 * 1024.0;

/// Synthesizes pages on demand without ever holding them all, so the test
/// harness itself cannot be what blows the budget.
struct HugeDocument {
    pages_remaining: u32,
    next_number: u32,
    blocks_per_page: usize,
}

impl Source for HugeDocument {
    fn next_page(&mut self) -> Option<Result<Page>> {
        if self.pages_remaining == 0 {
            return None;
        }
        self.pages_remaining -= 1;
        let number = self.next_number;
        self.next_number += 1;

        let blocks = (0..self.blocks_per_page)
            .map(|i| Block {
                id: i as u32,
                bbox: None,
                kind: BlockKind::Paragraph,
                text: format!(
                    "Page {number} block {i}. {}",
                    "This is filler prose that exists to occupy space. ".repeat(12)
                ),
            })
            .collect();

        Some(Ok(Page {
            number,
            width: 612.0,
            height: 792.0,
            origin: PageOrigin::Native,
            blocks,
        }))
    }
}

#[test]
fn a_five_thousand_page_document_stays_under_the_ceiling() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ALLOC.reset_peak_usage();

    let source = HugeDocument {
        pages_remaining: 5_000,
        next_number: 1,
        blocks_per_page: 20,
    };

    // Consume every chunk and keep nothing: this is how the CLI uses it.
    let mut count = 0usize;
    let mut total_chars = 0usize;
    for chunk in Chunker::new(source, "huge.pdf".to_string(), ChunkConfig::default()) {
        let chunk = chunk.unwrap();
        count += 1;
        total_chars += chunk.text.chars().count();
    }

    let peak = ALLOC.peak_usage() as f32;
    eprintln!(
        "5000 pages: peak heap {:.2} MB, {count} chunks",
        peak / 1024.0 / 1024.0
    );

    assert!(count > 1_000, "expected many chunks, got {count}");
    assert!(total_chars > 1_000_000, "expected a large document");
    assert!(
        peak < CEILING_BYTES,
        "peak heap {:.1} MB exceeded the {:.0} MB ceiling after {count} chunks",
        peak / 1024.0 / 1024.0,
        CEILING_BYTES / 1024.0 / 1024.0
    );
}

#[test]
fn peak_memory_does_not_grow_with_document_length() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The real property: ten times the pages must not mean ten times the
    // memory. A ceiling test alone could pass by luck on a small input.
    let measure = |pages: u32| -> f32 {
        ALLOC.reset_peak_usage();
        let source = HugeDocument {
            pages_remaining: pages,
            next_number: 1,
            blocks_per_page: 20,
        };
        for chunk in Chunker::new(source, "doc.pdf".to_string(), ChunkConfig::default()) {
            let _ = chunk.unwrap();
        }
        ALLOC.peak_usage() as f32
    };

    let small = measure(100);
    let large = measure(2_000);
    eprintln!(
        "100 pages: {:.2} MB; 2000 pages: {:.2} MB",
        small / 1024.0 / 1024.0,
        large / 1024.0 / 1024.0
    );

    assert!(
        large < small * 3.0,
        "memory grew with document length: {:.1} MB for 100 pages vs {:.1} MB for 2000",
        small / 1024.0 / 1024.0,
        large / 1024.0 / 1024.0
    );
}
