//! The one glob matcher this crate has: `*` is any run of characters, `?` is
//! exactly one.
//!
//! May not depend on:
//! - anything. It is a string primitive with no vocabulary of its own, and it
//!   lives here because two layers that may not depend on each other both need
//!   it: transcript discovery matches a configured pattern against a file name,
//!   and the model table matches a configured pattern against a stored model id.
//!
//! It is one function rather than two copies because the two callers cannot see
//! each other: a fix applied to one copy would leave the other matching by the
//! old rule, and nothing would report the divergence. What differs between the
//! callers is what they reject before they get here (path separators and
//! character classes for a file name; nothing for a model id), which is each
//! caller's own concern.
//!
//! Character classes, braces and escapes are not implemented, so a caller that
//! could receive one rejects it rather than passing it through: `[a-z]` reaching
//! this function matches the literal characters, which is a silent mis-match.

/// Whether `pattern` matches the whole of `text`.
///
/// Iterative with a single backtrack point, so a pattern carrying several stars
/// stays linear in the common case instead of exploring the exponential tree the
/// naive recursion does.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_chars(&pattern, &text)
}

/// The same match over already-split characters, for a caller holding a pattern
/// it matches against many texts and does not want to re-split each time.
pub fn glob_match_chars(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut resume = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(at) = star {
            p = at + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_pattern_matches_only_itself() {
        assert!(glob_match("claude-opus-5", "claude-opus-5"));
        assert!(!glob_match("claude-opus-5", "claude-opus-4"));
        assert!(!glob_match("claude-opus-5", "claude-opus-5-preview"));
    }

    #[test]
    fn a_star_matches_any_run_including_none() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("a*", "a"));
        assert!(glob_match("a*b", "ab"));
        assert!(glob_match("a*b", "axxxb"));
        assert!(!glob_match("a*b", "axxxc"));
        assert!(glob_match("*.jsonl", "rollout-2026-09-08.jsonl"));
    }

    #[test]
    fn a_question_mark_matches_exactly_one_character_never_zero() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("o?-*", "o3-mini"));
        assert!(!glob_match("o?-*", "o-mini"));
    }

    #[test]
    fn several_stars_backtrack_to_the_match_that_exists() {
        assert!(glob_match("*a*b*c*", "xxaxxbxxcxx"));
        assert!(!glob_match("*a*b*c*", "xxaxxcxxbxx"));
        assert!(glob_match("**", "anything"));
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(!glob_match("glm*", "GLM-5.3"));
        assert!(glob_match("glm*", "glm-5.3"));
    }

    #[test]
    fn the_pre_split_form_agrees_with_the_string_form() {
        for (pattern, text) in [
            ("a*b", "axxxb"),
            ("a*b", "axxxc"),
            ("?", ""),
            ("*", "x"),
            ("glm-5.3-flash*", "glm-5.3-flash-max-k2"),
        ] {
            assert_eq!(
                glob_match(pattern, text),
                glob_match_chars(
                    &pattern.chars().collect::<Vec<_>>(),
                    &text.chars().collect::<Vec<_>>()
                ),
                "pattern {pattern:?} against {text:?}"
            );
        }
    }
}
