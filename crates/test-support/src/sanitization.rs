//! The shared forbidden-pattern list for transcript fixtures, the release
//! binary and persisted meter evidence (aub-n27.4).
//!
//! Fixtures are committed, so they are publishable text, and a scan over the
//! whole fixture directory is what enforces that rather than the care of
//! whoever captured them. A release binary and a sampled database are not
//! committed, but the same leak (a credential, a home path, an account
//! identifier) is exactly as unacceptable in either. The pattern list lives
//! in `docs/forbidden-patterns.txt`, once, so the corpus audit, the
//! release-binary scan and the sampling-run scan all read the same patterns
//! instead of each restating them.
//!
//! The list is deliberately concrete: credential key prefixes, credential
//! field labels, absolute home paths and account-identifier shapes, never a
//! bare word like "token" (token counts are the content of every fixture).
//! Matching is case-insensitive, so `Authorization` and `authorization` are
//! both caught.

use std::sync::LazyLock;

/// The documented, single-source pattern file every scan reads.
const FORBIDDEN_PATTERNS_SOURCE: &str = include_str!("../../../docs/forbidden-patterns.txt");

/// Credential-shaped and identity-shaped substrings that must never appear in
/// a committed fixture, a release binary, or persisted evidence.
pub static FORBIDDEN_PATTERNS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| parse_forbidden_patterns(FORBIDDEN_PATTERNS_SOURCE));

/// Parses the forbidden-pattern file format: one pattern per line, verbatim
/// (a pattern may carry meaningful trailing whitespace, so lines are never
/// trimmed), with blank lines and lines starting with `#` ignored as
/// comments. Panics on an empty result: a list edited down to nothing would
/// make every caller's scan pass vacuously instead of failing loudly.
fn parse_forbidden_patterns(source: &'static str) -> Vec<&'static str> {
    let patterns: Vec<&'static str> = source
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        !patterns.is_empty(),
        "the forbidden-pattern list is empty: every scan that reads it would pass vacuously"
    );
    patterns
}

/// Every forbidden pattern present in `text`, in list order. An empty result
/// means the text is clean.
pub fn matched_patterns(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    FORBIDDEN_PATTERNS
        .iter()
        .filter(|pattern| lower.contains(&pattern.to_lowercase()))
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_fixture_matches_nothing() {
        let clean = r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        assert!(matched_patterns(clean).is_empty());
    }

    #[test]
    fn a_credential_shaped_value_is_caught() {
        let dirty = r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10}},"api_key":"sk-ant-1234567890"}"#;
        let hits = matched_patterns(dirty);
        assert!(hits.contains(&"sk-ant-"), "hits: {hits:?}");
        assert!(hits.contains(&"api_key"), "hits: {hits:?}");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let dirty = "Authorization: Bearer abcdef";
        let hits = matched_patterns(dirty);
        assert!(hits.contains(&"bearer "), "hits: {hits:?}");
        assert!(hits.contains(&"authorization"), "hits: {hits:?}");
    }

    #[test]
    fn token_counts_are_not_credential_shaped() {
        let fixture = r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":2}}}"#;
        assert!(matched_patterns(fixture).is_empty());
    }

    #[test]
    fn an_absolute_home_path_is_caught() {
        let dirty = r#"{"credential_detail":"/home/example-user/.config/aub/anthropic.json"}"#;
        let hits = matched_patterns(dirty);
        assert!(hits.contains(&"/home/"), "hits: {hits:?}");
    }

    #[test]
    fn an_account_identifier_is_caught() {
        let dirty = r#"{"account":"someone@example.com"}"#;
        let hits = matched_patterns(dirty);
        assert!(hits.contains(&"@"), "hits: {hits:?}");
    }

    #[test]
    fn an_empty_pattern_source_panics_instead_of_passing_vacuously() {
        let result = std::panic::catch_unwind(|| parse_forbidden_patterns(""));
        assert!(
            result.is_err(),
            "an empty forbidden-pattern source must panic, not return an empty list"
        );
    }

    #[test]
    fn a_comment_only_pattern_source_panics_instead_of_passing_vacuously() {
        let result = std::panic::catch_unwind(|| parse_forbidden_patterns("# nothing here\n\n"));
        assert!(
            result.is_err(),
            "a comment-only forbidden-pattern source must panic, not return an empty list"
        );
    }

    #[test]
    fn a_pattern_with_trailing_whitespace_is_preserved_verbatim() {
        let patterns = parse_forbidden_patterns("bearer \nauthorization\n");
        assert_eq!(patterns, vec!["bearer ", "authorization"]);
    }
}
