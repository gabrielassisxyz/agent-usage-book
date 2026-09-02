//! The cumulative-source pipeline: deduplicate first (the caller has already
//! done that), order the survivors, difference them into deltas.
//!
//! The order is the whole bead (aub-lqe.9, PLAN.md section 18): a cumulative
//! source reports totals so far, so differencing adjacent survivors yields the
//! consumption between them, and differencing before deduplicating or before
//! ordering turns a replayed snapshot into apparent new consumption. The
//! pipeline runs pre-store, on the deduplicated canonical batch, and replaces
//! a cumulative source's events with its deltas; every other source passes
//! through unchanged.
//!
//! A counter reset is a total that decreases. There is no correct silent
//! behaviour for one: the affected record is rejected with a typed reason, no
//! negative delta is stored, and the excluded delta's components surface as
//! partial coverage on the group the record would have joined.
//!
//! May not depend on:
//! - presentation
//! - SQLite

use std::collections::BTreeMap;

use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};
use crate::evidence::ComponentKind;
use crate::transcripts::parser::{EvidenceClassification, NormalizedUsageEvent, ParserVersion};

/// One cumulative source's ordered series outcome: the deltas that replace its
/// events, and the counter resets that were rejected instead of stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CumulativeOutcome {
    /// Delta events, one per surviving ordered record, each carrying the
    /// consumption between its predecessor and itself. The first record in a
    /// series is baselined at zero, so its delta is its own total.
    pub deltas: Vec<NormalizedUsageEvent>,
    /// Records rejected because their totals decreased against the ordered
    /// predecessor. A negative delta is never silently stored.
    pub resets: Vec<CounterResetExclusion>,
}

/// A rejected record and what its exclusion hides. The interval spans the
/// previous ordered record's timestamp to the rejected record's timestamp, and
/// the components name what the report's coverage must mark missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterResetExclusion {
    /// The record whose totals decreased.
    pub rejected: NormalizedUsageEvent,
    /// The ordered predecessor it decreased against.
    pub previous: NormalizedUsageEvent,
    /// The first timestamp of the affected interval, when the predecessor was
    /// dated.
    pub interval_since: Option<crate::domain::time::UtcTimestamp>,
    /// The first timestamp after the affected interval, when the rejected
    /// record was dated.
    pub interval_until: Option<crate::domain::time::UtcTimestamp>,
    /// The token kinds the excluded delta would have carried.
    pub missing: Vec<ComponentKind>,
}

impl CounterResetExclusion {
    /// Why the record was rejected, for diagnostics and quarantine-class-style
    /// reporting.
    pub fn reason(&self) -> &'static str {
        "counter_reset"
    }
}

/// Derives deltas for the cumulative sources among `canonical` and passes every
/// other source's events through unchanged.
///
/// `is_cumulative` reports whether a parser version declares itself cumulative
/// (`ParserAdapter::reports_cumulative`); the caller supplies it as a predicate
/// so the pipeline never has to enumerate parsers itself.
///
/// The events are grouped by source series: parser version plus session,
/// falling back to the file name when the source attributes no session. Each
/// group is ordered by the strongest available discriminator: source
/// sequence where the event carries one, then the record timestamp, then a
/// documented deterministic tiebreak (known components ascending, then source
/// file) for records that share a timestamp and carry no sequence. Two records
/// that tie through the whole order are the same semantic record and have
/// already collapsed in dedup, so the tiebreak is never load-bearing between
/// genuinely distinct records; it exists so the order is never arbitrary.
pub fn derive_cumulative_deltas(
    canonical: Vec<NormalizedUsageEvent>,
    is_cumulative: &dyn Fn(&ParserVersion) -> bool,
) -> (Vec<NormalizedUsageEvent>, CumulativeOutcome) {
    let mut passthrough = Vec::new();
    let mut groups: BTreeMap<SeriesKey, Vec<NormalizedUsageEvent>> = BTreeMap::new();
    for event in canonical {
        if is_cumulative(event.parser_version()) {
            groups.entry(series_key(&event)).or_default().push(event);
        } else {
            passthrough.push(event);
        }
    }

    let mut outcome = CumulativeOutcome {
        deltas: Vec::new(),
        resets: Vec::new(),
    };
    for (_, mut series) in groups {
        series.sort_by(compare_series_records);
        let mut previous: Option<NormalizedUsageEvent> = None;
        for record in series {
            match &previous {
                None => outcome.deltas.push(delta_from_zero(&record)),
                Some(prev) => match difference(prev, &record) {
                    Ok(delta) => outcome.deltas.push(with_delta_usage(&record, delta)),
                    Err(missing) => outcome.resets.push(CounterResetExclusion {
                        rejected: record.clone(),
                        previous: prev.clone(),
                        interval_since: prev.occurred_at(),
                        interval_until: record.occurred_at(),
                        missing,
                    }),
                },
            }
            previous = Some(record);
        }
    }

    passthrough.extend(outcome.deltas.iter().cloned());
    (passthrough, outcome)
}

/// The series an event belongs to: its parser's domain, narrowed by the session
/// the source attributes it to, falling back to the file it was read from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SeriesKey {
    parser: String,
    scope: String,
}

fn series_key(event: &NormalizedUsageEvent) -> SeriesKey {
    let parser = event.parser_version().as_str().to_string();
    let scope = match event.session() {
        Some(session) => format!(
            "session:{}:{}",
            session.source().as_str(),
            session.native().as_str()
        ),
        None => format!("file:{}", event_file(event)),
    };
    SeriesKey { parser, scope }
}

/// The file an event was read from: the one provenance source that is not a
/// strong-identity entry. The same convention `report::spend` uses, defined
/// there first; this module reads it rather than inventing a second.
fn event_file(event: &NormalizedUsageEvent) -> String {
    event
        .provenance()
        .sources()
        .iter()
        .find(|source| !source.starts_with(crate::transcripts::parser::STRONG_IDENTITY_PREFIX))
        .cloned()
        .unwrap_or_default()
}

/// Orders two records of one series.
///
/// Source sequence first (a claim the source makes, immune to clock skew),
/// then the record timestamp, then the documented tiebreak: known components
/// ascending, then the source file. The tiebreak exists so equal timestamps
/// without a sequence still produce a deterministic order rather than an
/// arbitrary one; records that tie through it are the same semantic record and
/// dedup has already collapsed them.
fn compare_series_records(
    a: &NormalizedUsageEvent,
    b: &NormalizedUsageEvent,
) -> std::cmp::Ordering {
    a.sequence()
        .cmp(&b.sequence())
        .then_with(|| a.occurred_at().cmp(&b.occurred_at()))
        .then_with(|| known_components(a).cmp(&known_components(b)))
        .then_with(|| event_file(a).cmp(&event_file(b)))
}

/// The four known components as an orderable tuple.
fn known_components(event: &NormalizedUsageEvent) -> (u64, u64, u64, u64) {
    let known = event.usage().known();
    (
        known.input().value(),
        known.output().value(),
        known.cache_read().value(),
        known.cache_write().value(),
    )
}

/// Differences one ordered record against its predecessor. The error carries
/// the kinds that decreased: a counter reset has no correct silent handling,
/// so the record is rejected with the exclusion named rather than a negative
/// delta stored.
fn difference(
    previous: &NormalizedUsageEvent,
    record: &NormalizedUsageEvent,
) -> Result<KnownTokenVector, Vec<ComponentKind>> {
    let prev = previous.usage().known();
    let curr = record.usage().known();
    let input = curr.input().value();
    let output = curr.output().value();
    let cache_read = curr.cache_read().value();
    let cache_write = curr.cache_write().value();
    let mut decreased: Vec<ComponentKind> = Vec::new();
    if input < prev.input().value() {
        decreased.push(ComponentKind::new("input"));
    }
    if output < prev.output().value() {
        decreased.push(ComponentKind::new("output"));
    }
    if cache_read < prev.cache_read().value() {
        decreased.push(ComponentKind::new("cache_read"));
    }
    if cache_write < prev.cache_write().value() {
        decreased.push(ComponentKind::new("cache_write"));
    }
    if !decreased.is_empty() {
        return Err(decreased);
    }
    Ok(KnownTokenVector::new(
        InputTokens::new(input - prev.input().value()),
        OutputTokens::new(output - prev.output().value()),
        CacheReadTokens::new(cache_read - prev.cache_read().value()),
        CacheWriteTokens::new(cache_write - prev.cache_write().value()),
    ))
}

/// The first record in a series is its own delta: the counter's baseline is
/// zero, so the totals-so-far at the first observation are the consumption up
/// to that observation.
fn delta_from_zero(record: &NormalizedUsageEvent) -> NormalizedUsageEvent {
    let known = record.usage().known();
    let delta = KnownTokenVector::new(
        InputTokens::new(known.input().value()),
        OutputTokens::new(known.output().value()),
        CacheReadTokens::new(known.cache_read().value()),
        CacheWriteTokens::new(known.cache_write().value()),
    );
    with_delta_usage(record, delta)
}

/// The same event carrying delta usage, preserving the record's timestamp,
/// session, sequence, provenance and classification so the store's identity
/// and replay protection stay anchored to the source record.
fn with_delta_usage(
    record: &NormalizedUsageEvent,
    delta: KnownTokenVector,
) -> NormalizedUsageEvent {
    let usage = UsageVector::new(
        delta,
        record.usage().unknown().clone(),
        record.usage().coverage().clone(),
        record.usage().quality().clone(),
    );
    let mut event = NormalizedUsageEvent::new(
        usage,
        record.classification().clone(),
        record.provenance().clone(),
        record.parser_version().clone(),
    );
    if let Some(occurred_at) = record.occurred_at() {
        event = event.with_occurred_at(occurred_at);
    }
    if let Some(session) = record.session() {
        event = event.with_session(session.clone());
    }
    if let Some(sequence) = record.sequence() {
        event = event.with_sequence(sequence);
    }
    event
}

/// Sums the known components of a list of events, for callers that report a
/// series total. Unknown components are summed by key the same way the spend
/// report sums them.
pub fn total_known_components(events: &[NormalizedUsageEvent]) -> KnownTokenVector {
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    for event in events {
        let known = event.usage().known();
        input += known.input().value();
        output += known.output().value();
        cache_read += known.cache_read().value();
        cache_write += known.cache_write().value();
    }
    KnownTokenVector::new(
        InputTokens::new(input),
        OutputTokens::new(output),
        CacheReadTokens::new(cache_read),
        CacheWriteTokens::new(cache_write),
    )
}

/// A count by unknown-component key across events, mirroring the spend path's
/// aggregation so a caller comparing totals sees one convention.
pub fn total_unknown_components(events: &[NormalizedUsageEvent]) -> BTreeMap<String, TokenCount> {
    let mut unknown: BTreeMap<String, TokenCount> = BTreeMap::new();
    for event in events {
        for (key, count) in event.usage().unknown() {
            let entry = unknown.entry(key.clone()).or_insert(TokenCount::new(0));
            *entry = *entry + *count;
        }
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::SessionId;
    use crate::domain::ids::SourceNamespace;
    use crate::domain::time::UtcTimestamp;
    use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
    use crate::transcripts::parser::EvidenceClassification;

    fn cumulative_parser() -> ParserVersion {
        ParserVersion::new("codex-1")
    }

    fn plain_parser() -> ParserVersion {
        ParserVersion::new("claude-code-1")
    }

    fn is_cumulative(version: &ParserVersion) -> bool {
        version.as_str() == "codex-1"
    }

    fn event(
        parser: ParserVersion,
        totals: (u64, u64),
        occurred_at: Option<i64>,
        session: Option<&str>,
        file: &str,
    ) -> NormalizedUsageEvent {
        let known = KnownTokenVector::new(
            InputTokens::new(totals.0),
            OutputTokens::new(totals.1),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        );
        let mut event = NormalizedUsageEvent::new(
            UsageVector::new(
                known,
                BTreeMap::new(),
                CoverageCompleteness::Complete,
                EvidenceQuality::Measured,
            ),
            EvidenceClassification::Reported,
            Provenance::new([file.to_string()]),
            parser,
        );
        if let Some(nanos) = occurred_at {
            event = event.with_occurred_at(UtcTimestamp::from_unix_nanos(nanos));
        }
        if let Some(native) = session {
            event = event.with_session(SessionId::new(
                SourceNamespace::new("codex"),
                crate::domain::ids::NativeSessionId::new(native),
            ));
        }
        event
    }

    #[test]
    fn non_cumulative_sources_pass_through_untouched() {
        let plain = event(plain_parser(), (10, 5), Some(1_000), None, "a.jsonl");
        let (canonical, outcome) = derive_cumulative_deltas(vec![plain.clone()], &is_cumulative);
        assert_eq!(
            canonical,
            vec![plain],
            "a non-cumulative event is its own consumption"
        );
        assert!(outcome.deltas.is_empty());
        assert!(outcome.resets.is_empty());
    }

    #[test]
    fn deltas_are_differences_of_the_ordered_survivors() {
        let first = event(
            cumulative_parser(),
            (100, 10),
            Some(1_000),
            Some("s1"),
            "f1.jsonl",
        );
        let second = event(
            cumulative_parser(),
            (250, 40),
            Some(2_000),
            Some("s1"),
            "f2.jsonl",
        );
        let (canonical, outcome) =
            derive_cumulative_deltas(vec![second.clone(), first.clone()], &is_cumulative);
        // The input order is reversed; the ordering step must repair it.
        let totals = total_known_components(&outcome.deltas);
        assert_eq!(totals.input().value(), 250);
        assert_eq!(totals.output().value(), 40);
        assert_eq!(outcome.deltas.len(), 2);
        assert_eq!(canonical.len(), 2);
        assert_eq!(outcome.resets.len(), 0);
        // The first record is its own delta (baseline zero), the second is the
        // difference against it.
        assert_eq!(outcome.deltas[0].usage().known().input().value(), 100);
        assert_eq!(outcome.deltas[1].usage().known().input().value(), 150);
        assert_eq!(outcome.deltas[1].usage().known().output().value(), 30);
    }

    /// A replayed cumulative record collapses in dedup before this pipeline
    /// runs; this test pins the ordering path the deduplicated series takes
    /// when two survivors share a timestamp: deterministic, not arbitrary.
    #[test]
    fn equal_timestamps_without_sequence_order_deterministically() {
        let a = event(
            cumulative_parser(),
            (100, 5),
            Some(1_000),
            Some("s1"),
            "f1.jsonl",
        );
        let b = event(
            cumulative_parser(),
            (200, 9),
            Some(1_000),
            Some("s1"),
            "f0.jsonl",
        );
        let (_, outcome) = derive_cumulative_deltas(vec![b.clone(), a.clone()], &is_cumulative);
        // Same timestamp, no sequence: the tiebreak orders by known components
        // ascending, so (100, 5) precedes (200, 9) regardless of input order.
        assert_eq!(outcome.deltas[0].usage().known().input().value(), 100);
        assert_eq!(outcome.deltas[1].usage().known().input().value(), 100);
        assert_eq!(outcome.deltas[1].usage().known().output().value(), 4);
    }

    #[test]
    fn a_source_sequence_orders_where_a_timestamp_cannot() {
        // Two records with identical timestamps but a source sequence: the
        // sequence is the stronger discriminator and orders the series.
        let earlier = event(
            cumulative_parser(),
            (100, 5),
            Some(1_000),
            Some("s1"),
            "f1.jsonl",
        )
        .with_sequence(1);
        let later = event(
            cumulative_parser(),
            (200, 9),
            Some(1_000),
            Some("s1"),
            "f2.jsonl",
        )
        .with_sequence(2);
        let (_, outcome) = derive_cumulative_deltas(vec![later, earlier], &is_cumulative);
        assert_eq!(outcome.deltas[0].usage().known().input().value(), 100);
        assert_eq!(outcome.deltas[1].usage().known().input().value(), 100);
    }

    #[test]
    fn a_counter_reset_is_rejected_with_an_interval_never_a_negative_delta() {
        let before = event(
            cumulative_parser(),
            (500, 50),
            Some(1_000),
            Some("s1"),
            "f1.jsonl",
        );
        let after = event(
            cumulative_parser(),
            (80, 8),
            Some(2_000),
            Some("s1"),
            "f2.jsonl",
        );
        let (_, outcome) = derive_cumulative_deltas(vec![before, after], &is_cumulative);
        assert_eq!(outcome.resets.len(), 1, "the decreasing record is rejected");
        assert_eq!(outcome.deltas.len(), 1, "no delta crosses the reset");
        let reset = &outcome.resets[0];
        assert_eq!(reset.reason(), "counter_reset");
        assert_eq!(
            reset.rejected.usage().known().input().value(),
            80,
            "the rejected record is the one whose totals decreased"
        );
        assert_eq!(
            reset.missing,
            vec![ComponentKind::new("input"), ComponentKind::new("output")],
            "both decreasing components are named as the excluded coverage"
        );
        assert_eq!(
            reset.interval_since.map(UtcTimestamp::unix_nanos),
            Some(1_000)
        );
        assert_eq!(
            reset.interval_until.map(UtcTimestamp::unix_nanos),
            Some(2_000)
        );
        let totals = total_known_components(&outcome.deltas);
        assert_eq!(totals.input().value(), 500, "the pre-reset delta survives");
    }

    #[test]
    fn a_reset_in_one_series_does_not_touch_another() {
        let good_first = event(
            cumulative_parser(),
            (10, 1),
            Some(1_000),
            Some("s1"),
            "f1.jsonl",
        );
        let good_second = event(
            cumulative_parser(),
            (20, 2),
            Some(2_000),
            Some("s1"),
            "f2.jsonl",
        );
        let reset_first = event(
            cumulative_parser(),
            (500, 50),
            Some(3_000),
            Some("s2"),
            "f3.jsonl",
        );
        let reset_second = event(
            cumulative_parser(),
            (5, 1),
            Some(4_000),
            Some("s2"),
            "f4.jsonl",
        );
        let (_, outcome) = derive_cumulative_deltas(
            vec![reset_second, reset_first, good_second, good_first],
            &is_cumulative,
        );
        assert_eq!(outcome.resets.len(), 1, "only the second series resets");
        assert_eq!(
            outcome.deltas.len(),
            3,
            "the healthy series derives both deltas"
        );
    }

    /// Separate sessions never difference against each other: grouping is per
    /// series, so one session's totals are never a baseline for another's.
    #[test]
    fn sessions_are_independent_series() {
        let a = event(
            cumulative_parser(),
            (100, 5),
            Some(1_000),
            Some("s1"),
            "f1.jsonl",
        );
        let b = event(
            cumulative_parser(),
            (70, 7),
            Some(2_000),
            Some("s2"),
            "f2.jsonl",
        );
        let (_, outcome) = derive_cumulative_deltas(vec![b, a], &is_cumulative);
        assert!(
            outcome.resets.is_empty(),
            "different sessions do not reset each other"
        );
        assert_eq!(outcome.deltas.len(), 2);
        let totals = total_known_components(&outcome.deltas);
        assert_eq!(totals.input().value(), 170, "each series baselines at zero");
        assert_eq!(totals.output().value(), 12);
    }
}
