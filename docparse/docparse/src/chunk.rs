//! Structure-aware chunking.
//!
//! Chunks break on sentence boundaries, never mid-sentence, and carry the
//! real page numbers and block ids their text came from.

use std::collections::VecDeque;

use crate::error::Result;
use crate::model::{BlockRef, Chunk, Page, PageOrigin};
use crate::normalize::normalize_block_text;
use crate::source::Source;

/// Splits on `.`, `!` and `?` when followed by whitespace or end of input,
/// absorbing trailing quotes and brackets so `he said "stop."` stays whole.
///
/// Deliberately simple: no abbreviation dictionary, no locale rules. It
/// cuts only at ASCII punctuation, so every byte index it produces is a
/// valid UTF-8 boundary.
pub fn split_sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if matches!(bytes[i], b'.' | b'!' | b'?') {
            let mut end = i + 1;
            while end < bytes.len()
                && matches!(bytes[end], b'.' | b'!' | b'?' | b'"' | b'\'' | b')' | b']')
            {
                end += 1;
            }

            if end >= bytes.len() || bytes[end].is_ascii_whitespace() {
                let piece = text[start..end].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                start = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }

    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }

    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkConfig {
    /// Soft target in characters; a chunk closes once it reaches this.
    /// Clamped down to `max_chars` by `Chunker::new`, so `max_chars` stays a
    /// true ceiling.
    pub target_chars: usize,
    /// Hard ceiling. Only a single oversized sentence can trigger a cut.
    pub max_chars: usize,
    /// How many trailing sentences repeat at the start of the next chunk.
    /// Overlap is silently dropped when a chunk consumes no more than this
    /// many sentences: re-queuing them would make no forward progress.
    pub overlap_sentences: usize,
    /// When false a chunk never spans pages, so page attribution is exact.
    pub cross_page: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_chars: 800,
            max_chars: 1200,
            overlap_sentences: 1,
            cross_page: false,
        }
    }
}

/// One sentence plus where it came from.
#[derive(Clone)]
struct Sentence {
    text: String,
    page: u32,
    block: u32,
    origin: PageOrigin,
}

pub struct Chunker<S: Source> {
    source: S,
    document: String,
    cfg: ChunkConfig,
    /// Sentences read but not yet emitted. Bounded by one page's text,
    /// which is what makes the memory ceiling structural rather than tuned.
    pending: Vec<Sentence>,
    ready: VecDeque<Chunk>,
    next_id: u64,
    exhausted: bool,
}

impl<S: Source> Chunker<S> {
    pub fn new(source: S, document_name: String, mut cfg: ChunkConfig) -> Self {
        cfg.target_chars = cfg.target_chars.min(cfg.max_chars);
        Self {
            source,
            document: document_name,
            cfg,
            pending: Vec::new(),
            ready: VecDeque::new(),
            next_id: 0,
            exhausted: false,
        }
    }

    fn ingest(&mut self, page: Page) {
        for block in &page.blocks {
            let normalized = normalize_block_text(&block.text);
            for sentence in split_sentences(&normalized) {
                self.pending.push(Sentence {
                    text: sentence.to_string(),
                    page: page.number,
                    block: block.id,
                    origin: page.origin,
                });
            }
        }
    }

    fn push_chunk(&mut self, text: String, parts: &[Sentence], hard_split: bool) {
        let page_start = parts.iter().map(|s| s.page).min().unwrap_or(0);
        let page_end = parts.iter().map(|s| s.page).max().unwrap_or(0);

        let mut blocks: Vec<BlockRef> = Vec::new();
        for s in parts {
            let r = BlockRef {
                page: s.page,
                block: s.block,
            };
            if blocks.last() != Some(&r) {
                blocks.push(r);
            }
        }

        let origin = weakest_origin(parts);

        self.ready.push_back(Chunk {
            id: self.next_id,
            source: self.document.clone(),
            page_start,
            page_end,
            blocks,
            text,
            origin,
            hard_split,
        });
        self.next_id += 1;
    }

    /// Emits chunks from `pending`. When `flush` is false, stops as soon as
    /// the remainder is too small to reach `target_chars`, so the next page
    /// can contribute to it.
    fn drain(&mut self, flush: bool) {
        loop {
            if self.pending.is_empty() {
                return;
            }

            // An oversized lone sentence is the only thing ever cut.
            if self.pending[0].text.chars().count() > self.cfg.max_chars {
                let s = self.pending.remove(0);
                for piece in hard_split(&s.text, self.cfg.max_chars) {
                    self.push_chunk(piece, std::slice::from_ref(&s), true);
                }
                continue;
            }

            let mut take = 0usize;
            let mut len = 0usize;
            for s in &self.pending {
                let n = s.text.chars().count();
                let added = if take == 0 { n } else { n + 1 }; // +1 for the joining space
                if take > 0 && len + added > self.cfg.target_chars {
                    break;
                }
                len += added;
                take += 1;
                if len >= self.cfg.target_chars {
                    break;
                }
            }

            // Wait for more input only when the WHOLE buffer was scanned and
            // still fell short. If the scan stopped early because the next
            // sentence would overshoot the target, more input can never
            // change the outcome: waiting would be a fixed point that buffers
            // the entire document instead of emitting.
            if take == self.pending.len() && len < self.cfg.target_chars && !flush {
                return;
            }

            let used: Vec<Sentence> = self.pending.drain(..take).collect();
            let text = used
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            self.push_chunk(text, &used, false);

            // Re-queue overlap only when a regular chunk will follow. The
            // non-empty check avoids a trailing chunk of pure overlap, and
            // the oversized check avoids the same when the next sentence will
            // be hard-split instead. `take > k` guarantees termination:
            // each pass then consumes at least one new sentence.
            if let Some(next) = self.pending.first() {
                let k = self.cfg.overlap_sentences.min(used.len());
                if k > 0 && take > k && next.text.chars().count() <= self.cfg.max_chars {
                    for s in used[used.len() - k..].iter().rev() {
                        self.pending.insert(0, s.clone());
                    }
                }
            }
        }
    }
}

impl<S: Source> Iterator for Chunker<S> {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(chunk) = self.ready.pop_front() {
                return Some(Ok(chunk));
            }

            if self.exhausted {
                if self.pending.is_empty() {
                    return None;
                }
                self.drain(true);
                continue;
            }

            match self.source.next_page() {
                None => {
                    self.exhausted = true;
                    self.drain(true);
                }
                Some(Err(e)) => {
                    self.exhausted = true;
                    return Some(Err(e));
                }
                Some(Ok(page)) => {
                    self.ingest(page);
                    // Not crossing pages means flushing at every page
                    // boundary, which is exactly what keeps attribution exact.
                    self.drain(!self.cfg.cross_page);
                }
            }
        }
    }
}

/// The weakest provenance claim among the contributing sentences.
///
/// A chunk is only as trustworthy as its least trustworthy sentence, so an
/// OCR guess anywhere in a chunk makes the whole chunk an OCR guess. Taking
/// the first sentence's origin would let a half-OCR chunk claim to be
/// extracted text. Order, weakest first: `Ocr` (lowest confidence wins),
/// `Synthesized`, `Native`.
fn weakest_origin(parts: &[Sentence]) -> PageOrigin {
    let mut weakest = parts
        .first()
        .expect("push_chunk is never called without parts")
        .origin;
    for s in &parts[1..] {
        weakest = match (weakest, s.origin) {
            (PageOrigin::Ocr { confidence: a }, PageOrigin::Ocr { confidence: b }) => {
                PageOrigin::Ocr {
                    confidence: a.min(b),
                }
            }
            (w @ PageOrigin::Ocr { .. }, _) => w,
            (_, o @ PageOrigin::Ocr { .. }) => o,
            (w @ PageOrigin::Synthesized, _) => w,
            (_, o @ PageOrigin::Synthesized) => o,
            (w, _) => w,
        };
    }
    weakest
}

/// Cuts an oversized sentence into `max_chars`-sized pieces on character
/// boundaries. Byte slicing would panic on multibyte text.
fn hard_split(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(max_chars.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

#[cfg(test)]
mod sentence_tests {
    use super::*;

    #[test]
    fn splits_on_terminators_followed_by_space() {
        assert_eq!(
            split_sentences("One. Two! Three?"),
            vec!["One.", "Two!", "Three?"]
        );
    }

    #[test]
    fn keeps_a_decimal_number_intact() {
        // No whitespace after the period, so it is not a sentence end.
        assert_eq!(
            split_sentences("Pi is 3.14 exactly."),
            vec!["Pi is 3.14 exactly."]
        );
    }

    #[test]
    fn absorbs_closing_quotes_into_the_sentence() {
        assert_eq!(
            split_sentences("He said \"stop.\" Then he left."),
            vec!["He said \"stop.\"", "Then he left."]
        );
    }

    #[test]
    fn handles_multibyte_text_without_panicking() {
        assert_eq!(
            split_sentences("Café ouvert. Naïve approche! Fin?"),
            vec!["Café ouvert.", "Naïve approche!", "Fin?"]
        );
    }

    #[test]
    fn text_without_terminators_is_one_sentence() {
        assert_eq!(
            split_sentences("no terminator here"),
            vec!["no terminator here"]
        );
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(split_sentences("").is_empty());
        assert!(split_sentences("   ").is_empty());
    }
}
