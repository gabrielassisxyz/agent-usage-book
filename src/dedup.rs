//! One canonical replay-deduplication implementation.
//!
//! Deduplication happens once, here, after parsing and before anything counts an
//! event, because a per-parser implementation is how two parsers end up with two
//! definitions of the same event. A transcript replays usage: Claude Code writes one
//! line per content block of an assistant message and every line carries the same
//! message identifier, with the output count growing across them, so a consumer that
//! keeps the first occurrence undercounts and one that sums them multiplies.
//!
//! Every event resolves to one of two identity kinds, checked in this order:
//!
//! - **Strong**: a source-provided identifier. A claim the source is making, so it is
//!   the final word: two occurrences sharing one either replay the same message or
//!   collide, and either way they collapse to one canonical event.
//! - **Heuristic**: computed by [`fingerprint::HeuristicKey`] when the source wrote no
//!   identifier. An inference this system is making, scoped to the parser that
//!   produced it so one parser's replay-equivalence domain is never read against
//!   another's, and never even computed for an event that already has a strong
//!   identity: the two domains do not interact, which is what lets a stable
//!   identifier outrank a heuristic one by construction rather than by comparison.
//!
//! Either way the occurrence with the largest output count is the one kept, because
//! the others are earlier snapshots of the same message.
//!
//! The database uniqueness constraint over `(source_namespace, native_event_id)` is
//! the final authority for the strong domain; `(parser_version, heuristic_key)`, scoped
//! per parser, is the final authority for the heuristic domain. Both are enforced by
//! `usage_occurrence` (`crate::store::usage_occurrence`); `aub-lqe.8` adds the table's
//! remaining occurrence metadata once `usage_event` exists to reference.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::collections::BTreeMap;

use crate::transcripts::NormalizedUsageEvent;
use crate::transcripts::parser::ParserVersion;
use fingerprint::HeuristicKey;

pub mod cumulative;

/// The outcome of deduplicating one batch of events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deduplicated {
    /// One event per identity, strong or heuristic, in first-seen order.
    pub canonical: Vec<NormalizedUsageEvent>,
    /// Occurrences that collapsed into an already-seen identity because their input
    /// and cache counts matched it.
    pub replayed_occurrences: u64,
    /// Occurrences that shared a strong identity with a kept event but disagreed on a
    /// count other than output. The kept event is the one with the larger output;
    /// the disagreement is reported, never resolved silently. A heuristic key already
    /// encodes every count, so two occurrences that share one never disagree: this
    /// class exists only for the strong domain.
    pub collisions: u64,
    /// Occurrences that carried no source-provided identifier and were routed
    /// through the heuristic domain instead, whether they started a new canonical
    /// event or collapsed into one already seen. Reports how much of the batch
    /// rests on an inference this system made rather than a claim the source made.
    pub without_identity: u64,
}

/// Collapses replayed occurrences of one event into one canonical event.
pub fn deduplicate(events: Vec<NormalizedUsageEvent>) -> Deduplicated {
    let mut canonical: Vec<NormalizedUsageEvent> = Vec::with_capacity(events.len());
    let mut index_by_strong: BTreeMap<String, usize> = BTreeMap::new();
    let mut index_by_heuristic: BTreeMap<(ParserVersion, HeuristicKey), usize> = BTreeMap::new();
    let mut replayed_occurrences = 0u64;
    let mut collisions = 0u64;
    let mut without_identity = 0u64;

    for event in events {
        if let Some(identity) = event.strong_identity().map(str::to_string) {
            match index_by_strong.get(&identity) {
                None => {
                    index_by_strong.insert(identity, canonical.len());
                    canonical.push(event);
                }
                Some(&kept_at) => {
                    let kept = &canonical[kept_at];
                    if same_message(kept, &event) {
                        replayed_occurrences += 1;
                    } else {
                        collisions += 1;
                    }
                    if output_of(&event) > output_of(kept) {
                        canonical[kept_at] = event;
                    }
                }
            }
            continue;
        }

        without_identity += 1;
        let key = (
            event.parser_version().clone(),
            HeuristicKey::compute(&event),
        );
        match index_by_heuristic.get(&key) {
            None => {
                index_by_heuristic.insert(key, canonical.len());
                canonical.push(event);
            }
            Some(&kept_at) => {
                replayed_occurrences += 1;
                let kept = &canonical[kept_at];
                if output_of(&event) > output_of(kept) {
                    canonical[kept_at] = event;
                }
            }
        }
    }

    Deduplicated {
        canonical,
        replayed_occurrences,
        collisions,
        without_identity,
    }
}

/// Two occurrences are the same message when everything but the output count
/// agrees: input, both cache kinds and the unknown components.
fn same_message(kept: &NormalizedUsageEvent, other: &NormalizedUsageEvent) -> bool {
    let a = kept.usage().known();
    let b = other.usage().known();
    a.input() == b.input()
        && a.cache_read() == b.cache_read()
        && a.cache_write() == b.cache_write()
        && kept.usage().unknown() == other.usage().unknown()
}

fn output_of(event: &NormalizedUsageEvent) -> u64 {
    event.usage().known().output().value()
}

/// The heuristic replay fingerprint: the identity computed for an event that
/// carries no source-provided identifier.
mod fingerprint {
    use crate::transcripts::NormalizedUsageEvent;

    /// A parser-scoped heuristic identity. Meaningful only alongside the parser
    /// version that produced it: two fingerprints from different parsers are
    /// never compared, which is what keeps one parser's replay-equivalence domain
    /// from being read against another's.
    ///
    /// Computed only from fields stable across a replay of the same underlying
    /// record: the record's own timestamp, its session, and its token counts.
    /// Never the source pathname, because the same logical event replayed into a
    /// different file must still fingerprint the same, and never the time this
    /// system ingested the record, which says nothing about what the source
    /// reported. `NormalizedUsageEvent` carries no line number or ingest
    /// timestamp at all, so neither can leak into this computation by
    /// construction; a source that later exposes a stronger replay discriminator
    /// than the record timestamp (a source sequence number, or message ancestry)
    /// takes that discriminator's place ahead of the timestamp, because it can
    /// separate two requests a shared timestamp cannot.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub struct HeuristicKey(String);

    impl HeuristicKey {
        /// Computes the key for an event that carries no strong identity.
        pub fn compute(event: &NormalizedUsageEvent) -> Self {
            let known = event.usage().known();
            let mut key = discriminator(event);
            key.push('|');
            key.push_str(
                event
                    .session()
                    .map(|session| session.native().as_str())
                    .unwrap_or("session:none"),
            );
            // The output count is deliberately excluded: a replayed message grows
            // its output count across lines by construction (module doc), so
            // including it here would fingerprint every replay line as a distinct
            // event, defeating the collapse this key exists to enable. `input`,
            // `cache_read` and `cache_write` do not grow across a replay and stay
            // part of the key.
            key.push('|');
            key.push_str(&format!(
                "{}:{}:{}",
                known.input().value(),
                known.cache_read().value(),
                known.cache_write().value(),
            ));
            for (component, count) in event.usage().unknown() {
                key.push('|');
                key.push_str(component);
                key.push('=');
                key.push_str(&count.value().to_string());
            }
            Self(key)
        }
    }

    /// The strongest replay discriminator this event carries beyond its counts.
    /// Today every native parser reports only a record timestamp, so this always
    /// falls back to it; a parser that later exposes a source sequence number or
    /// message ancestry link would report it here instead, ahead of the
    /// timestamp, because two adjacent requests can legitimately share one
    /// timestamp but never one sequence number.
    fn discriminator(event: &NormalizedUsageEvent) -> String {
        event
            .occurred_at()
            .map(|occurred_at| format!("t:{}", occurred_at.unix_nanos()))
            .unwrap_or_else(|| "t:none".to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
        use crate::domain::time::UtcTimestamp;
        use crate::domain::tokens::{
            CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
            UsageVector,
        };
        use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
        use crate::transcripts::parser::{EvidenceClassification, ParserVersion};
        use std::collections::BTreeMap;

        fn event(file: &str, input: u64, output: u64) -> NormalizedUsageEvent {
            let usage = UsageVector::new(
                KnownTokenVector::new(
                    InputTokens::new(input),
                    OutputTokens::new(output),
                    CacheReadTokens::new(0),
                    CacheWriteTokens::new(0),
                ),
                BTreeMap::new(),
                CoverageCompleteness::Complete,
                EvidenceQuality::Measured,
            );
            NormalizedUsageEvent::new(
                usage,
                EvidenceClassification::Reported,
                Provenance::new(vec![file.to_string()]),
                ParserVersion::new("test-1"),
            )
            .with_occurred_at(UtcTimestamp::from_unix_nanos(1_000_000_000))
            .with_session(SessionId::new(
                SourceNamespace::new("test"),
                NativeSessionId::new("s1"),
            ))
        }

        /// The source pathname never reaches the fingerprint: two events that
        /// differ only in which file carried them fingerprint identically.
        #[test]
        fn the_source_pathname_is_excluded() {
            let a = HeuristicKey::compute(&event("first.jsonl", 10, 5));
            let b = HeuristicKey::compute(&event("second.jsonl", 10, 5));
            assert_eq!(a, b, "the fingerprint must not depend on the source file");
        }

        /// Two independent requests of equal size at different times do not
        /// fingerprint the same: the timestamp is enough semantic context to
        /// keep them apart.
        #[test]
        fn equal_sized_requests_at_different_times_do_not_collide() {
            let a = event("file.jsonl", 10, 5);
            let b = event("file.jsonl", 10, 5)
                .with_occurred_at(UtcTimestamp::from_unix_nanos(2_000_000_000));
            assert_ne!(
                HeuristicKey::compute(&a),
                HeuristicKey::compute(&b),
                "two adjacent equal-sized requests must not collapse"
            );
        }

        /// A real difference in the input count is enough semantic context to
        /// tell two occurrences apart, even at the same timestamp.
        #[test]
        fn different_input_counts_at_the_same_time_do_not_collide() {
            let a = event("file.jsonl", 10, 5);
            let b = event("file.jsonl", 11, 5);
            assert_ne!(HeuristicKey::compute(&a), HeuristicKey::compute(&b));
        }

        /// The output count is excluded on purpose: a replayed message grows its
        /// output count across lines, so two occurrences differing only in
        /// output must fingerprint the same in order to collapse.
        #[test]
        fn a_growing_output_count_alone_does_not_change_the_fingerprint() {
            let a = event("file.jsonl", 10, 5);
            let b = event("file.jsonl", 10, 913);
            assert_eq!(HeuristicKey::compute(&a), HeuristicKey::compute(&b));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
    use crate::domain::time::UtcTimestamp;
    use crate::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
    };
    use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
    use crate::transcripts::parser::{
        EvidenceClassification, ParserVersion, STRONG_IDENTITY_PREFIX,
    };
    use proptest::prelude::*;

    fn event(id: Option<&str>, input: u64, output: u64, cache_read: u64) -> NormalizedUsageEvent {
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(cache_read),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        let mut sources = vec!["file.jsonl".to_string()];
        if let Some(id) = id {
            sources.push(format!("{STRONG_IDENTITY_PREFIX}{id}"));
        }
        NormalizedUsageEvent::new(
            usage,
            EvidenceClassification::Reported,
            Provenance::new(sources),
            ParserVersion::new("test-1"),
        )
    }

    /// A no-identity event with a caller-chosen source file, timestamp and
    /// session, so heuristic-domain tests can control what the fingerprint sees.
    fn heuristic_event(
        file: &str,
        occurred_at_nanos: i64,
        session: &str,
        input: u64,
        output: u64,
    ) -> NormalizedUsageEvent {
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        NormalizedUsageEvent::new(
            usage,
            EvidenceClassification::Reported,
            Provenance::new(vec![file.to_string()]),
            ParserVersion::new("test-1"),
        )
        .with_occurred_at(UtcTimestamp::from_unix_nanos(occurred_at_nanos))
        .with_session(SessionId::new(
            SourceNamespace::new("test"),
            NativeSessionId::new(session),
        ))
    }

    /// Three lines of one message with a growing output count collapse to the
    /// last snapshot, and the two earlier ones are replays.
    #[test]
    fn replays_of_one_identity_keep_the_largest_output() {
        let result = deduplicate(vec![
            event(Some("m1"), 2, 100, 500),
            event(Some("m1"), 2, 400, 500),
            event(Some("m1"), 2, 913, 500),
        ]);
        assert_eq!(result.canonical.len(), 1);
        assert_eq!(result.canonical[0].usage().known().output().value(), 913);
        assert_eq!(result.replayed_occurrences, 2);
        assert_eq!(result.collisions, 0);
    }

    /// The planted negative: two events with different identities never collapse,
    /// even when every count is identical.
    #[test]
    fn different_identities_never_collapse() {
        let result = deduplicate(vec![
            event(Some("m1"), 10, 5, 0),
            event(Some("m2"), 10, 5, 0),
        ]);
        assert_eq!(result.canonical.len(), 2);
        assert_eq!(result.replayed_occurrences, 0);
    }

    /// Occurrences arriving out of order still keep the largest output.
    #[test]
    fn the_largest_output_wins_regardless_of_order() {
        let result = deduplicate(vec![
            event(Some("m1"), 2, 913, 500),
            event(Some("m1"), 2, 100, 500),
        ]);
        assert_eq!(result.canonical[0].usage().known().output().value(), 913);
        assert_eq!(result.replayed_occurrences, 1);
    }

    /// A shared identity with a different input count is a collision, reported
    /// rather than silently merged or silently dropped.
    #[test]
    fn a_disagreeing_occurrence_is_a_collision_not_a_replay() {
        let result = deduplicate(vec![
            event(Some("m1"), 2, 100, 500),
            event(Some("m1"), 999, 100, 500),
        ]);
        assert_eq!(result.canonical.len(), 1);
        assert_eq!(result.collisions, 1);
        assert_eq!(result.replayed_occurrences, 0);
    }

    /// First-seen order is preserved, so a rendering over the canonical set is
    /// deterministic across runs.
    #[test]
    fn canonical_order_is_first_seen_order() {
        let result = deduplicate(vec![
            event(Some("b"), 1, 1, 0),
            event(Some("a"), 2, 2, 0),
            event(Some("b"), 1, 9, 0),
        ]);
        let ids: Vec<_> = result
            .canonical
            .iter()
            .map(|e| e.strong_identity().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["b", "a"]);
    }

    /// Two occurrences with no identifier but the same timestamp, session and
    /// counts are the same replayed message and collapse, keeping the larger
    /// output: the heuristic domain does the same job the strong domain does,
    /// for the sources that give it nothing stronger to work with.
    #[test]
    fn heuristic_replays_collapse_to_the_largest_output() {
        let result = deduplicate(vec![
            heuristic_event("file.jsonl", 1_000, "s1", 10, 100),
            heuristic_event("file.jsonl", 1_000, "s1", 10, 400),
        ]);
        assert_eq!(result.canonical.len(), 1);
        assert_eq!(result.canonical[0].usage().known().output().value(), 400);
        assert_eq!(result.replayed_occurrences, 1);
        assert_eq!(result.without_identity, 2);
    }

    /// Two heuristic-domain events with different fingerprints (here, different
    /// sessions) never collapse, even with identical counts.
    #[test]
    fn heuristic_events_with_different_fingerprints_never_collapse() {
        let result = deduplicate(vec![
            heuristic_event("file.jsonl", 1_000, "s1", 10, 5),
            heuristic_event("file.jsonl", 1_000, "s2", 10, 5),
        ]);
        assert_eq!(result.canonical.len(), 2);
        assert_eq!(result.replayed_occurrences, 0);
        assert_eq!(result.without_identity, 2);
    }

    /// A stable identifier outranks a heuristic identity by construction: an
    /// event that carries one is never routed through the heuristic domain at
    /// all, so two occurrences sharing a strong identity collapse even when
    /// their counts (what a heuristic fingerprint would compute over) disagree
    /// enough that a heuristic comparison would have called them different
    /// events.
    #[test]
    fn a_stable_identifier_outranks_a_disagreeing_heuristic_signature() {
        let result = deduplicate(vec![
            event(Some("m1"), 2, 100, 500),
            event(Some("m1"), 999_999, 100, 500),
        ]);
        assert_eq!(
            result.canonical.len(),
            1,
            "the strong identity must win even though the counts disagree"
        );
        assert_eq!(result.without_identity, 0);
    }

    /// Strong and heuristic identities live in separate uniqueness domains:
    /// inserting one of each with the same underlying value never lets one
    /// stand in for the other. A heuristic-domain event whose fingerprint
    /// happens to look like some other event's strong identifier does not
    /// collide with it, because the two indexes are never compared.
    #[test]
    fn strong_and_heuristic_identities_are_separate_uniqueness_domains() {
        let result = deduplicate(vec![
            event(Some("m1"), 10, 5, 0),
            heuristic_event("file.jsonl", 1_000, "s1", 10, 5),
        ]);
        assert_eq!(
            result.canonical.len(),
            2,
            "a strong identity and an unrelated heuristic event must not collide"
        );
        assert_eq!(result.replayed_occurrences, 0);
        assert_eq!(result.collisions, 0);
        assert_eq!(result.without_identity, 1);
    }

    /// Events without an identifier are routed through the heuristic domain and
    /// counted there, whether or not their fingerprints happen to match.
    #[test]
    fn events_without_identity_are_routed_through_the_heuristic_domain_and_counted() {
        let result = deduplicate(vec![
            event(None, 10, 5, 0),
            event(None, 10, 5, 0),
            event(Some("m1"), 1, 1, 0),
        ]);
        assert_eq!(
            result.canonical.len(),
            2,
            "the two identical no-identity occurrences are a heuristic replay"
        );
        assert_eq!(result.replayed_occurrences, 1);
        assert_eq!(result.without_identity, 2);
    }

    proptest::proptest! {
        /// The same logical event, replayed into a different file on every
        /// occurrence but otherwise identical, is one canonical event no matter
        /// how many times it is replayed.
        #[test]
        fn prop_the_same_logical_event_in_different_files_collapses(
            replays in 2usize..8,
            occurred_at_nanos in 0i64..10_000_000_000i64,
            input in 0u64..1_000_000u64,
            output in 0u64..1_000_000u64,
        ) {
            let events: Vec<_> = (0..replays)
                .map(|i| {
                    heuristic_event(
                        &format!("replay-{i}.jsonl"),
                        occurred_at_nanos,
                        "s1",
                        input,
                        output,
                    )
                })
                .collect();
            let result = deduplicate(events);
            prop_assert_eq!(result.canonical.len(), 1);
            prop_assert_eq!(result.replayed_occurrences, (replays - 1) as u64);
        }

        /// Two independent equal-sized requests at different timestamps remain
        /// two canonical events: equal size alone is never enough context to
        /// collapse them.
        #[test]
        fn prop_equal_sized_requests_at_different_times_remain_distinct(
            input in 0u64..1_000_000u64,
            output in 0u64..1_000_000u64,
            first_nanos in 0i64..5_000_000_000i64,
            gap_nanos in 1i64..5_000_000_000i64,
        ) {
            let a = event(None, input, output, 0)
                .with_occurred_at(UtcTimestamp::from_unix_nanos(first_nanos))
                .with_session(SessionId::new(
                    SourceNamespace::new("test"),
                    NativeSessionId::new("s1"),
                ));
            let b = event(None, input, output, 0)
                .with_occurred_at(UtcTimestamp::from_unix_nanos(first_nanos + gap_nanos))
                .with_session(SessionId::new(
                    SourceNamespace::new("test"),
                    NativeSessionId::new("s1"),
                ));
            let result = deduplicate(vec![a, b]);
            prop_assert_eq!(result.canonical.len(), 2);
            prop_assert_eq!(result.replayed_occurrences, 0);
        }
    }
}
