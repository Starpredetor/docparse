//! Text cleanup, scoped to a single block.
//!
//! Deliberately *not* document-wide: collapsing whitespace across an
//! entire document destroys the block and page structure the backends
//! worked to preserve.

/// Collapses internal whitespace runs to single spaces, maps common
/// typographic characters to ASCII, drops control characters, and trims
/// both ends. Block boundaries are the caller's business and are never
/// crossed here.
pub fn normalize_block_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;

    for ch in input.chars() {
        let replacement: &str = match ch {
            '\u{201C}' | '\u{201D}' | '\u{201E}' => "\"",
            '\u{2018}' | '\u{2019}' | '\u{201A}' => "'",
            '\u{2013}' | '\u{2014}' | '\u{2212}' => "-",
            '\u{2026}' => "...",
            // No arm for U+00A0, U+2007 or U+202F: `char::is_whitespace` is already
            // true for them, so the default branch below collapses them to a single
            // space. An explicit mapping here would be dead code that reads as though
            // it were load-bearing.
            _ => {
                if ch.is_whitespace() {
                    // Remember it, but only emit one space, and only if
                    // something follows. This trims both ends for free.
                    pending_space = true;
                    continue;
                }
                if ch.is_control() {
                    continue;
                }
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push(ch);
                continue;
            }
        };

        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push_str(replacement);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_typographic_characters_to_ascii() {
        assert_eq!(
            normalize_block_text("\u{201C}quoted\u{201D} and \u{2018}single\u{2019}"),
            "\"quoted\" and 'single'"
        );
        assert_eq!(
            normalize_block_text("en\u{2013}dash em\u{2014}dash minus\u{2212}sign"),
            "en-dash em-dash minus-sign"
        );
        assert_eq!(normalize_block_text("wait\u{2026}"), "wait...");
        assert_eq!(normalize_block_text("a\u{2026}b"), "a...b");
        assert_eq!(normalize_block_text("a \u{2026} b"), "a ... b");
    }

    #[test]
    fn collapses_whitespace_runs_and_trims_the_edges() {
        assert_eq!(
            normalize_block_text("  lots   of \t\t space \n\n here  "),
            "lots of space here"
        );
    }

    #[test]
    fn treats_a_non_breaking_space_as_a_space() {
        assert_eq!(normalize_block_text("a\u{00a0}b"), "a b");
    }

    #[test]
    fn drops_control_characters_without_eating_their_neighbours() {
        assert_eq!(normalize_block_text("a\u{0008}b\u{0000}c"), "abc");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let input = "A perfectly normal sentence, with punctuation!";
        assert_eq!(normalize_block_text(input), input);
    }

    #[test]
    fn empty_and_whitespace_only_input_yield_empty_output() {
        assert_eq!(normalize_block_text(""), "");
        assert_eq!(normalize_block_text("   \n\t "), "");
    }

    #[test]
    fn preserves_non_ascii_letters() {
        // Normalization is about layout artifacts, not about stripping
        // languages. The Python version's NFKD pass mangles accented text.
        assert_eq!(normalize_block_text("café naïve Omega"), "café naïve Omega");
    }
}
