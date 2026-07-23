//! PURE: normalizes ABS/Hardcover series names and picks a single confident
//! match between an ABS series and Hardcover's search candidates, per
//! PLAN.md section 5 step 3. Duplicate same-name candidates (common in
//! Hardcover's catalog) are resolved by a unique highest readers_count;
//! anything less confident is left for the dashboard's unmatched queue.
//! Consumed by `checker.rs`, which calls `pick_series_match` after fetching
//! candidates from `MetadataSource::find_series`.

use crate::metadata::SeriesCandidate;
use unicode_normalization::UnicodeNormalization;

/// Leading articles stripped from the front of a normalized name.
const LEADING_ARTICLES: [&str; 3] = ["the", "a", "an"];

/// Normalizes a series name for comparison: Unicode NFKC (so composed and
/// decomposed forms of the same accented text compare equal), lowercased,
/// punctuation stripped, whitespace collapsed, then a leading article
/// ("the"/"a"/"an") and a trailing "series" word are dropped.
pub fn normalize_series_name(name: &str) -> String {
    let nfkc: String = name.nfkc().collect();
    let lower = nfkc.to_lowercase();

    // Apostrophes are dropped entirely (no space), so "Hitchhiker's" becomes
    // "hitchhikers" rather than "hitchhiker s". Every other non-alphanumeric,
    // non-whitespace character becomes a space so word boundaries survive
    // (e.g. "Guide: Foo" -> "guide foo", not "guidefoo").
    let mut cleaned = String::with_capacity(lower.len());
    for ch in lower.chars() {
        if ch == '\'' || ch == '\u{2019}' {
            continue;
        }
        if ch.is_alphanumeric() || ch.is_whitespace() {
            cleaned.push(ch);
        } else {
            cleaned.push(' ');
        }
    }

    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let words = strip_leading_article(&words);
    let words = strip_trailing_series(words);

    words.join(" ")
}

/// Drops a single leading "the"/"a"/"an" word, unless it's the whole name.
fn strip_leading_article<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    match words {
        [first, rest @ ..] if !rest.is_empty() && LEADING_ARTICLES.contains(first) => rest,
        _ => words,
    }
}

/// Drops a single trailing "series" word, unless it's the whole name.
fn strip_trailing_series<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    match words {
        [rest @ .., last] if !rest.is_empty() && *last == "series" => rest,
        _ => words,
    }
}

/// Result of comparing an ABS series name against a set of Hardcover
/// candidates. Only ever `Matched` when exactly one candidate normalizes to
/// the same name as the ABS series; anything else is left for manual review.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchOutcome<'a> {
    Matched(&'a SeriesCandidate),
    Ambiguous,
    NoMatch,
}

/// Picks a series match by normalized-name equality. Hardcover's catalog is
/// full of user-created duplicates ("Harry Potter", "Harry Potter ",
/// "Harry Potter,"), so when several candidates normalize the same the one
/// with the strictly highest `readers_count` wins; a genuine tie (including
/// all-zero counts) is still `Ambiguous`. `Ambiguous` and `NoMatch` are both
/// dashboard-review outcomes.
pub fn pick_series_match<'a>(
    abs_name: &str,
    candidates: &'a [SeriesCandidate],
) -> MatchOutcome<'a> {
    let target = normalize_series_name(abs_name);
    let matches: Vec<&SeriesCandidate> = candidates
        .iter()
        .filter(|c| normalize_series_name(&c.name) == target)
        .collect();

    match matches.as_slice() {
        [] => MatchOutcome::NoMatch,
        [only] => MatchOutcome::Matched(only),
        several => {
            let max = several.iter().map(|c| c.readers_count).max().unwrap_or(0);
            let mut at_max = several.iter().filter(|c| c.readers_count == max);
            match (max, at_max.next(), at_max.next()) {
                // A unique, non-zero leader among the duplicates is a real
                // match; zero readers everywhere means we know nothing.
                (1.., Some(leader), None) => MatchOutcome::Matched(leader),
                _ => MatchOutcome::Ambiguous,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_article_and_case() {
        assert_eq!(normalize_series_name("The Expanse"), "expanse");
        assert_eq!(normalize_series_name("expanse"), "expanse");
        assert_eq!(
            normalize_series_name("A Song of Ice and Fire"),
            "song of ice and fire"
        );
        assert_eq!(
            normalize_series_name("An Unexpected Journey"),
            "unexpected journey"
        );
    }

    #[test]
    fn strips_trailing_series_word() {
        assert_eq!(
            normalize_series_name("The Stormlight Archive Series"),
            "stormlight archive"
        );
        assert_eq!(
            normalize_series_name("Stormlight Archive"),
            "stormlight archive"
        );
    }

    #[test]
    fn strips_punctuation_without_splitting_contractions() {
        assert_eq!(
            normalize_series_name("The Hitchhiker's Guide to the Galaxy"),
            "hitchhikers guide to the galaxy"
        );
        assert_eq!(normalize_series_name("Book: A Novel!"), "book a novel");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(normalize_series_name("  The   Expanse  "), "expanse");
        assert_eq!(normalize_series_name("Foo\tBar\nBaz"), "foo bar baz");
    }

    #[test]
    fn nfkc_folds_composed_and_decomposed_accents_the_same() {
        let composed = "Caf\u{00e9} Series"; // é as a single codepoint
        let decomposed = "Cafe\u{0301} Series"; // e + combining acute accent
        assert_eq!(
            normalize_series_name(composed),
            normalize_series_name(decomposed)
        );
    }

    #[test]
    fn a_name_that_is_only_an_article_or_only_series_is_left_alone() {
        // Stripping would leave nothing; the whole-word guard keeps it intact.
        assert_eq!(normalize_series_name("The"), "the");
        assert_eq!(normalize_series_name("Series"), "series");
    }

    fn candidate(id: &str, name: &str) -> SeriesCandidate {
        candidate_with_readers(id, name, 0)
    }

    fn candidate_with_readers(id: &str, name: &str, readers_count: u64) -> SeriesCandidate {
        SeriesCandidate {
            source_id: id.to_string(),
            name: name.to_string(),
            slug: None,
            is_completed: false,
            readers_count,
        }
    }

    #[test]
    fn exact_normalized_match_wins_among_multiple_candidates() {
        let candidates = vec![
            candidate("hc-1", "Something Else"),
            candidate("hc-2", "The Expanse"),
            candidate("hc-3", "Expanse Companion"),
        ];

        assert_eq!(
            pick_series_match("Expanse", &candidates),
            MatchOutcome::Matched(&candidates[1])
        );
    }

    #[test]
    fn single_survivor_is_matched_even_without_exact_string_equality() {
        let candidates = vec![candidate("hc-1", "the expanse series")];

        assert_eq!(
            pick_series_match("Expanse", &candidates),
            MatchOutcome::Matched(&candidates[0])
        );
    }

    #[test]
    fn zero_candidates_is_no_match() {
        assert_eq!(pick_series_match("Expanse", &[]), MatchOutcome::NoMatch);

        let candidates = vec![candidate("hc-1", "Completely Unrelated")];
        assert_eq!(
            pick_series_match("Expanse", &candidates),
            MatchOutcome::NoMatch
        );
    }

    #[test]
    fn duplicate_names_resolved_by_unique_highest_readers_count() {
        // Mirrors Hardcover's real duplicate rows for "Harry Potter": one
        // canonical series with tens of thousands of readers plus junk
        // near-duplicates ("Harry Potter ", "Harry Potter,") with ~0 readers.
        let candidates = vec![
            candidate_with_readers("hc-1185", "Harry Potter", 80311),
            candidate_with_readers("hc-286585", "Harry Potter ", 1),
            candidate_with_readers("hc-276347", "Harry Potter,", 0),
        ];

        assert_eq!(
            pick_series_match("Harry Potter", &candidates),
            MatchOutcome::Matched(&candidates[0])
        );
    }

    #[test]
    fn duplicate_names_with_tied_readers_count_stay_ambiguous() {
        let tied = vec![
            candidate_with_readers("hc-1", "The Expanse", 50),
            candidate_with_readers("hc-2", "Expanse", 50),
        ];
        assert_eq!(pick_series_match("Expanse", &tied), MatchOutcome::Ambiguous);
    }

    #[test]
    fn duplicate_names_with_all_zero_readers_stay_ambiguous() {
        // Zero readers everywhere carries no signal; never guess.
        let candidates = vec![
            candidate("hc-1", "The Expanse"),
            candidate("hc-2", "Expanse"),
        ];

        assert_eq!(
            pick_series_match("Expanse", &candidates),
            MatchOutcome::Ambiguous
        );
    }
}
