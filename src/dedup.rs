//! One canonical replay-deduplication implementation.
//!
//! Deduplication happens once, here, after parsing and before anything counts an
//! event, because a per-parser implementation is how two parsers end up with two
//! definitions of the same event. A transcript replays usage: Claude Code writes one
//! line per content block of an assistant message and every line carries the same
//! message identifier, with the output count growing across them, so a consumer that
//! keeps the first occurrence undercounts and one that sums them multiplies.
//!
//! This module currently owns the strong-identity half of that job: events carrying
//! a source-provided identifier collapse to one canonical event per identifier, and
//! the occurrence with the largest output count is the one kept, because the others
//! are earlier snapshots of the same message. Events without an identifier are
//! passed through one per occurrence and counted, so a report can say how much of
//! its total rests on occurrences nothing could collapse. Heuristic fingerprints and
//! the database uniqueness constraint are the other half and are not here yet.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::collections::BTreeMap;

use crate::transcripts::NormalizedUsageEvent;

/// The outcome of deduplicating one batch of events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deduplicated {
    /// One event per strong identity, plus every event that had none, in first-seen
    /// order.
    pub canonical: Vec<NormalizedUsageEvent>,
    /// Occurrences that collapsed into an already-seen identity because their input
    /// and cache counts matched it.
    pub replayed_occurrences: u64,
    /// Occurrences that shared an identity with a kept event but disagreed on a
    /// count other than output. The kept event is the one with the larger output;
    /// the disagreement is reported, never resolved silently.
    pub collisions: u64,
    /// Events that carried no strong identity and were kept one per occurrence.
    pub without_identity: u64,
}

/// Collapses replayed occurrences of one event into one canonical event.
pub fn deduplicate(events: Vec<NormalizedUsageEvent>) -> Deduplicated {
    let mut canonical: Vec<NormalizedUsageEvent> = Vec::with_capacity(events.len());
    let mut index_by_identity: BTreeMap<String, usize> = BTreeMap::new();
    let mut replayed_occurrences = 0;
    let mut collisions = 0;
    let mut without_identity = 0;

    for event in events {
        let Some(identity) = event.strong_identity().map(str::to_string) else {
            without_identity += 1;
            canonical.push(event);
            continue;
        };
        match index_by_identity.get(&identity) {
            None => {
                index_by_identity.insert(identity, canonical.len());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
    };
    use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
    use crate::transcripts::parser::{
        EvidenceClassification, ParserVersion, STRONG_IDENTITY_PREFIX,
    };

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
        assert_eq!(result.without_identity, 0);
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

    /// Events without an identity are kept one per occurrence and counted, so a
    /// report can say how much rests on occurrences nothing could collapse.
    #[test]
    fn events_without_identity_pass_through_and_are_counted() {
        let result = deduplicate(vec![
            event(None, 10, 5, 0),
            event(None, 10, 5, 0),
            event(Some("m1"), 1, 1, 0),
        ]);
        assert_eq!(result.canonical.len(), 3);
        assert_eq!(result.without_identity, 2);
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
}
