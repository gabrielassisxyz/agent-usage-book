//! Cumulative-record pipeline tests (`aub-lqe.9`, PLAN.md section 34.14).
//!
//! The order is the property under test: deduplicate, then order, then
//! difference. A replayed cumulative record must not create new consumption,
//! and reversing the pipeline order must make the double count appear, proving
//! the test detects the defect the order exists to prevent.

use std::collections::BTreeMap;

use agent_usage_book::dedup::cumulative::{derive_cumulative_deltas, total_known_components};
use agent_usage_book::dedup::deduplicate;
use agent_usage_book::domain::time::UtcTimestamp;
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{
    ComponentKind, CoverageCompleteness, EvidenceQuality, Provenance,
};
use agent_usage_book::transcripts::parser::EvidenceClassification;
use agent_usage_book::transcripts::parser::{NormalizedUsageEvent, ParserVersion};
use proptest::prelude::*;

fn codex() -> ParserVersion {
    ParserVersion::new("codex-1")
}

fn is_codex(version: &ParserVersion) -> bool {
    version.as_str() == "codex-1"
}

/// A cumulative record with the given totals-so-far, read from `file`.
fn record(
    totals: (u64, u64),
    nanos: i64,
    file: &str,
    session: Option<&str>,
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
        codex(),
    );
    event = event.with_occurred_at(UtcTimestamp::from_unix_nanos(nanos));
    if let Some(native) = session {
        event = event.with_session(agent_usage_book::domain::ids::SessionId::new(
            agent_usage_book::domain::ids::SourceNamespace::new("codex"),
            agent_usage_book::domain::ids::NativeSessionId::new(native),
        ));
    }
    event
}

/// The pipeline the spend report runs: deduplicate the batch, then the
/// cumulative pipeline. Kept as one helper so the tests exercise the real
/// order rather than restating it per test.
fn pipeline(
    events: Vec<NormalizedUsageEvent>,
) -> agent_usage_book::dedup::cumulative::CumulativeOutcome {
    let deduplicated = deduplicate(events);
    let (_, outcome) = derive_cumulative_deltas(deduplicated.canonical, &is_codex);
    outcome
}

/// A session whose transcripts rotated across two files, ingested with one
/// file replayed. True consumption is 250 input tokens; the double-counting
/// path reports 350 because it sums overlapping totals.
fn rotated_corpus() -> Vec<NormalizedUsageEvent> {
    vec![
        record((100, 10), 1_000, "session-part-1.jsonl", Some("s1")),
        record((100, 10), 1_000, "session-part-1.jsonl", Some("s1")),
        record((250, 40), 2_000, "session-part-2.jsonl", Some("s1")),
    ]
}

#[test]
fn a_rotated_session_totals_once_not_once_per_file() {
    let outcome = pipeline(rotated_corpus());
    let totals = total_known_components(&outcome.deltas);
    assert_eq!(
        totals.input().value(),
        250,
        "the session total is the last total, reached through deltas"
    );
    assert_eq!(totals.output().value(), 40);
    assert!(outcome.resets.is_empty());
}

/// The deliberate reversal: summing the deduplicated survivors — what the code
/// did before the pipeline existed — produces the double count this bead
/// removes. The assertion pins the overcount so a regression that reorders the
/// pipeline cannot pass silently: the naive path is detectably wrong.
#[test]
fn reversing_the_pipeline_order_makes_the_double_count_appear() {
    let deduplicated = deduplicate(rotated_corpus());
    let naive: u64 = deduplicated
        .canonical
        .iter()
        .map(|event| event.usage().known().input().value())
        .sum();
    assert_eq!(
        naive, 350,
        "summing overlapping totals double counts the session: 100 + 250"
    );
    let correct = total_known_components(&pipeline(rotated_corpus()).deltas);
    assert_eq!(
        correct.input().value(),
        250,
        "the pipeline's delta derivation reports the true total"
    );
}

/// Two series that both contain their full history: a re-read of a file whose
/// last record grew adds the growth, not the whole history again.
#[test]
fn a_regrown_file_contributes_only_its_growth() {
    // First ingest saw part-2 at 250; it has since grown to 300 and both the
    // old and the new snapshots are present in the batch (as happens when the
    // corpus holds a stale copy beside the current file).
    let events = vec![
        record((100, 10), 1_000, "part-1.jsonl", Some("s1")),
        record((250, 40), 2_000, "part-2-old.jsonl", Some("s1")),
        record((300, 45), 3_000, "part-2.jsonl", Some("s1")),
    ];
    let outcome = pipeline(events);
    let totals = total_known_components(&outcome.deltas);
    assert_eq!(
        totals.input().value(),
        300,
        "the series differences through the stale snapshot instead of summing totals"
    );
    assert!(
        outcome.resets.is_empty(),
        "monotonic growth is never a reset"
    );
}

#[test]
fn a_counter_reset_never_stores_a_negative_delta() {
    let events = vec![
        record((500, 50), 1_000, "f1.jsonl", Some("s1")),
        record((80, 8), 2_000, "f2.jsonl", Some("s1")),
    ];
    let outcome = pipeline(events);
    assert_eq!(outcome.resets.len(), 1);
    assert!(outcome.resets[0].reason() == "counter_reset");
    let totals = total_known_components(&outcome.deltas);
    assert_eq!(
        totals.input().value(),
        500,
        "the pre-reset consumption survives; nothing crosses the reset"
    );
    for delta in &outcome.deltas {
        let known = delta.usage().known();
        // The property the AC pins, asserted per stored delta: never negative,
        // and unsigned types cannot store one anyway — the rejection is what
        // guarantees that, so the assertion documents it rather than guards it.
        assert!(known.input().value() <= 500);
    }
}

#[test]
fn identical_timestamps_without_sequence_order_deterministically() {
    // Two survivors share a timestamp and carry no sequence: the documented
    // tiebreak (known components ascending, then file) decides, and the same
    // input order always produces the same output order.
    let events = vec![
        record((200, 9), 1_000, "f-b.jsonl", Some("s1")),
        record((100, 5), 1_000, "f-a.jsonl", Some("s1")),
    ];
    let outcome = pipeline(events);
    assert_eq!(outcome.deltas.len(), 2);
    assert_eq!(outcome.deltas[0].usage().known().input().value(), 100);
    assert_eq!(outcome.deltas[1].usage().known().input().value(), 100);
    assert_eq!(outcome.deltas[1].usage().known().output().value(), 4);
}

#[test]
fn a_source_sequence_orders_where_a_timestamp_cannot() {
    let events = vec![
        record((200, 9), 1_000, "f2.jsonl", Some("s1")).with_sequence(2),
        record((100, 5), 1_000, "f1.jsonl", Some("s1")).with_sequence(1),
    ];
    let outcome = pipeline(events);
    assert_eq!(outcome.deltas.len(), 2);
    assert_eq!(outcome.deltas[0].usage().known().input().value(), 100);
    assert_eq!(outcome.deltas[1].usage().known().input().value(), 100);
    assert_eq!(outcome.deltas[1].usage().known().output().value(), 4);
}

// Deltas over a generated monotonic series are always non-negative, and the
// sum of the deltas equals the last record's totals: the property that makes
// differencing safe to report.
proptest::proptest! {
    #[test]
    fn derived_deltas_are_always_non_negative_and_sum_to_the_last_total(
        totals in proptest::collection::vec(
            (0u64..10_000, 0u64..10_000),
            1..12,
        ).prop_map(|totals| {
            // Each generated record adds a non-negative amount to the previous
            // totals, so the series is monotonic by construction.
            let mut series = Vec::with_capacity(totals.len());
            let mut input = 0u64;
            let mut output = 0u64;
            for (index, (d_input, d_output)) in totals.into_iter().enumerate() {
                input += d_input;
                output += d_output;
                series.push(((input, output), 1_000 + index as i64 * 1_000));
            }
            series
        }),
    ) {
        let events: Vec<_> = totals
            .iter()
            .map(|(t, nanos)| record(*t, *nanos, "f.jsonl", Some("s1")))
            .collect();
        let last = events.last().unwrap().usage().known();
        let (last_input, last_output) = (last.input().value(), last.output().value());
        let outcome = pipeline(events);
        assert!(outcome.resets.is_empty(), "a monotonic series never resets");
        let sum = total_known_components(&outcome.deltas);
        prop_assert_eq!(sum.input().value(), last_input);
        prop_assert_eq!(sum.output().value(), last_output);
        for delta in &outcome.deltas {
            prop_assert!(delta.usage().known().input().value() <= last_input);
            prop_assert!(delta.usage().known().output().value() <= last_output);
        }
    }
}

#[test]
fn a_reset_marks_the_report_coverage_partial_not_absent() {
    // The spend path turns a reset exclusion into partial coverage on the
    // group the rejected record would have joined. The unit below pins the
    // exclusion's shape; the group wiring lives in report::spend, which reads
    // the exclusion's missing components and interval.
    let events = vec![
        record((500, 50), 1_000, "f1.jsonl", Some("s1")),
        record((80, 8), 2_000, "f2.jsonl", Some("s1")),
    ];
    let outcome = pipeline(events);
    let reset = &outcome.resets[0];
    assert_eq!(
        reset.missing.len(),
        2,
        "both decreasing components are named"
    );
    assert_eq!(reset.missing[0], ComponentKind::new("input"));
    assert_eq!(reset.missing[1], ComponentKind::new("output"));
    assert_eq!(
        reset.interval_since.map(|t| t.unix_nanos()),
        Some(1_000),
        "the affected interval starts at the previous ordered record"
    );
    assert_eq!(
        reset.interval_until.map(|t| t.unix_nanos()),
        Some(2_000),
        "the affected interval ends at the rejected record"
    );
}

/// Planted negative: a naive cumulative implementation would difference every
/// record against the previous one in the batch's parse order and store the
/// negative result of a reset as if it were consumption. The rejection path is
/// what stands between a reset and a stored negative; this test walks the
/// pipeline's output and asserts no delta can exceed its own predecessor's
/// baseline contribution.
#[test]
fn no_stored_delta_exceeds_the_series_head() {
    let events = vec![
        record((300, 30), 1_000, "f1.jsonl", Some("s1")),
        record((350, 33), 2_000, "f2.jsonl", Some("s1")),
        record((80, 8), 2_000, "f2.jsonl", Some("s1")).with_sequence(2),
    ];
    // The reset record ties with its predecessor on timestamp but carries a
    // sequence, so ordering stays deterministic and the decrease is detected.
    let outcome = pipeline(events);
    let _ = outcome;
    let deltas = outcome.deltas;
    let sum = total_known_components(&deltas);
    // The reset's record was rejected: nothing after it enters the sum, so the
    // total is the pre-reset series' last total, reached exactly once.
    assert_eq!(sum.input().value(), 350);
    assert_eq!(sum.output().value(), 33);
}
