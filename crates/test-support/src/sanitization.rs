//! The shared forbidden-pattern list for transcript fixtures.
//!
//! Fixtures are committed, so they are publishable text, and a scan over the
//! whole fixture directory is what enforces that rather than the care of
//! whoever captured them. The list lives here, once, so the corpus audit and
//! any capture-time scan read the same patterns instead of restating them.
//!
//! The list is deliberately concrete: credential key prefixes and credential
//! field labels, never bare words like "token" (token counts are the content
//! of every fixture). Matching is case-insensitive, so `Authorization` and
//! `authorization` are both caught.

/// Credential-shaped substrings that must never appear in a committed fixture.
pub const FORBIDDEN_PATTERNS: &[&str] = &[
    "sk-ant-",       // Anthropic API key prefix
    "sk-proj-",      // OpenAI project-scoped key prefix
    "sk-or-",        // OpenAI organization key prefix
    "ghp_",          // GitHub personal access token prefix
    "github_pat_",   // GitHub fine-grained token prefix
    "glpat-",        // GitLab personal access token prefix
    "AKIA",          // AWS access key id prefix
    "xoxb-",         // Slack bot token prefix
    "xoxp-",         // Slack user token prefix
    "-----BEGIN",    // private key block marker
    "bearer ",       // authorization header value
    "api_key",       // credential field name or label
    "apikey",        // credential field name or label
    "api-key",       // credential field name or label
    "authorization", // credential field name or label
    "password",      // credential field name
    "secret",        // credential field name
];

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
}
