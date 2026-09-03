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
//! The two domains fail differently, and the difference is deliberate. A strong
//! identity is a claim the source made, so two occurrences sharing one are the same
//! event even where their counts disagree: the disagreement is counted and the larger
//! output kept. A heuristic key is an inference this module made, so when the
//! inference is contradicted (two occurrences sharing one key whose canonical payload
//! digests differ beyond replay growth) the inference has failed for that pair: the
//! pair is recorded as a [`HeuristicKeyCollision`], quarantined rather than merged,
//! and no winner is selected anywhere. Silent selection here is an undercount, and an
//! undercount is indistinguishable from correct output (aub-lqe.10, PLAN.md 12.10,
//! 18, 34.13).
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

use crate::evidence::ComponentKind;
use crate::transcripts::NormalizedUsageEvent;
use crate::transcripts::parser::ParserVersion;

pub use fingerprint::{HeuristicKey, canonical_payload_digest};

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
    /// Heuristic-key collisions: pairs of occurrences that shared one parser-scoped
    /// key but normalize to materially different payloads. Both occurrences of every
    /// pair are excluded from `canonical`; the pair is quarantined and the affected
    /// aggregates report partial coverage rather than either occurrence being kept.
    pub heuristic_collisions: Vec<HeuristicKeyCollision>,
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
    let mut heuristic_collisions: Vec<HeuristicKeyCollision> = Vec::new();
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
                let kept_digest = canonical_payload_digest(&canonical[kept_at]);
                let challenger_digest = canonical_payload_digest(&event);
                if kept_digest == challenger_digest {
                    replayed_occurrences += 1;
                    let kept = &canonical[kept_at];
                    if output_of(&event) > output_of(kept) {
                        canonical[kept_at] = event;
                    }
                } else {
                    // Materially different payloads under one key: the inference
                    // failed for this pair. Quarantine both, select neither, and
                    // free the key so a later occurrence starts a fresh round
                    // rather than joining a decided dispute.
                    let kept = canonical.remove(kept_at);
                    reindex_after_removal(&mut index_by_strong, kept_at);
                    reindex_after_removal(&mut index_by_heuristic, kept_at);
                    index_by_heuristic.remove(&key);
                    heuristic_collisions.push(HeuristicKeyCollision {
                        parser_version: event.parser_version().clone(),
                        heuristic_key: key.1,
                        first_digest: kept_digest,
                        second_digest: challenger_digest,
                        first: kept,
                        second: event,
                    });
                }
            }
        }
    }

    Deduplicated {
        canonical,
        replayed_occurrences,
        collisions,
        heuristic_collisions,
        without_identity,
    }
}

/// Decrements every canonical index above a removed slot, so the indexes keep
/// naming the events they named. Collisions are rare, so the linear pass over
/// the maps is cheap next to the correctness of keeping the canonical vector
/// contiguous.
fn reindex_after_removal<K: Ord>(index: &mut BTreeMap<K, usize>, removed_at: usize) {
    for slot in index.values_mut() {
        if *slot > removed_at {
            *slot -= 1;
        }
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

/// A heuristic-key collision: two occurrences that shared one parser-scoped
/// heuristic key but normalize to materially different canonical payloads. The
/// key cannot tell them apart and the digests prove them different, so neither
/// occurrence is selected: the pair is quarantined with the collision as its
/// failure class and the aggregates that include the pair's interval report
/// partial coverage naming the components the pair would have carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeuristicKeyCollision {
    parser_version: ParserVersion,
    heuristic_key: HeuristicKey,
    first: NormalizedUsageEvent,
    second: NormalizedUsageEvent,
    first_digest: String,
    second_digest: String,
}

impl HeuristicKeyCollision {
    /// The parser whose heuristic key both occurrences shared.
    pub fn parser_version(&self) -> &ParserVersion {
        &self.parser_version
    }

    /// The shared key, as it is stored in `usage_occurrence.heuristic_key`.
    pub fn heuristic_key(&self) -> &HeuristicKey {
        &self.heuristic_key
    }

    /// The two colliding occurrences, in first-seen order. Neither is the
    /// canonical one: no code path may select between them.
    pub fn occurrences(&self) -> [&NormalizedUsageEvent; 2] {
        [&self.first, &self.second]
    }

    /// The canonical payload digests of the two occurrences, in the same order
    /// as [`Self::occurrences`]. They differ by construction; this is what the
    /// quarantine hashes so the same collision recurring merges rather than
    /// duplicating.
    pub fn payload_digests(&self) -> (&str, &str) {
        (&self.first_digest, &self.second_digest)
    }

    /// The token components the pair would have carried into an aggregate,
    /// sorted and deduplicated: every kind with a nonzero count in either
    /// occurrence. The aggregate that includes the pair's interval marks these
    /// as its missing coverage, because nothing of the pair was counted.
    pub fn missing_components(&self) -> Vec<ComponentKind> {
        let mut kinds: BTreeMap<String, ()> = BTreeMap::new();
        for occurrence in [&self.first, &self.second] {
            let known = occurrence.usage().known();
            for (kind, count) in [
                ("input", known.input().value()),
                ("output", known.output().value()),
                ("cache_read", known.cache_read().value()),
                ("cache_write", known.cache_write().value()),
            ] {
                if count > 0 {
                    kinds.insert(kind.to_string(), ());
                }
            }
            for (name, count) in occurrence.usage().unknown() {
                if count.value() > 0 {
                    kinds.insert(name.clone(), ());
                }
            }
        }
        kinds.into_keys().map(ComponentKind::new).collect()
    }
}

/// The heuristic replay fingerprint: the identity computed for an event that
/// carries no source-provided identifier.
mod fingerprint {
    use crate::evidence::{ComponentKind, CoverageCompleteness, EstimatorId, EvidenceQuality};
    use crate::transcripts::NormalizedUsageEvent;
    use crate::transcripts::parser::EvidenceClassification;

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
        /// The fingerprint algorithm's version. Recorded on every heuristic-domain
        /// occurrence row (`usage_occurrence.heuristic_algorithm_version`), so the
        /// stored ledger declares which algorithm computed its identities. Bump this
        /// whenever [`Self::compute`]'s output changes shape: a stored row under an
        /// older version then cannot be silently extended with new-version keys,
        /// because the same logical event would fork into two canonical identities.
        /// `crate::store::usage_occurrence::heuristic_rebuild_required` reads the
        /// stored versions and names every parser whose identities must be rebuilt
        /// under the running version rather than silently mixed.
        pub const ALGORITHM_VERSION: &str = "hk1";

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

        /// The key's string form, as stored in `usage_occurrence.heuristic_key`.
        pub fn as_str(&self) -> &str {
            &self.0
        }
    }

    /// The canonical payload digest of one occurrence: a stable digest over the
    /// semantic payload it carries, deliberately excluding the one dimension a
    /// replay is expected to change. Two occurrences sharing a heuristic key and
    /// this digest carry the same material payload, so their difference is replay
    /// growth and they collapse; sharing the key while disagreeing here is the
    /// collision [`super::HeuristicKeyCollision`] records. The output count is the
    /// excluded dimension, for the same reason [`HeuristicKey`] excludes it; input,
    /// both cache kinds, the unknown components and the evidence qualifications
    /// (coverage, quality, classification) are the material payload. This is what
    /// `usage_occurrence.canonical_payload_digest` stores, so the same materiality
    /// comparison stays available at the store boundary.
    pub fn canonical_payload_digest(event: &NormalizedUsageEvent) -> String {
        use sha2::{Digest, Sha256};
        let mut payload = String::new();
        let known = event.usage().known();
        field(&mut payload, "input", &known.input().value().to_string());
        field(
            &mut payload,
            "cache_read",
            &known.cache_read().value().to_string(),
        );
        field(
            &mut payload,
            "cache_write",
            &known.cache_write().value().to_string(),
        );
        for (name, count) in event.usage().unknown() {
            field(
                &mut payload,
                "unknown",
                &format!("{}={}", name, count.value()),
            );
        }
        match event.usage().coverage() {
            CoverageCompleteness::Complete => field(&mut payload, "coverage", "complete"),
            CoverageCompleteness::Partial { missing } => {
                let mut names: Vec<&str> = missing.iter().map(ComponentKind::as_str).collect();
                names.sort_unstable();
                field(
                    &mut payload,
                    "coverage",
                    &format!("partial:{}", names.join(",")),
                );
            }
        }
        match event.usage().quality() {
            EvidenceQuality::Measured => field(&mut payload, "quality", "measured"),
            EvidenceQuality::Estimated {
                methods,
                uncertainty,
            } => field(
                &mut payload,
                "quality",
                &digest_quality_form("estimated", methods, uncertainty.as_ref()),
            ),
            EvidenceQuality::Mixed {
                methods,
                uncertainty,
            } => field(
                &mut payload,
                "quality",
                &digest_quality_form("mixed", methods, uncertainty.as_ref()),
            ),
        }
        match event.classification() {
            EvidenceClassification::Reported => field(&mut payload, "classification", "reported"),
            EvidenceClassification::Derived => field(&mut payload, "classification", "derived"),
            EvidenceClassification::Reconstructed { estimator, version } => field(
                &mut payload,
                "classification",
                &format!("reconstructed:{}:{}", estimator.as_str(), version.as_str()),
            ),
        }
        format!("{:x}", Sha256::digest(payload.as_bytes()))
    }

    /// Appends one length-prefixed field to the digest input, so a component or
    /// estimator name that happens to contain a separator cannot alias two
    /// different payloads into one digest.
    fn field(out: &mut String, tag: &str, value: &str) {
        out.push_str(tag);
        out.push('=');
        out.push_str(&value.len().to_string());
        out.push(':');
        out.push_str(value);
        out.push(';');
    }

    /// The digest form of a non-measured evidence quality: the variant, its
    /// estimator methods, and the uncertainty bounds it carries.
    fn digest_quality_form<T: crate::domain::interval::DomainQuantity>(
        variant: &str,
        methods: &std::collections::BTreeSet<EstimatorId>,
        uncertainty: Option<&crate::domain::interval::Interval<T>>,
    ) -> String {
        let mut names: Vec<&str> = methods.iter().map(EstimatorId::as_str).collect();
        names.sort_unstable();
        let bounds = match uncertainty {
            Some(interval) => format!(
                ":{}-{}",
                interval.lower().to_exact_string(),
                interval.upper().to_exact_string()
            ),
            None => ":none".to_string(),
        };
        format!("{}:{}{}", variant, names.join(","), bounds)
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

    /// A heuristic-domain event whose evidence qualifications the caller
    /// controls, so collision tests can make two same-key occurrences differ
    /// materially beyond the output count.
    fn qualified_heuristic_event(
        occurred_at_nanos: i64,
        input: u64,
        output: u64,
        coverage: CoverageCompleteness,
        classification: EvidenceClassification,
    ) -> NormalizedUsageEvent {
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            coverage,
            EvidenceQuality::Measured,
        );
        NormalizedUsageEvent::new(
            usage,
            classification,
            Provenance::new(vec!["file.jsonl".to_string()]),
            ParserVersion::new("test-1"),
        )
        .with_occurred_at(UtcTimestamp::from_unix_nanos(occurred_at_nanos))
        .with_session(SessionId::new(
            SourceNamespace::new("test"),
            NativeSessionId::new("s1"),
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

    /// Two occurrences sharing a heuristic key whose canonical payload digests
    /// differ produce a collision record rather than a merge, and neither
    /// occurrence is kept: canonical is empty, and the pair is named in full.
    #[test]
    fn a_heuristic_collision_is_recorded_and_neither_occurrence_is_kept() {
        let first = qualified_heuristic_event(
            1_000,
            10,
            5,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reported,
        );
        let second = qualified_heuristic_event(
            1_000,
            10,
            913,
            CoverageCompleteness::partial([ComponentKind::new("output")]),
            EvidenceClassification::Reported,
        );
        let result = deduplicate(vec![first, second]);
        assert_eq!(result.heuristic_collisions.len(), 1);
        assert!(result.canonical.is_empty(), "no winner may be selected");
        assert_eq!(result.replayed_occurrences, 0);
        assert_eq!(result.without_identity, 2);
        let collision = &result.heuristic_collisions[0];
        assert_eq!(collision.parser_version().as_str(), "test-1");
        assert_eq!(collision.occurrences().len(), 2);
        let (first_digest, second_digest) = collision.payload_digests();
        assert_ne!(first_digest, second_digest);
        assert_eq!(
            first_digest,
            &canonical_payload_digest(collision.occurrences()[0])
        );
    }

    /// The planted negative for the collision record: the same two occurrences
    /// differing only in the output count, which is the replay dimension, are
    /// not a collision. The digests agree and the pair collapses exactly as it
    /// did before the collision path existed.
    #[test]
    fn the_same_payload_beyond_output_is_a_replay_not_a_collision() {
        let result = deduplicate(vec![
            heuristic_event("file.jsonl", 1_000, "s1", 10, 5),
            heuristic_event("file.jsonl", 1_000, "s1", 10, 913),
        ]);
        assert!(result.heuristic_collisions.is_empty());
        assert_eq!(result.canonical.len(), 1);
        assert_eq!(result.replayed_occurrences, 1);
        assert_eq!(result.canonical[0].usage().known().output().value(), 913);
    }

    /// A collision quarantines its pair and frees the key: the next occurrence
    /// under that key starts a fresh round instead of joining a decided
    /// dispute, and a replay of it collapses normally.
    #[test]
    fn a_collision_frees_the_key_for_a_fresh_round() {
        let reported = qualified_heuristic_event(
            1_000,
            10,
            5,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reported,
        );
        let estimated = qualified_heuristic_event(
            1_000,
            10,
            5,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reconstructed {
                estimator: crate::evidence::EstimatorId::new("chars"),
                version: crate::transcripts::parser::EstimatorVersion::new("1"),
            },
        );
        let later = qualified_heuristic_event(
            1_000,
            10,
            913,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reported,
        );
        let replay_of_later = qualified_heuristic_event(
            1_000,
            10,
            950,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reported,
        );
        let result = deduplicate(vec![reported, estimated, later, replay_of_later]);
        assert_eq!(result.heuristic_collisions.len(), 1);
        assert_eq!(result.canonical.len(), 1);
        assert_eq!(result.replayed_occurrences, 1);
        assert_eq!(result.canonical[0].usage().known().output().value(), 950);
    }

    /// The collision names what the pair would have carried: every kind with a
    /// nonzero count in either occurrence, deduplicated and sorted.
    #[test]
    fn a_collision_names_the_components_its_pair_would_have_carried() {
        let first = qualified_heuristic_event(
            1_000,
            10,
            5,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reported,
        );
        let second = qualified_heuristic_event(
            1_000,
            10,
            0,
            CoverageCompleteness::partial([ComponentKind::new("output")]),
            EvidenceClassification::Reported,
        );
        let result = deduplicate(vec![first, second]);
        let names: Vec<String> = result.heuristic_collisions[0]
            .missing_components()
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect();
        assert_eq!(names, ["input", "output"]);
    }

    /// The payload digest is stable for the same material payload and blind to
    /// the source file, like the key it accompanies.
    #[test]
    fn the_payload_digest_is_deterministic_and_file_free() {
        let a = heuristic_event("first.jsonl", 1_000, "s1", 10, 5);
        let b = heuristic_event("second.jsonl", 1_000, "s1", 10, 5);
        assert_eq!(canonical_payload_digest(&a), canonical_payload_digest(&b));
    }

    /// An output-only difference leaves the digest alone: that dimension is the
    /// replay growth a shared key exists to collapse. The planted negative is
    /// the classification, which no replay changes and which must move the
    /// digest, because merging a measured and a reconstructed occurrence would
    /// silently erase the distinction between them.
    #[test]
    fn an_output_only_difference_keeps_the_digest_and_a_classification_difference_breaks_it() {
        let small = heuristic_event("file.jsonl", 1_000, "s1", 10, 5);
        let grown = heuristic_event("file.jsonl", 1_000, "s1", 10, 913);
        assert_eq!(
            canonical_payload_digest(&small),
            canonical_payload_digest(&grown)
        );

        let reconstructed = qualified_heuristic_event(
            1_000,
            10,
            5,
            CoverageCompleteness::Complete,
            EvidenceClassification::Reconstructed {
                estimator: crate::evidence::EstimatorId::new("chars"),
                version: crate::transcripts::parser::EstimatorVersion::new("1"),
            },
        );
        assert_ne!(
            canonical_payload_digest(&small),
            canonical_payload_digest(&reconstructed)
        );
    }

    /// A coverage difference is a material payload difference: an occurrence
    /// that claims a complete count and one that admits a missing component
    /// never digest the same under one key.
    #[test]
    fn a_coverage_difference_breaks_the_digest() {
        let complete = heuristic_event("file.jsonl", 1_000, "s1", 10, 5);
        let partial = qualified_heuristic_event(
            1_000,
            10,
            5,
            CoverageCompleteness::partial([ComponentKind::new("cache-read")]),
            EvidenceClassification::Reported,
        );
        assert_ne!(
            canonical_payload_digest(&complete),
            canonical_payload_digest(&partial)
        );
    }

    /// The fingerprint algorithm carries a version, and it is a version string,
    /// not an absence: the stored rows' versions are what make an algorithm
    /// change detectable instead of silent.
    #[test]
    fn the_fingerprint_algorithm_declares_a_version() {
        assert!(!HeuristicKey::ALGORITHM_VERSION.is_empty());
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
