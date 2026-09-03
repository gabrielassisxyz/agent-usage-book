//! Unit, integration and contract tests for `aub spend --credits` (`aub-ai3.5`).
//!
//! The end-to-end half lives in `tests/e2e/cases/017-spend-credits.sh`, which is the
//! only place the whole path (transcripts, ingest, an activated model and the release
//! binary) is exercised at once.

use std::collections::BTreeMap;

use agent_usage_book::cost_model::convert;
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::provenance::{DerivationId, EvidenceId, QuerySemantics};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{
    CoverageCompleteness, Derivation, EvidenceQuality, Provenance, RequiredFact,
};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{spend_json, validate_spend_report_json};
use agent_usage_book::presentation::render::render_spend_report;
use agent_usage_book::report::{
    IngestSummary, LedgerGeneration, ReportMetadata, SpendGroup, SpendGroupCreditsProvenance,
    SpendGroupProvenance, SpendReport,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::cost_model::{
    activate, anthropic_claude_messages_incomplete_v1, anthropic_claude_messages_v1, load_active_at,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;

const COMPLETE_MODEL: &str = "anthropic-claude-messages-v1";
const INCOMPLETE_MODEL: &str = "anthropic-claude-messages-incomplete-v1";

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageVector {
    UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(cache_write),
        ),
        BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    )
}

fn node() -> agent_usage_book::report::ProvenanceNode {
    agent_usage_book::report::ProvenanceNode::new(
        vec![EvidenceId::new("ev-credits-1")],
        vec![],
        QuerySemantics::new("day", "2026-08-25..2026-08-26"),
        1,
        1,
        agent_usage_book::report::ValueArithmetic::Sum,
    )
}

/// A one-group spend report over a fixed day, with whatever credit derivation the
/// caller wants on the group. Mirrors what `assemble_canonical` builds, without the
/// ingest path the end-to-end case covers.
fn report_with(credits: Option<Derivation<Credits>>, model: Option<&str>) -> SpendReport {
    let key = LogicalName::new("day=2026-08-25");
    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![EvidenceId::new("ev-credits-1")],
        vec![],
        QuerySemantics::new("day", "2026-08-25..2026-08-26"),
    );
    let mut group = SpendGroup::new(
        key.clone(),
        usage(100_000, 20_000, 50_000, 10_000),
        Provenance::new(["transcripts/claude-code/session.jsonl".to_string()]),
        DerivationId::from_manifest(&manifest),
    );
    let mut credit_provenance = Vec::new();
    if let Some(credits) = credits {
        group = group.with_credits(credits);
        credit_provenance.push(SpendGroupCreditsProvenance::new(key.clone(), node()));
    }
    let now = UtcTimestamp::from_unix_nanos(2_000);
    SpendReport::new(
        ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        UtcDate::parse("2026-08-25").unwrap(),
        UtcDate::parse("2026-08-26").unwrap(),
        vec![group],
        vec![SpendGroupProvenance::new(key, node())],
        IngestSummary::default(),
    )
    .with_credit_model(model.map(agent_usage_book::domain::provenance::CostModelId::new))
    .with_credit_provenance(credit_provenance)
}

fn open_migrated(name: &str) -> (tempdir::Scratch, rusqlite::Connection) {
    let scratch = tempdir::Scratch::new(name);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(
        &scratch.path().join("ledger.db"),
        AccessMode::ReadWrite,
        &policy,
    )
    .expect("db must open");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
    run_migrations(&mut conn, &registry(), None, &clock).expect("migrations must apply");
    (scratch, conn)
}

mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct Scratch(PathBuf);

    impl Scratch {
        pub fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-spend-credits-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Unit: a report nobody asked credits of carries none, in either surface. The token
/// report is byte-for-byte what it was before the dimension existed.
#[test]
fn credits_are_absent_unless_requested() {
    let report = report_with(None, None);

    let text = render_spend_report(&report);
    assert!(
        !text.contains("credits"),
        "unrequested credits leaked: {text}"
    );
    assert!(text.contains(
        "day=2026-08-25  input 100000 tokens · output 20000 tokens · cache read 50000 tokens · cache write 10000 tokens (complete)"
    ));

    let json = spend_json(&report, RunId::from_string("run-no-credits".to_string()));
    assert!(
        !json.contains("\"credits\""),
        "unrequested credits leaked: {json}"
    );
    assert!(!json.contains("credit_model"));
    validate_spend_report_json(&json).expect("a report without credits must still validate");
}

/// Unit: every modeled kind contributes its exact term, and the amount, unit and
/// qualification all reach both surfaces.
///
/// 100k input at 3.0, 20k output at 15.0, 50k cache read at 0.30 and 10k cache write
/// at 3.75 credits per million: 300_000 + 300_000 + 15_000 + 37_500 micro-credits.
#[test]
fn every_modeled_token_kind_contributes_its_exact_term() {
    let model = anthropic_claude_messages_v1(UtcTimestamp::from_unix_nanos(1_000));
    let derivation = convert(&model, &usage(100_000, 20_000, 50_000, 10_000));
    match &derivation {
        Derivation::Available(qualified) => {
            let (credits, coverage, quality, provenance) = qualified.clone().into_parts();
            assert_eq!(credits, Credits::from_micros(652_500));
            assert_eq!(coverage, CoverageCompleteness::Complete);
            assert_eq!(quality, EvidenceQuality::Measured);
            assert!(
                provenance
                    .sources()
                    .contains(&format!("cost-model:{COMPLETE_MODEL}"))
            );
        }
        Derivation::Unavailable { missing, .. } => panic!("expected a conversion: {missing:?}"),
    }

    let report = report_with(Some(derivation), Some(COMPLETE_MODEL));

    let text = render_spend_report(&report);
    assert!(text.contains(&format!(
        "converted to credits under cost model {COMPLETE_MODEL}"
    )));
    assert!(text.contains("0.65 credits (complete)"), "{text}");
    assert!(
        text.contains("input 100000 tokens"),
        "tokens must survive: {text}"
    );

    let json = spend_json(&report, RunId::from_string("run-credits".to_string()));
    assert!(
        json.contains("\"credits\":{\"value\":\"0.65\",\"unit\":\"credits\""),
        "{json}"
    );
    assert!(
        json.contains(&format!("cost-model:{COMPLETE_MODEL}")),
        "{json}"
    );
    assert!(
        json.contains(&format!("\"credit_model\":\"{COMPLETE_MODEL}\"")),
        "{json}"
    );
    assert!(json.contains("\"coverage\""), "{json}");
    validate_spend_report_json(&json).expect("a credited report must validate");
}

/// Unit: the defect this project exists to repair. Cache-write tokens against a model
/// with no cache-write term produce a refusal naming the kind, and the token counts
/// are still reported: a missing term suppresses the credit total, never the evidence.
#[test]
fn a_missing_term_blocks_the_total_and_names_the_kind() {
    let model = anthropic_claude_messages_incomplete_v1(UtcTimestamp::from_unix_nanos(1_000));
    let derivation = convert(&model, &usage(100_000, 20_000, 50_000, 10_000));
    let missing = match &derivation {
        Derivation::Unavailable { missing, .. } => missing.clone(),
        Derivation::Available(_) => panic!("a model without a cache-write term must refuse"),
    };
    assert!(
        missing
            .iter()
            .any(|fact| RequiredFact::as_str(fact).contains("cache")),
        "the refusal must name the kind: {missing:?}"
    );

    let report = report_with(Some(derivation), Some(INCOMPLETE_MODEL));

    let text = render_spend_report(&report);
    assert!(text.contains("credits unavailable:"), "{text}");
    assert!(
        text.contains("cache write 10000 tokens"),
        "tokens must survive: {text}"
    );
    assert!(
        !text.contains("0.00 credits"),
        "a refusal must never render a zero: {text}"
    );

    let json = spend_json(&report, RunId::from_string("run-incomplete".to_string()));
    assert!(
        json.contains("\"credits\":{\"status\":\"unavailable\",\"unit\":\"credits\""),
        "{json}"
    );
    assert!(json.contains("\"missing\":["), "{json}");
    validate_spend_report_json(&json).expect("a refusing report must validate");
}

/// Unit: a caller that asked for credits with no model active gets a derivation whose
/// missing fact is the model itself, not a silent absence indistinguishable from
/// never having asked.
#[test]
fn a_request_with_no_active_model_refuses_by_naming_the_model() {
    let (_scratch, conn) = open_migrated("no-active");
    let at = UtcTimestamp::from_unix_nanos(2_000_000_000);
    assert!(
        load_active_at(&conn, at)
            .expect("lookup must succeed")
            .is_none(),
        "a migrated ledger activates no cost model on its own"
    );

    let derivation: Derivation<Credits> = Derivation::unavailable(
        [RequiredFact::new("active cost model")],
        Provenance::new(["cost-model:unavailable".to_string()]),
    )
    .expect("a named missing fact");
    let report = report_with(Some(derivation), None);

    let text = render_spend_report(&report);
    assert!(
        text.contains("credits requested with no active cost model"),
        "{text}"
    );
    assert!(
        text.contains("credits unavailable: active cost model"),
        "{text}"
    );
}

/// Integration: activation and append-only supersession move which model the report
/// converts against. The call that resolves the model is identical across both halves;
/// only the ledger changed, and neither a source edit nor a configuration key was
/// involved.
#[test]
fn supersession_moves_the_selected_model_without_a_source_edit() {
    let (_scratch, mut conn) = open_migrated("supersession");
    let first = UtcTimestamp::from_unix_nanos(2_000_000_000);
    let second = UtcTimestamp::from_unix_nanos(3_000_000_000);
    let sample = usage(100_000, 20_000, 50_000, 10_000);

    let complete = anthropic_claude_messages_v1(first);
    activate(&mut conn, &complete, first, None).expect("first activation must succeed");

    let active = load_active_at(&conn, first)
        .expect("lookup")
        .expect("a model is active");
    assert_eq!(active.id().as_str(), COMPLETE_MODEL);
    assert!(matches!(
        convert(&active, &sample),
        Derivation::Available(_)
    ));

    let incomplete = anthropic_claude_messages_incomplete_v1(second);
    activate(&mut conn, &incomplete, second, Some(complete.id()))
        .expect("supersession must succeed");

    let active = load_active_at(&conn, second)
        .expect("lookup")
        .expect("a model is active");
    assert_eq!(active.id().as_str(), INCOMPLETE_MODEL);
    assert!(matches!(
        convert(&active, &sample),
        Derivation::Unavailable { .. }
    ));

    // Append-only: the superseded model is still the one active at its own instant.
    let earlier = load_active_at(&conn, first)
        .expect("lookup")
        .expect("a model is active");
    assert_eq!(earlier.id().as_str(), COMPLETE_MODEL);
}

/// Contract: the two surfaces agree. The same report rendered as text and as JSON
/// carries the same amount, the same qualification and the same model identifier, so
/// a consumer switching format learns nothing new and loses nothing.
#[test]
fn human_and_json_agree_on_amount_qualification_and_model_identity() {
    let model = anthropic_claude_messages_v1(UtcTimestamp::from_unix_nanos(1_000));
    let report = report_with(
        Some(convert(&model, &usage(100_000, 20_000, 50_000, 10_000))),
        Some(COMPLETE_MODEL),
    );

    let text = render_spend_report(&report);
    let json = spend_json(&report, RunId::from_string("run-contract".to_string()));

    for surface in [&text, &json] {
        assert!(
            surface.contains("0.65"),
            "amount must appear in both: {surface}"
        );
        assert!(
            surface.contains("complete"),
            "qualification must appear in both: {surface}"
        );
        assert!(
            surface.contains(COMPLETE_MODEL),
            "the model identifier must appear in both: {surface}"
        );
    }
    assert!(text.contains("credits"));
    assert!(json.contains("\"unit\":\"credits\""));
}
