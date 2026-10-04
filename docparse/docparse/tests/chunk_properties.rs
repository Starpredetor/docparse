use docparse::chunk::{split_sentences, ChunkConfig, Chunker};
use docparse::model::{Block, BlockKind, Page, PageOrigin};
use docparse::{Chunk, Result, Source};

/// A `Source` backed by a fixed list of pages, so chunker tests need no
/// file format at all.
struct FakeSource {
    pages: std::vec::IntoIter<Page>,
}

impl Source for FakeSource {
    fn next_page(&mut self) -> Option<Result<Page>> {
        self.pages.next().map(Ok)
    }
}

fn page(number: u32, texts: &[&str]) -> Page {
    Page {
        number,
        width: 612.0,
        height: 792.0,
        origin: PageOrigin::Native,
        blocks: texts
            .iter()
            .enumerate()
            .map(|(i, t)| Block {
                id: i as u32,
                bbox: None,
                kind: BlockKind::Paragraph,
                text: (*t).to_string(),
            })
            .collect(),
    }
}

fn chunks(pages: Vec<Page>, cfg: ChunkConfig) -> Vec<Chunk> {
    let source = FakeSource {
        pages: pages.into_iter(),
    };
    Chunker::new(source, "doc.pdf".to_string(), cfg)
        .collect::<Result<Vec<_>>>()
        .unwrap()
}

/// `n` sentences of roughly 44 characters each.
fn sentences(n: usize) -> String {
    (0..n)
        .map(|i| format!("This is sentence number {i} of the test body."))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn no_chunk_ever_ends_mid_sentence() {
    let body = sentences(40);
    let out = chunks(vec![page(1, &[&body])], ChunkConfig::default());

    assert!(out.len() > 1, "expected the body to need several chunks");
    for c in &out {
        let trimmed = c.text.trim_end();
        assert!(
            trimmed.ends_with('.') || trimmed.ends_with('!') || trimmed.ends_with('?'),
            "chunk {} ends mid-sentence: {:?}",
            c.id,
            trimmed
        );
    }
}

#[test]
fn every_sentence_appears_in_at_least_one_chunk() {
    let body = sentences(40);
    let out = chunks(vec![page(1, &[&body])], ChunkConfig::default());
    let joined = out
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    for i in 0..40 {
        assert!(
            joined.contains(&format!("sentence number {i} of")),
            "lost sentence {i}"
        );
    }
}

#[test]
fn chunks_do_not_span_pages_by_default_and_page_numbers_are_exact() {
    let out = chunks(
        vec![
            page(1, &["Page one sentence."]),
            page(2, &["Page two sentence."]),
            page(7, &["Page seven sentence."]),
        ],
        ChunkConfig::default(),
    );

    assert_eq!(out.len(), 3);
    for c in &out {
        assert_eq!(
            c.page_start, c.page_end,
            "chunk {} spans pages though cross_page is false",
            c.id
        );
    }
    // Real source page numbers, not positional indices. The Python code
    // clamps here and mislabels every late chunk with the last page.
    assert_eq!(out[0].page_start, 1);
    assert_eq!(out[1].page_start, 2);
    assert_eq!(out[2].page_start, 7);
    assert_eq!(out[2].text, "Page seven sentence.");
}

#[test]
fn overlap_repeats_the_configured_number_of_sentences() {
    let cfg = ChunkConfig {
        target_chars: 60,
        max_chars: 200,
        overlap_sentences: 1,
        cross_page: false,
    };
    let body =
        "Alpha one here. Bravo two here. Charlie three here. Delta four here. Echo five here.";
    let out = chunks(vec![page(1, &[body])], cfg);

    assert!(out.len() >= 2, "got {} chunks", out.len());
    for pair in out.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        let last = split_sentences(&prev.text).last().unwrap().to_string();
        assert!(
            next.text.starts_with(&last),
            "chunk {} should begin with {last:?}, got {:?}",
            next.id,
            next.text
        );
    }
}

#[test]
fn zero_overlap_duplicates_nothing() {
    let cfg = ChunkConfig {
        target_chars: 60,
        max_chars: 200,
        overlap_sentences: 0,
        cross_page: false,
    };
    let body = "Alpha one here. Bravo two here. Charlie three here. Delta four here.";
    let out = chunks(vec![page(1, &[body])], cfg);

    let total: usize = out.iter().map(|c| c.text.chars().count()).sum();
    assert!(
        total <= body.chars().count() + out.len(),
        "with no overlap chunks must not duplicate text: {total} vs {}",
        body.chars().count()
    );
}

#[test]
fn an_oversized_single_sentence_is_hard_split_and_flagged() {
    let giant = format!("{}end", "word ".repeat(400));
    let cfg = ChunkConfig {
        target_chars: 200,
        max_chars: 300,
        overlap_sentences: 1,
        cross_page: false,
    };
    let out = chunks(vec![page(1, &[&giant])], cfg);

    assert!(out.len() > 1);
    assert!(
        out.iter().all(|c| c.hard_split),
        "pieces of an oversized sentence must be flagged"
    );
    for c in &out {
        assert!(
            c.text.chars().count() <= 300,
            "piece exceeds max_chars: {}",
            c.text.chars().count()
        );
    }
}

#[test]
fn chunks_record_which_blocks_they_came_from() {
    let out = chunks(
        vec![page(
            1,
            &["First block sentence.", "Second block sentence."],
        )],
        ChunkConfig::default(),
    );

    assert_eq!(out.len(), 1, "both blocks fit in one chunk");
    let refs: Vec<(u32, u32)> = out[0].blocks.iter().map(|b| (b.page, b.block)).collect();
    assert_eq!(refs, vec![(1, 0), (1, 1)]);
}

#[test]
fn cross_page_mode_lets_a_chunk_span_pages() {
    let cfg = ChunkConfig {
        target_chars: 400,
        max_chars: 800,
        overlap_sentences: 0,
        cross_page: true,
    };
    let out = chunks(
        vec![
            page(1, &["Short page one."]),
            page(2, &["Short page two."]),
            page(3, &["Short page three."]),
        ],
        cfg,
    );

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].page_start, 1);
    assert_eq!(out[0].page_end, 3);
}

#[test]
fn chunk_ids_are_sequential_from_zero() {
    let body = sentences(30);
    let out = chunks(vec![page(1, &[&body])], ChunkConfig::default());
    let ids: Vec<u64> = out.iter().map(|c| c.id).collect();
    assert_eq!(ids, (0..out.len() as u64).collect::<Vec<_>>());
}

#[test]
fn an_empty_document_produces_no_chunks() {
    assert!(chunks(vec![], ChunkConfig::default()).is_empty());
    assert!(chunks(vec![page(1, &[])], ChunkConfig::default()).is_empty());
}

#[test]
fn output_is_deterministic_across_runs() {
    let body = sentences(40);
    let build = || vec![page(1, &[&body]), page(2, &[&body])];

    let a = chunks(build(), ChunkConfig::default());
    let b = chunks(build(), ChunkConfig::default());

    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

/// A source that records how many pages the chunker has pulled so far.
struct CountingSource {
    pages: std::vec::IntoIter<Page>,
    pulled: std::rc::Rc<std::cell::Cell<usize>>,
}

impl Source for CountingSource {
    fn next_page(&mut self) -> Option<Result<Page>> {
        let p = self.pages.next();
        if p.is_some() {
            self.pulled.set(self.pulled.get() + 1);
        }
        p.map(Ok)
    }
}

fn page_with_origin(number: u32, text: &str, origin: PageOrigin) -> Page {
    let mut p = page(number, &[text]);
    p.origin = origin;
    p
}

/// Regression for the cross-page fixed point: a short sentence followed by a
/// long-but-legal one never reaches `target_chars` from the front, so a
/// chunker that merely waits for more input buffers the whole document.
/// This is the measurement behind the bounded-memory promise.
#[test]
fn cross_page_mode_buffers_a_bounded_number_of_pages() {
    let long = format!("{}.", "w".repeat(1000));
    let pages: Vec<Page> = (1..=500u32)
        .map(|n| page(n, &[&format!("Ab. {long}")]))
        .collect();
    let pulled = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let source = CountingSource {
        pages: pages.into_iter(),
        pulled: pulled.clone(),
    };
    let cfg = ChunkConfig {
        target_chars: 800,
        max_chars: 1200,
        overlap_sentences: 1,
        cross_page: true,
    };
    let mut chunker = Chunker::new(source, "doc.pdf".to_string(), cfg);

    let first = chunker.next().expect("a chunk").unwrap();
    assert!(!first.text.is_empty());
    assert!(
        pulled.get() <= 3,
        "first chunk needed {} pages; pending is growing with the document",
        pulled.get()
    );
}

#[test]
fn a_chunk_mixing_native_and_ocr_is_reported_as_ocr() {
    let cfg = ChunkConfig {
        cross_page: true,
        overlap_sentences: 0,
        ..ChunkConfig::default()
    };
    let out = chunks(
        vec![
            page_with_origin(1, "Native text.", PageOrigin::Native),
            page_with_origin(2, "Guessed text.", PageOrigin::Ocr { confidence: 0.6 }),
        ],
        cfg,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].origin, PageOrigin::Ocr { confidence: 0.6 });
}

#[test]
fn a_chunk_spanning_two_ocr_pages_reports_the_lower_confidence() {
    let cfg = ChunkConfig {
        cross_page: true,
        overlap_sentences: 0,
        ..ChunkConfig::default()
    };
    let out = chunks(
        vec![
            page_with_origin(1, "First text.", PageOrigin::Ocr { confidence: 0.9 }),
            page_with_origin(2, "Second text.", PageOrigin::Ocr { confidence: 0.4 }),
        ],
        cfg,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].origin, PageOrigin::Ocr { confidence: 0.4 });
}

#[test]
fn hard_split_never_cuts_inside_a_multibyte_character() {
    let giant = "é".repeat(500);
    let cfg = ChunkConfig {
        target_chars: 200,
        max_chars: 301,
        overlap_sentences: 1,
        cross_page: false,
    };
    let out = chunks(vec![page(1, &[&giant])], cfg);

    assert!(out.len() > 1);
    assert!(out.iter().all(|c| c.hard_split));
    assert!(out.iter().all(|c| c.text.chars().count() <= 301));
    let rejoined: String = out.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(rejoined, giant);
}

#[test]
fn several_sentences_from_one_block_collapse_to_one_block_ref() {
    let out = chunks(
        vec![page(1, &["One here. Two here. Three here."])],
        ChunkConfig::default(),
    );
    assert_eq!(out.len(), 1);
    let refs: Vec<(u32, u32)> = out[0].blocks.iter().map(|b| (b.page, b.block)).collect();
    assert_eq!(refs, vec![(1, 0)]);
}

#[test]
fn a_block_contributing_at_two_separate_positions_appears_twice() {
    let mut p = page(1, &["A one.", "B two.", "C three."]);
    p.blocks[2].id = 0;
    let out = chunks(vec![p], ChunkConfig::default());
    assert_eq!(out.len(), 1);
    let refs: Vec<(u32, u32)> = out[0].blocks.iter().map(|b| (b.page, b.block)).collect();
    assert_eq!(refs, vec![(1, 0), (1, 1), (1, 0)]);
}

#[test]
fn overlap_is_not_emitted_alone_before_an_oversized_sentence() {
    // Distinct words, so a hard-split piece is never a substring of another.
    let giant = format!(
        "{}.",
        (0..40)
            .map(|i| format!("w{i:03}x"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let body = format!("Alpha one here. Bravo two here. {giant}");
    let cfg = ChunkConfig {
        target_chars: 60,
        max_chars: 100,
        overlap_sentences: 1,
        cross_page: false,
    };
    let out = chunks(vec![page(1, &[&body])], cfg);

    for pair in out.windows(2) {
        assert!(
            !pair[0].text.contains(&pair[1].text),
            "chunk {} carries no new text: {:?}",
            pair[1].id,
            pair[1].text
        );
    }
}

#[test]
fn max_chars_is_a_ceiling_even_when_target_chars_exceeds_it() {
    let body = sentences(10);
    let cfg = ChunkConfig {
        target_chars: 500,
        max_chars: 100,
        overlap_sentences: 0,
        cross_page: false,
    };
    let out = chunks(vec![page(1, &[&body])], cfg);
    for c in &out {
        assert!(c.text.chars().count() <= 100, "{}", c.text.chars().count());
    }
}
