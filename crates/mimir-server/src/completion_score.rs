//! Fuzzy scoring for completion candidates.
//!
//! Wraps [`nucleo_matcher`] (the scorer extracted from helix-editor) to
//! rank `(prefix, candidate)` pairs by subsequence-with-bonus score, then
//! converts that score to an LSP `sort_text` so editors order the popup
//! best-first.
//!
//! Why a separate module: keeps `backend.rs` from growing further, and
//! lets us swap in a different scorer later without touching the LSP
//! routing layer.

use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Build a fresh [`Matcher`] suitable for one completion call.
///
/// `Matcher::new` allocates ~135 KB of working memory; reuse a single
/// matcher across all candidates within one completion request.
pub fn matcher() -> Matcher {
    Matcher::new(Config::DEFAULT)
}

/// Score a single `(prefix, candidate)` pair.
///
/// * Returns `Some(0)` for an empty prefix — every candidate matches with
///   a neutral score so callers don't need an empty-prefix special case.
/// * Returns `Some(score)` for a fuzzy hit (higher = better; ranges roughly
///   0..1500 for typical SV identifiers).
/// * Returns `None` for a non-match.
///
/// Matching is case-insensitive via `Config::DEFAULT`'s smart-case
/// handling — SV is case-sensitive but identifiers are usually lowercase,
/// and editors often type in mixed case.
pub fn score(matcher: &mut Matcher, prefix: &str, candidate: &str) -> Option<u32> {
    if prefix.is_empty() {
        return Some(0);
    }
    let mut hay_buf = Vec::new();
    let mut needle_buf = Vec::new();
    let haystack = Utf32Str::new(candidate, &mut hay_buf);
    let needle = Utf32Str::new(prefix, &mut needle_buf);
    matcher.fuzzy_match(haystack, needle).map(u32::from)
}

/// Convert a numeric score to an LSP `sort_text` value.
///
/// LSP has no numeric `sort_text`; clients sort items lexicographically by
/// the string. To get descending-by-score we map `score` to
/// `u32::MAX - score` and zero-pad to a fixed 8-hex-digit width so the
/// strings compare in the right order.
pub fn assign_sort_text(score: u32) -> String {
    format!("{:08x}", u32::MAX - score)
}

/// Tunable boost for same-file matches over cross-file ones, applied as
/// `score + SAME_FILE_BOOST`. Picked larger than the typical fuzzy score
/// range (~1500 for short identifiers) so a same-file fuzzy hit always
/// beats a cross-file exact hit, matching today's "your file wins" UX.
pub const SAME_FILE_BOOST: u32 = 10_000;

/// Tunable demotion for keyword candidates so user symbols sort first.
/// Applied as `score / KEYWORD_DIVIDE` then no boost — even a perfect
/// keyword score lands well below any user symbol.
pub const KEYWORD_DIVIDE: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prefix_matches_everything_with_neutral_score() {
        let mut m = matcher();
        assert_eq!(score(&mut m, "", "anything"), Some(0));
        assert_eq!(score(&mut m, "", "my_class"), Some(0));
    }

    #[test]
    fn exact_prefix_beats_subsequence() {
        let mut m = matcher();
        let prefix_score = score(&mut m, "clas", "class").expect("class matches");
        let subseq_score = score(&mut m, "clas", "my_class").expect("my_class matches");
        assert!(
            prefix_score > subseq_score,
            "expected prefix score ({prefix_score}) > subseq score ({subseq_score})",
        );
    }

    #[test]
    fn non_match_returns_none() {
        let mut m = matcher();
        assert!(score(&mut m, "xyz", "module").is_none());
    }

    #[test]
    fn subsequence_matches_when_chars_in_order() {
        // `cls` is a subsequence of `my_class` (c…l…s) and of `class` too.
        let mut m = matcher();
        assert!(score(&mut m, "cls", "class").is_some());
        assert!(score(&mut m, "cls", "my_class").is_some());
        // Out-of-order chars don't match.
        assert!(score(&mut m, "slc", "class").is_none());
    }

    #[test]
    fn sort_text_is_descending() {
        let high = assign_sort_text(1000);
        let low = assign_sort_text(10);
        assert!(
            high < low,
            "higher score must produce lexicographically earlier sort_text: high={high}, low={low}",
        );
    }

    #[test]
    fn sort_text_is_fixed_width() {
        assert_eq!(assign_sort_text(0).len(), 8);
        assert_eq!(assign_sort_text(u32::MAX).len(), 8);
        assert_eq!(assign_sort_text(12345).len(), 8);
    }
}
