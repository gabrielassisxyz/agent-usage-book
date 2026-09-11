//! The model table: which vendor and which priced model an event's stored model
//! id is valued against (`aub-28py`).
//!
//! The vendor of a usage event is a property of the model, never of the harness
//! that recorded it. One harness runs models from several vendors: the pi harness
//! records litellm aliases (`deepseek-v4-pro-high-k1`) that reach Ollama Cloud,
//! and deriving `ollama` from the string `pi` is not possible at all. So the
//! resolution runs over the stored model id, through a configured table of glob
//! patterns, and the harness is not consulted.
//!
//! The alias also encodes things that do not change the price: the reasoning
//! effort (`-high`, `-max`, `-xhigh`) and which upstream account was used
//! (`-k1`, `-k2`, `-k3`). A glob collapses those onto the one id the rate book
//! carries, which is why the table maps a pattern to a model rather than
//! renaming ids one by one.
//!
//! **Order is the whole design, and it is why this is an array rather than a
//! keyed table.** `glm-5.3-flash*` and `glm-5.3*` both match
//! `glm-5.3-flash-max-k2`, and only the first is right. TOML guarantees no order
//! among the keys of a table, and the parser this crate uses backs a table with a
//! `BTreeMap`, so a keyed section would have sorted `"glm-5.3*"` ahead of
//! `"glm-5.3-flash*"` (`*` is 0x2A, `-` is 0x2D) and priced every flash event at
//! the full model's rate. An array of tables carries its order in the format
//! itself, the way `[[accounts]]` and `[[transcripts]]` already do here.
//!
//! An id that matches no pattern and no built-in default is `Unmapped`, which is
//! a result rather than an absence: the spend footer counts those events and
//! names their ids, so a new alias is visible the day it first appears instead of
//! being valued against whatever card happened to sort first.

use crate::domain::glob::{glob_match, glob_match_chars};
use crate::error::Error;

/// One `[[models]]` entry: a glob over the stored model id, and the vendor and
/// model that matching events are priced as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRule {
    pattern: String,
    /// The pattern's characters, kept so a match does not re-split the string
    /// once per event.
    pattern_chars: Vec<char>,
    vendor: String,
    model: String,
}

impl ModelRule {
    /// Builds one rule, rejecting an empty pattern, vendor or model. All three
    /// are load-bearing: a rule with an empty vendor prices nothing and would
    /// read as a mapped event that no rate card can reach.
    pub fn new(
        pattern: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, Error> {
        let pattern = pattern.into();
        let vendor = vendor.into();
        let model = model.into();
        if pattern.is_empty() {
            return Err(Error::Usage("models[]: empty pattern".into()));
        }
        if vendor.is_empty() {
            return Err(Error::Usage(format!(
                "models[]: empty vendor for pattern {pattern:?}"
            )));
        }
        if model.is_empty() {
            return Err(Error::Usage(format!(
                "models[]: empty model for pattern {pattern:?}"
            )));
        }
        Ok(Self {
            pattern_chars: pattern.chars().collect(),
            pattern,
            vendor,
            model,
        })
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// What a stored model id is priced as.
///
/// `Unmapped` is one of the two answers rather than the absence of an answer: an
/// event nobody can price is reported as such, never valued at nothing and never
/// matched against an arbitrary card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricedModel {
    Mapped { vendor: String, model: String },
    Unmapped,
}

/// The configured rules in file order, plus the built-in defaults every table
/// falls through to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelTable {
    rules: Vec<ModelRule>,
}

/// The vendors the id itself identifies, applied after the configured table so
/// an operator can always override one. The id is carried through unchanged:
/// these three vendors publish rates under the same string the transcript
/// stores, so there is nothing to rewrite.
///
/// `opencode/*` and `opencode-go/*` are two distinct upstreams that both store
/// `<providerID>/<modelID>`, so the prefix is the whole of the vendor evidence.
const BUILT_IN_VENDORS: &[(&str, &str)] = &[
    ("claude*", "anthropic"),
    ("gpt*", "openai"),
    ("o?", "openai"),
    ("o?-*", "openai"),
    ("opencode/*", "opencode"),
    ("opencode-go/*", "opencode"),
];

impl ModelTable {
    /// Builds a table from its rules in file order, rejecting a repeated pattern
    /// and a pattern an earlier one already covers entirely. Both are dead
    /// config: the later rule can never decide anything, and a rule that never
    /// fires is indistinguishable from one whose vendor is wrong.
    pub fn new(rules: Vec<ModelRule>) -> Result<Self, Error> {
        for (index, rule) in rules.iter().enumerate() {
            for earlier in &rules[..index] {
                if earlier.pattern == rule.pattern {
                    return Err(Error::Usage(format!(
                        "models[]: pattern {:?} is listed twice; the second entry can never match",
                        rule.pattern
                    )));
                }
                if shadows(&earlier.pattern, &rule.pattern) {
                    return Err(Error::Usage(format!(
                        "models[]: pattern {:?} can never match because the earlier pattern {:?} \
                         already covers it; list the more specific pattern first",
                        rule.pattern, earlier.pattern
                    )));
                }
            }
        }
        Ok(Self { rules })
    }

    /// The vendor and priced model for a stored id: the first configured pattern
    /// that matches, then the built-in vendors, then `Unmapped`.
    pub fn resolve(&self, model_id: &str) -> PricedModel {
        if model_id.is_empty() {
            return PricedModel::Unmapped;
        }
        let id: Vec<char> = model_id.chars().collect();
        for rule in &self.rules {
            if glob_match_chars(&rule.pattern_chars, &id) {
                return PricedModel::Mapped {
                    vendor: rule.vendor.clone(),
                    model: rule.model.clone(),
                };
            }
        }
        for (pattern, vendor) in BUILT_IN_VENDORS {
            if glob_match(pattern, model_id) {
                return PricedModel::Mapped {
                    vendor: (*vendor).to_string(),
                    model: model_id.to_string(),
                };
            }
        }
        PricedModel::Unmapped
    }

    /// Every configured rule, in the order the file listed them.
    pub fn rules(&self) -> &[ModelRule] {
        &self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Whether every id `later` can match is already matched by `earlier`.
///
/// Decided only for the case it can be decided in: `earlier` is a plain prefix
/// glob, so everything it matches is "starts with this literal", and `later`
/// cannot produce a match that does not start with its own literal head. Any
/// harder overlap is left alone rather than guessed at, because a validator that
/// rejects a pattern which could in fact fire is worse than one that misses a
/// dead one.
fn shadows(earlier: &str, later: &str) -> bool {
    let Some(prefix) = prefix_glob(earlier) else {
        return false;
    };
    literal_head(later).starts_with(prefix)
}

/// The literal of a pattern shaped `literal*`, or `None` when the pattern
/// carries any other metacharacter.
fn prefix_glob(pattern: &str) -> Option<&str> {
    let literal = pattern.strip_suffix('*')?;
    (!literal.contains(['*', '?'])).then_some(literal)
}

/// The part of a pattern before its first metacharacter, which every string it
/// matches begins with.
fn literal_head(pattern: &str) -> &str {
    match pattern.find(['*', '?']) {
        Some(at) => &pattern[..at],
        None => pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example table from `aub-28py`, in the order it has to be written in.
    fn example_table() -> ModelTable {
        ModelTable::new(vec![
            ModelRule::new("deepseek-v4-pro*", "ollama", "deepseek-v4-pro").unwrap(),
            ModelRule::new("deepseek-v4-flash*", "ollama", "deepseek-v4-flash").unwrap(),
            ModelRule::new("glm-5.3-flash*", "ollama", "glm-5.3-flash").unwrap(),
            ModelRule::new("glm-5.3*", "ollama", "glm-5.3").unwrap(),
            ModelRule::new("glm-5.2*", "ollama", "glm-5.2").unwrap(),
            ModelRule::new("kimi-k3*", "ollama", "kimi-k3").unwrap(),
            ModelRule::new("kimi-k2.7*", "ollama", "kimi-k2.7-code").unwrap(),
            ModelRule::new("minimax-m3*", "ollama", "minimax-m3").unwrap(),
        ])
        .unwrap()
    }

    fn mapped(vendor: &str, model: &str) -> PricedModel {
        PricedModel::Mapped {
            vendor: vendor.to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn resolver_table_covers_every_id_this_machine_records() {
        let table = example_table();
        let cases: &[(&str, PricedModel)] = &[
            (
                "deepseek-v4-pro-high-k1",
                mapped("ollama", "deepseek-v4-pro"),
            ),
            (
                "deepseek-v4-flash-max-k2",
                mapped("ollama", "deepseek-v4-flash"),
            ),
            ("glm-5.3-flash-max-k2", mapped("ollama", "glm-5.3-flash")),
            ("glm-5.3-xhigh-k3", mapped("ollama", "glm-5.3")),
            ("glm-5.2-high-k1", mapped("ollama", "glm-5.2")),
            ("kimi-k3-high-k2", mapped("ollama", "kimi-k3")),
            ("kimi-k2.7", mapped("ollama", "kimi-k2.7-code")),
            ("minimax-m3-max-k3", mapped("ollama", "minimax-m3")),
            ("claude-opus-5", mapped("anthropic", "claude-opus-5")),
            ("gpt-5.6-terra", mapped("openai", "gpt-5.6-terra")),
            ("o3-mini", mapped("openai", "o3-mini")),
            (
                "opencode-go/muse-spark-1.3-contributor",
                mapped("opencode", "opencode-go/muse-spark-1.3-contributor"),
            ),
            (
                "opencode/muse-spark-1.3-contributor-free",
                mapped("opencode", "opencode/muse-spark-1.3-contributor-free"),
            ),
            ("<synthetic>", PricedModel::Unmapped),
            ("", PricedModel::Unmapped),
        ];
        for (id, expected) in cases {
            assert_eq!(&table.resolve(id), expected, "resolving {id:?}");
        }
    }

    /// The planted negative for the ordering rule. This id matches both
    /// `glm-5.3-flash*` and `glm-5.3*`, so an implementation that consults the
    /// rules in any order but the file's, or that keys them in a sorted map,
    /// prices it as the full model. The positive above and this case differ only
    /// in which of the two rules is listed first.
    #[test]
    fn a_later_general_pattern_never_wins_over_an_earlier_specific_one() {
        let specific_first = ModelTable::new(vec![
            ModelRule::new("glm-5.3-flash*", "ollama", "glm-5.3-flash").unwrap(),
            ModelRule::new("glm-5.3*", "ollama", "glm-5.3").unwrap(),
        ])
        .unwrap();
        assert_eq!(
            specific_first.resolve("glm-5.3-flash-max-k2"),
            mapped("ollama", "glm-5.3-flash")
        );
    }

    #[test]
    fn a_general_pattern_listed_first_shadows_the_specific_one_and_is_rejected() {
        let error = ModelTable::new(vec![
            ModelRule::new("glm-5.3*", "ollama", "glm-5.3").unwrap(),
            ModelRule::new("glm-5.3-flash*", "ollama", "glm-5.3-flash").unwrap(),
        ])
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("glm-5.3-flash*"), "{message}");
        assert!(message.contains("glm-5.3*"), "{message}");
    }

    #[test]
    fn a_repeated_pattern_is_rejected() {
        let error = ModelTable::new(vec![
            ModelRule::new("kimi-k2.7", "ollama", "kimi-k2.7-code").unwrap(),
            ModelRule::new("kimi-k2.7", "ollama", "kimi-k3").unwrap(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("listed twice"));
    }

    #[test]
    fn an_empty_pattern_vendor_or_model_is_rejected() {
        assert!(ModelRule::new("", "ollama", "glm-5.3").is_err());
        assert!(ModelRule::new("glm-5.3*", "", "glm-5.3").is_err());
        assert!(ModelRule::new("glm-5.3*", "ollama", "").is_err());
    }

    /// A configured rule outranks a built-in, so an operator whose `claude`
    /// traffic is proxied somewhere else is not overruled by the id's prefix.
    #[test]
    fn a_configured_pattern_outranks_a_built_in_vendor() {
        let table = ModelTable::new(vec![
            ModelRule::new("claude-opus-5*", "proxy", "claude-opus-5").unwrap(),
        ])
        .unwrap();
        assert_eq!(
            table.resolve("claude-opus-5"),
            mapped("proxy", "claude-opus-5")
        );
        assert_eq!(
            table.resolve("claude-sonnet-5"),
            mapped("anthropic", "claude-sonnet-5")
        );
    }

    /// `?` is one character and never zero, which is what separates the `o?-*`
    /// default from a bare prefix on `o`.
    #[test]
    fn the_single_character_wildcard_matches_exactly_one() {
        let table = ModelTable::default();
        assert_eq!(table.resolve("o3"), mapped("openai", "o3"));
        assert_eq!(table.resolve("o3-mini"), mapped("openai", "o3-mini"));
        assert_eq!(table.resolve("ollama-something"), PricedModel::Unmapped);
    }

    #[test]
    fn resolution_is_stable_across_two_calls() {
        let table = example_table();
        for id in [
            "glm-5.3-flash-max-k2",
            "kimi-k2.7",
            "gpt-5.6-terra",
            "<synthetic>",
        ] {
            assert_eq!(table.resolve(id), table.resolve(id), "resolving {id:?}");
        }
    }

    /// The ids this machine records, as prefixes a generated suffix is hung off,
    /// so the generated cases actually reach the table instead of missing every
    /// pattern and making the property vacuous.
    const ID_PREFIXES: [&str; 14] = [
        "deepseek-v4-pro",
        "deepseek-v4-flash",
        "glm-5.3-flash",
        "glm-5.3",
        "glm-5.2",
        "kimi-k3",
        "kimi-k2.7",
        "minimax-m3",
        "claude-opus-5",
        "gpt-5.6",
        "o3",
        "opencode/muse",
        "<synthetic>",
        "",
    ];

    proptest::proptest! {
        /// For any id, the rule reported is the first one that matches and no
        /// other, and when none matches the answer is exactly what the built-in
        /// defaults alone would have said. Resolving twice gives the same answer.
        #[test]
        fn prop_the_first_matching_rule_is_the_only_one_reported(
            prefix in proptest::sample::select(ID_PREFIXES.to_vec()),
            suffix in "[a-z0-9.<>/-]{0,12}",
        ) {
            let id = format!("{prefix}{suffix}");
            let table = example_table();
            let resolved = table.resolve(&id);
            let first = table
                .rules()
                .iter()
                .find(|rule| glob_match(rule.pattern(), &id));
            match first {
                Some(rule) => proptest::prop_assert_eq!(
                    &resolved,
                    &PricedModel::Mapped {
                        vendor: rule.vendor().to_string(),
                        model: rule.model().to_string(),
                    },
                    "id {:?} must be priced by the first pattern that matches it",
                    id
                ),
                None => proptest::prop_assert_eq!(
                    &resolved,
                    &ModelTable::default().resolve(&id),
                    "no configured rule matches {:?}, so only the built-ins may decide it",
                    id
                ),
            }
            proptest::prop_assert_eq!(&resolved, &table.resolve(&id));
        }
    }

    /// A pattern that starts with a star still resolves, which is the case the
    /// prefix-glob shadowing check deliberately declines to reason about.
    #[test]
    fn a_pattern_that_is_not_a_prefix_glob_still_matches() {
        let table = ModelTable::new(vec![
            ModelRule::new("*-k1", "ollama", "whatever-runs-on-k1").unwrap(),
        ])
        .unwrap();
        assert_eq!(
            table.resolve("deepseek-v4-pro-high-k1"),
            mapped("ollama", "whatever-runs-on-k1")
        );
    }
}
