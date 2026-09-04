//! One cross-cutting suite for the two invariants of PLAN.md sections 3.5, 31 and
//! 34.5, proved uniformly across every source rather than one test per source
//! scattered through the tree (`aub-71j.6`):
//!
//!  * **Zero is data.** A zero is produced only by evidence (a provider reporting
//!    0% used, a usage event with zero cache writes) or by valid arithmetic (an
//!    all-zero usage vector priced to zero). A missing transcript, an HTTP
//!    failure, a missing price and a stale meter are never zero.
//!  * **No silent fallback.** When a source fails, the old numeric value never
//!    reappears under a fresh label and zero never replaces it. The failure
//!    reason is present. Provider meter and projection readings *may* still show
//!    a historical value, but only carried with its original observation time and
//!    an explicit stale reason. Cost models, rate cards, calibrations and task
//!    attribution have no last-good arm at all: a witness inapplicable to the
//!    current query is never reused.
//!
//! The eight sources the bead enumerates: provider meter, transcript file,
//! transcript record, cost model, rate card, calibration, task tracker,
//! projection. Compile-time quantity-default bans live in `tests/compile_fail/`;
//! this suite is the runtime half.
//!
//! Each guard here has been broken once by hand to confirm it discriminates; the
//! mutation, the test it broke and the line it printed are recorded on the bead.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::attribution::{
    ResolvedTaskKind, TaskIdentityState, TaskKind, TaskKindCandidate, TaskKindMapping,
    TaskKindOrigin, resolve_task_kind,
};
use agent_usage_book::calibration::health::{
    ApplicabilityContext, CalibrationFacts, CalibrationHealth, HealthInputs, LifecycleState,
    compute_health, require_current_applicable,
};
use agent_usage_book::cost_model::convert;
use agent_usage_book::coverage::CoverageFraction;
use agent_usage_book::domain::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::freshness::{
    Freshness, FreshnessInput, FreshnessKind, LatestAttempt, Observed, StaleReason,
    compute_freshness,
};
use agent_usage_book::domain::ids::{
    BillingSemanticsId, CredentialContextId, MeterSemanticsId, NativeTaskId, SourceNamespace,
    TaskId,
};
use agent_usage_book::domain::money::Usd;
use agent_usage_book::domain::provenance::{CostModelId, DerivationId, EvidenceId};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining, QuotaUsed};
use agent_usage_book::domain::rate_card::{
    BillingBasis, CurrencyCode, Publication, RateCard, RateCardDraft, ReviewDuePolicy, TokenClass,
};
use agent_usage_book::domain::time::{
    ClockSkewEnvelope, FakeClock, MeasurementBasis, MonotonicDuration, ProviderObservedAt,
    ReceivedAt, UtcDate, UtcTimestamp,
};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{
    CoverageCompleteness, Derivation, EvidenceQuality, Provenance, RequiredFact,
};
use agent_usage_book::ingest::{IngestOptions, run as run_ingest};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{
    freshness_json, spend_json, validate_spend_report_json,
};
use agent_usage_book::presentation::precision::PERCENT;
use agent_usage_book::presentation::render::{render_meter_reading, render_spend_report};
use agent_usage_book::report::{
    IngestSummary, LedgerGeneration, ReportMetadata, SpendGroup, SpendGroupCreditsProvenance,
    SpendGroupProvenance, SpendReport,
};
use agent_usage_book::store::cost_model::{
    activate, anthropic_claude_messages_incomplete_v1, anthropic_claude_messages_v1, load_active_at,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::transcripts::native::ClaudeCodeParser;
use agent_usage_book::transcripts::parser::{ParserAdapter, QuarantineClass, SourceLocation};

// ---------------------------------------------------------------------------
// shared fixtures
// ---------------------------------------------------------------------------

const HORIZON_SECONDS: u64 = 300;
const COMMAND_HORIZON_SECONDS: u64 = 10;

fn envelope() -> ClockSkewEnvelope {
    ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10))
}

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

/// A last-good meter observation of `remaining_ppm`, observed by the provider at
/// `observed_nanos` and received locally at the same instant.
fn meter_observation(remaining_ppm: u32, observed_nanos: i64) -> Observed<QuotaRemaining> {
    Observed::new(
        QuotaRemaining::new(QuotaFractionPpm::new(remaining_ppm as i32).unwrap()),
        Some(ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(
            observed_nanos,
        ))),
        ReceivedAt::new(UtcTimestamp::from_unix_nanos(observed_nanos)),
        MeasurementBasis::ProviderObserved,
    )
}

/// Feeds the production freshness state machine one last-good observation and one
/// terminal attempt outcome, read `read_nanos` after the observation.
fn freshness_after(
    last_good: Option<Observed<QuotaRemaining>>,
    outcome: AttemptOutcome,
    observed_nanos: i64,
    read_nanos: i64,
    ctx: &CredentialContextId,
) -> Freshness<QuotaRemaining> {
    let started = AttemptStarted::new(
        AttemptId::new(7),
        UtcTimestamp::from_unix_nanos(observed_nanos + 1_000),
    );
    let result = AttemptResult::new(
        AttemptId::new(7),
        UtcTimestamp::from_unix_nanos(observed_nanos + 1_000),
        MonotonicDuration::from_seconds(0),
        outcome,
    );
    let input = FreshnessInput::new(
        last_good,
        Some(ctx),
        Some(LatestAttempt::new(started, Some(result), ctx)),
        None,
        Some(ctx),
        MonotonicDuration::from_seconds(HORIZON_SECONDS),
        MonotonicDuration::from_seconds(COMMAND_HORIZON_SECONDS),
        envelope(),
    );
    compute_freshness(
        &input,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(read_nanos)),
    )
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aub-zero-is-data-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
        ScratchDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Restore read permission so an unreadable-file fixture can be removed.
        if let Ok(entries) = std::fs::read_dir(self.0.join("claude-code")) {
            for entry in entries.flatten() {
                let _ =
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o644));
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn migrated_conn(scratch: &ScratchDir) -> rusqlite::Connection {
    let policy = agent_usage_book::store::connection::PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = agent_usage_book::store::connection::open(
        &scratch.path().join("ledger.db"),
        agent_usage_book::store::connection::AccessMode::ReadWrite,
        &policy,
    )
    .expect("db must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrations must apply");
    conn
}

#[allow(clippy::too_many_arguments)]
fn rate_card(
    id: i64,
    vendor: &str,
    model: &str,
    token_class: TokenClass,
    rate_micros: i64,
    start: &str,
    end: Option<&str>,
) -> RateCard {
    RateCard {
        id,
        imported_at: UtcTimestamp::from_unix_nanos(100),
        draft: RateCardDraft {
            vendor: vendor.to_string(),
            model: model.to_string(),
            token_class,
            rate_micros,
            currency: CurrencyCode::Usd,
            billing_basis: BillingBasis::PerMillionTokens,
            effective_start: UtcDate::parse(start).expect("valid start date"),
            effective_end: end.map(|d| UtcDate::parse(d).expect("valid end date")),
            publication: Publication {
                source: Some("https://pricing.example".to_string()),
                published_at: None,
            },
            review_due: ReviewDuePolicy::None,
        },
    }
}

/// A one-group spend report carrying whatever credit derivation the caller wants,
/// mirroring what `assemble_canonical` builds without the ingest path. Adapted
/// from `tests/spend_credits.rs::report_with`.
fn spend_report_with_credits(
    credits: Option<Derivation<Credits>>,
    model: Option<&str>,
) -> SpendReport {
    let key = LogicalName::new("day=2026-08-25");
    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![EvidenceId::new("ev-1")],
        vec![],
        agent_usage_book::domain::provenance::QuerySemantics::new("day", "2026-08-25..2026-08-26"),
    );
    let mut group = SpendGroup::new(
        key.clone(),
        usage(100_000, 20_000, 50_000, 10_000),
        Provenance::new(["transcripts/claude-code/session.jsonl".to_string()]),
        DerivationId::from_manifest(&manifest),
    );
    let node = || {
        agent_usage_book::report::ProvenanceNode::new(
            vec![EvidenceId::new("ev-1")],
            vec![],
            agent_usage_book::domain::provenance::QuerySemantics::new(
                "day",
                "2026-08-25..2026-08-26",
            ),
            1,
            1,
            agent_usage_book::report::ValueArithmetic::Sum,
        )
    };
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
    .with_credit_model(model.map(CostModelId::new))
    .with_credit_provenance(credit_provenance)
}

// ===========================================================================
// 1. Provider meter and projection: MAY show a historical value, only with its
//    own timestamp and an explicit stale reason, never a zero or a fresh label.
// ===========================================================================

/// Criterion 1, 2, 3: a transport failure after a good observation is `Stale`,
/// carries the exact historical `Observed` (its provider timestamp intact) and a
/// named reason, and is never `Fresh` and never zero.
#[test]
fn meter_failure_after_a_good_observation_keeps_the_historical_value_with_its_timestamp() {
    let ctx = CredentialContextId::new("ctx-meter");
    let observed_nanos = 1_000_000_000_000;
    let good = meter_observation(380_000, observed_nanos);

    let reading = freshness_after(
        Some(good.clone()),
        AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        observed_nanos,
        observed_nanos + 5_000,
        &ctx,
    );

    let Freshness::Stale {
        last_good: Some(carried),
        reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        ..
    } = &reading
    else {
        panic!("a transport failure after a success must be Stale(SourceUnreachable): {reading:?}");
    };
    assert_eq!(
        carried, &good,
        "the historical value and its original observation time travel unchanged"
    );
    assert_ne!(reading.kind(), FreshnessKind::Fresh);

    let human = render_meter_reading(
        &reading,
        "%",
        PERCENT,
        UtcTimestamp::from_unix_nanos(observed_nanos + 5_000),
        envelope(),
        None,
    );
    assert!(
        human.contains("stale"),
        "the human line names staleness: {human}"
    );
    assert!(human.contains("timeout"), "the reason is present: {human}");
    assert!(
        human.contains("~38%"),
        "the historical value is shown, marked approximate: {human}"
    );
    assert!(
        !human.contains("left"),
        "a stale line never renders the fresh form: {human}"
    );

    let json = freshness_json("work-meter", &reading);
    assert!(json.contains("\"freshness\":\"stale\""), "{json}");
    assert!(json.contains("\"reason\":\"source_unreachable\""), "{json}");
    assert!(
        json.contains("\"last_good\":{\"value\":\"380000\",\"unit\":\"ppm\"}"),
        "the historical ppm value rides in last_good: {json}"
    );
    assert!(
        !json.contains("\"freshness\":\"fresh\""),
        "no fresh label on a failed reading: {json}"
    );
}

/// Criterion 2: a failure before any successful observation is `Stale` with no
/// value at all. It never substitutes zero.
#[test]
fn meter_failure_before_any_success_has_no_value_and_no_zero() {
    let ctx = CredentialContextId::new("ctx-cold");
    let reading = freshness_after(
        None,
        AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        1_000_000_000,
        1_000_005_000,
        &ctx,
    );

    let Freshness::Stale {
        last_good: None, ..
    } = &reading
    else {
        panic!("a cold failure must be Stale with no last_good: {reading:?}");
    };

    let human = render_meter_reading(
        &reading,
        "%",
        PERCENT,
        UtcTimestamp::from_unix_nanos(1_000_005_000),
        envelope(),
        None,
    );
    assert_eq!(
        human, "? · stale · timeout",
        "no value is invented for a source that never succeeded"
    );
    assert!(
        !human.contains('0'),
        "no zero stands in for the absent value: {human}"
    );

    let json = freshness_json("work-meter", &reading);
    assert!(json.contains("\"last_good\":null"), "{json}");
}

/// Criterion 3, 6: a provider reporting exactly 0% used is a real measured zero.
/// It renders as a plain fresh `0%`, not as an approximation and not as stale.
#[test]
fn provider_zero_used_is_a_real_fresh_zero_percent() {
    let zero_used = QuotaUsed::new(QuotaFractionPpm::new(0).unwrap());
    assert_eq!(
        zero_used.complement(),
        QuotaRemaining::new(QuotaFractionPpm::new(QuotaFractionPpm::MAX as i32).unwrap()),
        "0% used complements to 100% remaining"
    );

    let observed_nanos = 1_000_000_000_000;
    let reading = freshness_after(
        Some(meter_observation(0, observed_nanos)),
        AttemptOutcome::Success,
        observed_nanos,
        observed_nanos + 1_000,
        &CredentialContextId::new("ctx-zero"),
    );
    assert_eq!(reading.kind(), FreshnessKind::Fresh);

    let human = render_meter_reading(
        &reading,
        "%",
        PERCENT,
        UtcTimestamp::from_unix_nanos(observed_nanos + 1_000),
        envelope(),
        None,
    );
    assert_eq!(
        human, "0% left",
        "a measured zero is shown plainly: {human}"
    );

    let json = freshness_json("work-zero", &reading);
    assert!(
        json.contains("\"freshness\":\"fresh\",\"remaining\":{\"value\":\"0\",\"unit\":\"ppm\"}"),
        "the fresh zero keeps its provenance as a real reading: {json}"
    );
}

/// Criterion 1, 3, 8 (projection through the release binary): a missing
/// projection renders the degraded marker and never a substituted account
/// value, in both the human and the JSON surface.
#[test]
fn e2e_missing_projection_never_substitutes_an_account_value() {
    let scratch = ScratchDir::new("proj-missing");
    std::fs::create_dir_all(scratch.path().join("home")).unwrap();
    std::fs::create_dir_all(scratch.path().join("state")).unwrap();
    std::fs::write(
        scratch.path().join("aub.toml"),
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\n",
            scratch.path().join("state").display()
        ),
    )
    .unwrap();

    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_aub"))
            .env("HOME", scratch.path().join("home"))
            .env("AUB_CONFIG_FILE", scratch.path().join("aub.toml"))
            .args(args)
            .output()
            .expect("aub must run");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    let (code, stdout) = run(&["status"]);
    assert_eq!(code, 0, "a missing projection is a displayable state");
    assert_eq!(stdout.trim_end(), "aub ?");
    assert!(
        !stdout.contains("work-primary"),
        "no account line is fabricated: {stdout}"
    );

    let (code, stdout) = run(&["status", "--format", "json"]);
    assert_eq!(code, 0);
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(document["projection"]["state"], "missing");
    assert!(document["projection"]["reason"].is_string());
    assert_eq!(document["accounts"].as_array().map(Vec::len), Some(0));
    assert!(
        document["accounts"].as_array().unwrap().is_empty(),
        "the empty account set is explicit, not a zero-valued account: {document}"
    );
}

/// Criterion 3 (meter through the release binary): a forced sample against an
/// unreachable endpoint records the attempt, renders the stale form and never a
/// fabricated fresh reading.
#[test]
fn e2e_meter_transport_failure_records_the_attempt_and_renders_no_fresh_value() {
    let scratch = ScratchDir::new("meter-unreachable");
    std::fs::create_dir_all(scratch.path().join("home")).unwrap();
    std::fs::create_dir_all(scratch.path().join("state")).unwrap();
    std::fs::create_dir_all(scratch.path().join("creds")).unwrap();
    std::fs::write(
        scratch.path().join("creds/a.json"),
        "{\"accessToken\":\"token-a\"}",
    )
    .unwrap();
    std::fs::write(
        scratch.path().join("aub.toml"),
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-a\"\nprovider = \"anthropic\"\n\
             credential = {{ kind = \"file\", path = \"{}\" }}\n",
            scratch.path().join("state").display(),
            scratch.path().join("creds/a.json").display(),
        ),
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_aub"))
        .env("HOME", scratch.path().join("home"))
        .env("AUB_CONFIG_FILE", scratch.path().join("aub.toml"))
        .env("AUB_ANTHROPIC_ENDPOINT", "http://127.0.0.1:9")
        .args(["now", "--format", "json"])
        .output()
        .expect("aub must run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a recorded failed sample is a successful run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let document: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let account = &document["accounts"][0];
    assert_eq!(
        account["freshness"], "stale",
        "an unreachable endpoint yields a stale reading, not a fresh one: {account}"
    );
    assert_eq!(account["last_good"], serde_json::Value::Null);
    assert!(
        account.get("remaining").is_none(),
        "no remaining value is fabricated: {account}"
    );

    let conn =
        rusqlite::Connection::open(scratch.path().join("state/ledger.db")).expect("ledger opens");
    let attempts: i64 = conn
        .query_row("SELECT count(*) FROM meter_attempt", [], |r| r.get(0))
        .unwrap();
    let results: i64 = conn
        .query_row("SELECT count(*) FROM meter_attempt_result", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!((attempts, results), (1, 1), "the failed attempt is durable");
}

// ===========================================================================
// 2. Transcript sources: a bad file is named, a bad record is quarantined, and
//    the surviving evidence keeps its qualification. Neither invents an event.
// ===========================================================================

/// Criterion 1 (transcript file): an unreadable file is named in the report,
/// the readable files still land, and nothing contributes a silent zero.
#[test]
fn transcript_file_that_cannot_be_read_is_named_not_silently_dropped() {
    let scratch = ScratchDir::new("bad-file");
    let corpus = scratch.path().join("claude-code");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(
        corpus.join("good.jsonl"),
        "{\"type\":\"assistant\",\"timestamp\":\"2026-08-25T10:00:00.000Z\",\"sessionId\":\"s1\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":50,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n",
    )
    .unwrap();
    let unreadable = corpus.join("locked.jsonl");
    std::fs::write(&unreadable, "{}").unwrap();
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

    let toml = format!(
        "[[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\npattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n",
        corpus.display()
    );
    let (config, _) = agent_usage_book::config::resolve(
        &agent_usage_book::config::Overrides::new(),
        &agent_usage_book::config::FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("config resolves");

    let mut conn = migrated_conn(&scratch);
    let report = run_ingest(
        &mut conn,
        &config,
        &IngestOptions::default(),
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000)),
        &mut |_| Ok(()),
    )
    .expect("ingest completes even with an unreadable file");

    assert!(
        report
            .unreadable_files
            .iter()
            .any(|f| f.contains("locked.jsonl")),
        "the unreadable file is named: {:?}",
        report.unreadable_files
    );
    assert!(
        report.files_parsed >= 1,
        "the readable file still landed: {report:?}"
    );
}

/// Criterion 1, 5 (transcript record): a malformed record is quarantined, the
/// good record survives, and an explicit zero in the good record is complete
/// data, not a missing-evidence placeholder.
#[test]
fn transcript_malformed_record_is_quarantined_and_explicit_zero_stays_complete() {
    let input = concat!(
        r#"{"message":{"usage":{"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        "\n",
        r#"{"message":{"usage":{"input_tokens":"not-a-number"#,
    );
    let output = ClaudeCodeParser.parse(input, &SourceLocation::new("session.jsonl", 1));

    assert_eq!(output.events().len(), 1, "the good record survives");
    assert_eq!(
        output.quarantined().len(),
        1,
        "the malformed record is quarantined, not turned into a zero event"
    );
    assert_eq!(
        output.quarantined()[0].class(),
        QuarantineClass::TruncatedStructure
    );
    assert_eq!(
        output.events()[0].usage().coverage(),
        &CoverageCompleteness::Complete,
        "explicit zero counts are complete data (PLAN.md 31)"
    );
}

// ===========================================================================
// 3. Cost model, rate card, calibration, task attribution: no last-good arm.
//    A witness inapplicable to the current query is never silently reused.
// ===========================================================================

/// Criterion 1, 2, 4 (cost model): a consumed kind with no term fails closed
/// naming the kind; it never returns zero credits. The witness is the supplied
/// model, so swapping models changes the outcome with no ambient fallback.
#[test]
fn cost_model_missing_term_refuses_and_never_returns_zero_credits() {
    let complete = anthropic_claude_messages_v1(UtcTimestamp::from_unix_nanos(1_000));
    let incomplete = anthropic_claude_messages_incomplete_v1(UtcTimestamp::from_unix_nanos(1_000));
    let consumed = usage(100_000, 20_000, 50_000, 10_000);

    assert!(
        matches!(convert(&complete, &consumed), Derivation::Available(_)),
        "the complete model prices the same usage"
    );

    match convert(&incomplete, &consumed) {
        Derivation::Unavailable { missing, .. } => {
            assert!(
                missing
                    .iter()
                    .any(|f| RequiredFact::as_str(f).contains("cache")),
                "the refusal names the unpriced kind: {missing:?}"
            );
        }
        Derivation::Available(qualified) => {
            let (credits, ..) = qualified.into_parts();
            panic!("a missing term must refuse, not return {credits:?}");
        }
    }
}

/// Criterion 4 (cost model): activation and append-only supersession move which
/// model the conversion resolves against, with no source edit and no config key.
/// The superseded model never leaks forward past its own instant.
#[test]
fn cost_model_supersession_moves_the_witness_without_reusing_the_old_one() {
    let scratch = ScratchDir::new("cm-supersede");
    let mut conn = migrated_conn(&scratch);
    let first = UtcTimestamp::from_unix_nanos(2_000_000_000);
    let second = UtcTimestamp::from_unix_nanos(3_000_000_000);
    let sample = usage(100_000, 20_000, 50_000, 10_000);

    let complete = anthropic_claude_messages_v1(first);
    activate(&mut conn, &complete, first, None).expect("first activation");
    let incomplete = anthropic_claude_messages_incomplete_v1(second);
    activate(&mut conn, &incomplete, second, Some(complete.id())).expect("supersession");

    let active_now = load_active_at(&conn, second)
        .unwrap()
        .expect("a model is active");
    assert!(
        matches!(
            convert(&active_now, &sample),
            Derivation::Unavailable { .. }
        ),
        "the current query uses the current (incomplete) model, not the retired one"
    );
    let active_then = load_active_at(&conn, first)
        .unwrap()
        .expect("a model is active");
    assert!(
        matches!(convert(&active_then, &sample), Derivation::Available(_)),
        "the retired model is still the one active at its own instant, never later"
    );
}

/// Criterion 6, 7 (cost model): an all-zero usage vector is priced to zero
/// credits by valid arithmetic, with full provenance and measured quality. This
/// is the valid-zero that contrasts with the missing-term refusal above.
#[test]
fn zero_usage_converts_to_a_real_zero_credit_amount_with_provenance() {
    let model = anthropic_claude_messages_v1(UtcTimestamp::from_unix_nanos(1_000));
    match convert(&model, &usage(0, 0, 0, 0)) {
        Derivation::Available(qualified) => {
            let (credits, coverage, quality, provenance) = qualified.into_parts();
            assert_eq!(credits, Credits::from_micros(0), "zero usage costs zero");
            assert_eq!(coverage, CoverageCompleteness::Complete);
            assert_eq!(quality, EvidenceQuality::Measured);
            assert!(
                provenance
                    .sources()
                    .iter()
                    .any(|s| s.starts_with("cost-model:")),
                "the zero carries the model that produced it: {provenance:?}"
            );
        }
        Derivation::Unavailable { missing, .. } => {
            panic!("an all-zero vector is valid arithmetic, not missing evidence: {missing:?}")
        }
    }
}

/// Criterion 4 (rate card): a card outside the queried date is not reused. The
/// query date decides applicability; an expired card yields `Incomplete`, never
/// a `Complete` zero, while an in-window date over the same book is `Complete`.
#[test]
fn rate_card_outside_its_window_is_not_reused_for_a_later_date() {
    let book = agent_usage_book::valuation::RateBook::new(vec![rate_card(
        1,
        "vendor",
        "model-a",
        TokenClass::Input,
        2_000_000,
        "2024-01-01",
        Some("2024-06-30"),
    )]);
    let priced = usage(10_000, 0, 0, 0);

    let in_window = agent_usage_book::valuation::value_usage_vector::<Usd>(
        &book,
        "vendor",
        "model-a",
        UtcDate::parse("2024-03-15").unwrap(),
        &priced,
    );
    assert!(
        matches!(
            in_window,
            agent_usage_book::valuation::ValuationOutcome::Complete(_)
        ),
        "the card values a date it covers"
    );

    let after_window = agent_usage_book::valuation::value_usage_vector::<Usd>(
        &book,
        "vendor",
        "model-a",
        UtcDate::parse("2024-12-01").unwrap(),
        &priced,
    );
    match after_window {
        agent_usage_book::valuation::ValuationOutcome::Incomplete { missing_rates, .. } => {
            assert_eq!(missing_rates.len(), 1);
            assert_eq!(missing_rates[0].token_class, "input");
            assert_eq!(missing_rates[0].date, UtcDate::parse("2024-12-01").unwrap());
        }
        other => panic!("an out-of-window date must not reuse the expired card: {other:?}"),
    }
}

/// Criterion 4 (calibration): a calibration fitted against a different plan tier
/// is `Inapplicable` and `require_current_applicable` refuses it naming the
/// state. A matching, active calibration is `Current` and is accepted.
#[test]
fn calibration_for_a_different_tier_is_inapplicable_and_refused() {
    let fitted = CalibrationFacts {
        plan_tier: agent_usage_book::store::calibration::PlanTier::new("pro-5h"),
        meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
        billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
    };
    let now = UtcTimestamp::from_unix_nanos(10_000);

    let drifted_context = ApplicabilityContext {
        plan_tier: agent_usage_book::store::calibration::PlanTier::new("team-5h"),
        meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
        billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
    };
    let inapplicable = compute_health(
        &HealthInputs {
            calibration: &fitted,
            context: &drifted_context,
            lifecycle: LifecycleState::Active,
            cost_model_superseded: false,
            drift: None,
            review_due_at: None,
        },
        now,
    );
    assert_eq!(inapplicable, CalibrationHealth::Inapplicable);
    let refusal = require_current_applicable(inapplicable)
        .expect_err("an inapplicable calibration must be refused");
    assert_eq!(refusal.health, CalibrationHealth::Inapplicable);

    let matching_context = ApplicabilityContext {
        plan_tier: agent_usage_book::store::calibration::PlanTier::new("pro-5h"),
        meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
        billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
    };
    let current = compute_health(
        &HealthInputs {
            calibration: &fitted,
            context: &matching_context,
            lifecycle: LifecycleState::Active,
            cost_model_superseded: false,
            drift: None,
            review_due_at: None,
        },
        now,
    );
    assert_eq!(current, CalibrationHealth::Current);
    require_current_applicable(current).expect("a current applicable calibration is accepted");
}

/// Criterion 4 (task attribution): equal-rank evidence that disagrees is a
/// `Conflict` with no winner chosen by order; a persisted state label this code
/// does not know parses to `None`, never a silent default.
#[test]
fn task_attribution_conflict_never_invents_a_winner() {
    let mapping = TaskKindMapping::new(
        3,
        [
            ("bug".to_string(), TaskKind::Bug),
            ("docs".to_string(), TaskKind::Docs),
        ],
    )
    .expect("mapping builds");
    let task = TaskId::new(SourceNamespace::new("beads"), NativeTaskId::new("aub-1"));
    let disagreeing = [
        TaskKindCandidate {
            task_id: task.clone(),
            origin: TaskKindOrigin::TrackerField("issue_type".to_string()),
            raw_value: "bug".to_string(),
        },
        TaskKindCandidate {
            task_id: task.clone(),
            origin: TaskKindOrigin::TrackerField("category".to_string()),
            raw_value: "docs".to_string(),
        },
    ];
    let both_orders = {
        let mut reversed = disagreeing.clone();
        reversed.reverse();
        [
            resolve_task_kind(&disagreeing, &mapping),
            resolve_task_kind(&reversed, &mapping),
        ]
    };
    for resolved in &both_orders {
        assert_eq!(
            resolved.state(),
            TaskIdentityState::Conflict,
            "no winner is selected by input order: {resolved:?}"
        );
        assert!(!matches!(resolved, ResolvedTaskKind::Resolved { .. }));
    }

    let agreeing = [
        disagreeing[0].clone(),
        TaskKindCandidate {
            raw_value: "bug".to_string(),
            ..disagreeing[1].clone()
        },
    ];
    assert!(
        matches!(
            resolve_task_kind(&agreeing, &mapping),
            ResolvedTaskKind::Resolved {
                kind: TaskKind::Bug,
                ..
            }
        ),
        "the near-identical agreeing case resolves"
    );

    assert_eq!(
        TaskIdentityState::parse("resolved"),
        Some(TaskIdentityState::Resolved)
    );
    assert_eq!(
        TaskIdentityState::parse("almost-resolved"),
        None,
        "an unknown state label is refused, never defaulted"
    );
}

// ===========================================================================
// 4. Empty eligible aggregates: explicit empty or unavailable, never a zero.
// ===========================================================================

/// Criterion 7 (property): a coverage ratio over an empty eligible set (a zero
/// denominator) is `None` for every numerator, never `Some(0.0)`. A positive
/// denominator always yields a value.
#[test]
fn prop_coverage_fraction_over_an_empty_denominator_is_none_never_zero() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..512 {
        let numerator = next() % 10_000;
        assert_eq!(
            CoverageFraction::new(numerator, 0),
            None,
            "numerator {numerator} over an empty set is not a number"
        );
        let denominator = 1 + next() % 10_000;
        assert!(
            CoverageFraction::new(numerator, denominator).is_some(),
            "a real denominator yields a fraction ({numerator}/{denominator})"
        );
    }
}

/// Criterion 7 (property): a `Derivation` cannot be made `Unavailable` with an
/// empty missing set, so a refusal always names at least one fact, and when
/// serialized it renders as an explicit `unavailable`, never as `value: 0`.
#[test]
fn prop_an_unavailable_derivation_always_names_a_fact_and_never_serializes_zero() {
    assert!(
        Derivation::<Credits>::unavailable([], Provenance::new(["p".to_string()])).is_err(),
        "an empty missing set is not a refusal"
    );

    for facts in [
        vec!["eligible events"],
        vec!["active cost model"],
        vec!["cache_write rate", "output rate"],
    ] {
        let derivation: Derivation<Credits> = Derivation::unavailable(
            facts.iter().map(|f| RequiredFact::new(*f)),
            Provenance::new(["cost-model:unavailable".to_string()]),
        )
        .expect("a named missing fact");
        let report = spend_report_with_credits(Some(derivation), None);
        let json = spend_json(&report, RunId::from_string("run-empty".to_string()));

        assert!(
            json.contains("\"credits\":{\"status\":\"unavailable\""),
            "an empty aggregate serializes as unavailable: {json}"
        );
        for fact in &facts {
            assert!(
                json.contains(fact),
                "the missing fact {fact} is named: {json}"
            );
        }
        assert!(
            !json.contains("\"credits\":{\"value\":\"0\""),
            "never a numeric zero total for an unavailable aggregate: {json}"
        );
        validate_spend_report_json(&json).expect("the refusing report validates");
    }
}

// ===========================================================================
// 5. Contract: the human and JSON surfaces agree on freshness, qualification
//    and unavailable states, and both keep a valid zero a real zero.
// ===========================================================================

/// Criterion 8: every stale reason renders the same state and the same reason
/// wording in both surfaces, and neither surface fabricates a value.
#[test]
fn human_and_json_agree_on_every_stale_reason() {
    let ctx = CredentialContextId::new("ctx-contract");
    let cases = [
        (
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            "timeout",
            "source_unreachable",
        ),
        (AttemptOutcome::AuthRequired, "auth!", "auth_required"),
    ];
    for (outcome, human_marker, json_marker) in cases {
        let observed_nanos = 1_000_000_000_000;
        let reading = freshness_after(
            Some(meter_observation(250_000, observed_nanos)),
            outcome,
            observed_nanos,
            observed_nanos + 5_000,
            &ctx,
        );
        let human = render_meter_reading(
            &reading,
            "%",
            PERCENT,
            UtcTimestamp::from_unix_nanos(observed_nanos + 5_000),
            envelope(),
            None,
        );
        let json = freshness_json("acct", &reading);
        assert!(
            human.contains(human_marker),
            "human surface names the state ({human_marker}): {human}"
        );
        assert!(
            json.contains(json_marker),
            "json surface names the state ({json_marker}): {json}"
        );
        assert!(
            !json.contains("\"freshness\":\"fresh\""),
            "neither surface labels a failed reading fresh: {json}"
        );
    }
}

/// Criterion 8: a credits refusal names the missing fact and never renders a
/// zero, in both the human line and the JSON document.
#[test]
fn human_and_json_agree_that_credits_are_unavailable_not_zero() {
    let model = anthropic_claude_messages_incomplete_v1(UtcTimestamp::from_unix_nanos(1_000));
    let derivation = convert(&model, &usage(100_000, 20_000, 50_000, 10_000));
    assert!(matches!(derivation, Derivation::Unavailable { .. }));
    let report = spend_report_with_credits(
        Some(derivation),
        Some("anthropic-claude-messages-incomplete-v1"),
    );

    let text = render_spend_report(&report);
    let json = spend_json(&report, RunId::from_string("run-unavail".to_string()));

    assert!(text.contains("credits unavailable:"), "{text}");
    assert!(
        !text.contains("0.00 credits"),
        "a refusal never renders a zero credit line: {text}"
    );
    assert!(
        json.contains("\"credits\":{\"status\":\"unavailable\""),
        "{json}"
    );
    assert!(!json.contains("\"credits\":{\"value\":\"0\""), "{json}");
    validate_spend_report_json(&json).expect("the report validates");
}

/// Criterion 6, 7, 8: a valid zero credit total (from an all-zero usage vector)
/// reaches both surfaces as a real zero with its qualification, never as
/// unavailable.
#[test]
fn human_and_json_agree_that_a_valid_zero_is_a_real_zero() {
    let model = anthropic_claude_messages_v1(UtcTimestamp::from_unix_nanos(1_000));
    let report = spend_report_with_credits(
        Some(convert(&model, &usage(0, 0, 0, 0))),
        Some("anthropic-claude-messages-v1"),
    );

    let text = render_spend_report(&report);
    let json = spend_json(&report, RunId::from_string("run-zero".to_string()));

    assert!(
        text.contains("0.00 credits (complete)"),
        "the human surface shows the measured zero with its qualification: {text}"
    );
    assert!(
        json.contains("\"credits\":{\"value\":\"0.00\",\"unit\":\"credits\""),
        "the json surface shows the measured zero: {json}"
    );
    assert!(
        !json.contains("\"status\":\"unavailable\""),
        "a valid zero is never unavailable: {json}"
    );
    validate_spend_report_json(&json).expect("the report validates");
}
