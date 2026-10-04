use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use docparse::chunk::{ChunkConfig, Chunker};
use docparse::model::{Block, BlockKind, Page, PageOrigin};
use docparse::{Result, Source};

/// A synthetic source, so chunker throughput is measured without disk or
/// PDF-parsing noise in the number.
struct Synthetic {
    remaining: u32,
    number: u32,
}

impl Source for Synthetic {
    fn next_page(&mut self) -> Option<Result<Page>> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        self.number += 1;
        Some(Ok(Page {
            number: self.number,
            width: 612.0,
            height: 792.0,
            origin: PageOrigin::Native,
            blocks: (0..8)
                .map(|i| Block {
                    id: i,
                    bbox: None,
                    kind: BlockKind::Paragraph,
                    text: "This is a representative sentence of body prose. ".repeat(8),
                })
                .collect(),
        }))
    }
}

fn chunker_throughput(c: &mut Criterion) {
    const PAGES: u32 = 500;
    let mut group = c.benchmark_group("chunker");
    group.throughput(Throughput::Elements(u64::from(PAGES)));

    group.bench_function("500_synthetic_pages", |b| {
        b.iter(|| {
            let source = Synthetic {
                remaining: PAGES,
                number: 0,
            };
            let mut count = 0usize;
            for chunk in Chunker::new(source, "bench".to_string(), ChunkConfig::default()) {
                // black_box keeps the optimizer from deleting the work.
                black_box(chunk.unwrap());
                count += 1;
            }
            black_box(count)
        })
    });
    group.finish();
}

#[cfg(feature = "pdf")]
fn pdf_throughput(c: &mut Criterion) {
    use std::path::PathBuf;

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two_pages.pdf");
    if !path.exists() {
        eprintln!("skipping pdf benchmark: fixture missing");
        return;
    }

    let mut group = c.benchmark_group("pdf");
    group.throughput(Throughput::Elements(2));
    group.bench_function("two_page_end_to_end", |b| {
        b.iter(|| {
            let source = docparse::pdf::PdfSource::open(&path).unwrap();
            let mut count = 0usize;
            for chunk in Chunker::new(source, "bench".to_string(), ChunkConfig::default()) {
                black_box(chunk.unwrap());
                count += 1;
            }
            black_box(count)
        })
    });
    group.finish();
}

#[cfg(feature = "pdf")]
criterion_group!(benches, chunker_throughput, pdf_throughput);
#[cfg(not(feature = "pdf"))]
criterion_group!(benches, chunker_throughput);
criterion_main!(benches);
