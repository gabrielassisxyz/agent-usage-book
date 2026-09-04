//! Integration coverage for the authoritative-surface comparison bookkeeping
//! (`aub-eun.12`, PLAN.md sections 34.8, 34.30, 45).
//!
//! The unit behaviour of the verdict and the store lives beside the code. This
//! suite proves the one property that needs the whole substrate: a comparison,
//! an unresolved mismatch and a linked correction are irreplaceable validation
//! evidence that survives a verified backup and restore exactly, links intact.
//!
//! May not depend on:
//! - presentation

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::backup::create_archive;
use agent_usage_book::cli::{
    AdapterSemanticsComparisonRequest, record_adapter_semantics_comparison,
};
use agent_usage_book::domain::authoritative_comparison::{
    AuthoritativeComparisonVerdict, DocumentedGranularity, compare_against_authoritative_surface,
};
use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::store::account::observe_account;
use agent_usage_book::store::adapter_semantics_validation::{
    AnnotationKind, NewAdapterSemanticsAnnotation, NewAuthoritativeSurfaceComparison,
    annotation_by_row_id, annotation_row_count, comparison_by_row_id, comparison_row_count,
    insert_annotation, insert_comparison, open_semantic_mismatch_findings,
};
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, ObservationRowId, WindowRowId,
    insert_observation, insert_response_evidence, insert_window,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::retention::{DurableClass, DurableClassCategory};
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};
use agent_usage_book::store::startup::FakeMountTable;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-adapter-semantics-integration-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn timeout() -> MonotonicDuration {
    MonotonicDuration::from_millis(1000)
}

const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
    ordinary_cadence: MonotonicDuration::from_millis(300_000),
    freshness_horizon: MonotonicDuration::from_millis(900_000),
    reset_edge_policy: String::new(),
    retry_backoff_policy: String::new(),
    command_budget: MonotonicDuration::from_millis(60_000),
    policy_algorithm_version: String::new(),
};

fn used(ppm: i32) -> QuotaUsed {
    QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap())
}

/// Migrates a fresh ledger in `state_dir` and seeds one observation with two
/// account-wide windows.
fn seed_observation(state_dir: &Path) -> (ObservationRowId, Vec<WindowRowId>) {
    let mut conn = open(
        &state_dir.join(LEDGER_DATABASE_FILE),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: timeout(),
        },
    )
    .expect("ledger must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrations must apply");

    let account = observe_account(
        &conn,
        "anthropic",
        "primary",
        UtcTimestamp::from_unix_nanos(10),
    )
    .expect("account must insert");
    let run = start_sample_run(
        &conn,
        Trigger::Manual,
        UtcTimestamp::from_unix_nanos(10),
        "seed",
    )
    .expect("sample run must insert");
    let snapshot =
        resolve_policy_snapshot(&conn, account, UtcTimestamp::from_unix_nanos(10), &POLICY)
            .expect("policy snapshot must insert");
    let attempt = start_meter_attempt(
        &conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "anthropic".into(),
            request_started_at: UtcTimestamp::from_unix_nanos(20),
            credential_context_id: Some("ctx".into()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(19),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-schema-v3".into(),
            meter_semantics_id: "account-5h-v2".into(),
        },
    )
    .expect("attempt must insert");
    let evidence_id = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: UtcTimestamp::from_unix_nanos(30),
            provider_observed_at_original: None,
            evidence_capsule: r#"{"windows":[]}"#.into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        },
    )
    .expect("evidence must insert");
    let observation_id = insert_observation(
        &conn,
        &NewMeterObservation {
            attempt_id: attempt,
            evidence_id,
            account_id: account,
            provider: "anthropic".into(),
            provider_observed_at: None,
            received_at: UtcTimestamp::from_unix_nanos(31),
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
            meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
            normalized_fingerprint: "fp-1".into(),
        },
    )
    .expect("observation must insert");
    let mut windows = Vec::new();
    for (key, ppm) in [("five_hour", 700_000), ("seven_day", 910_000)] {
        let window_id = insert_window(
            &conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new(key),
                scope: WindowScope::AccountWide,
                quota_used: used(ppm),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: UtcTimestamp::from_unix_nanos(100_000).into(),
                nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            },
        )
        .expect("window must insert");
        windows.push(window_id);
    }
    (observation_id, windows)
}

/// A comparison, an unresolved mismatch and a linked correction survive a
/// verified backup and restore exactly, with the correction still pointing at
/// the mismatch it corrects.
#[test]
fn comparison_and_correction_records_survive_a_verified_backup_and_restore() {
    let scratch = ScratchDir::new();
    let state_dir = scratch.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let (observation_id, windows) = seed_observation(&state_dir);

    let granularity = DocumentedGranularity::new(QuotaFractionPpm::new(10_000).unwrap());
    let (comparison_id, mismatch_id, correction_id) = {
        let conn = open(
            &state_dir.join(LEDGER_DATABASE_FILE),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: timeout(),
            },
        )
        .unwrap();
        let verdict =
            compare_against_authoritative_surface(used(700_000), used(410_000), granularity);
        assert_eq!(verdict, AuthoritativeComparisonVerdict::UnresolvedMismatch);
        let comparison_id = insert_comparison(
            &conn,
            &NewAuthoritativeSurfaceComparison {
                observation_id,
                window_id: windows[0],
                semantic_key: WindowSemanticKey::new("five_hour"),
                authoritative_surface: "console usage page".into(),
                documented_granularity: granularity,
                adapter_quota_used: used(700_000),
                authoritative_quota_used: used(410_000),
                read_at: UtcTimestamp::from_unix_nanos(500_000),
                verdict,
            },
        )
        .unwrap();
        let mismatch_id = insert_annotation(
            &conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Mismatch,
                comparison_id,
                observation_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                adapter_quota_used: used(700_000),
                authoritative_quota_used: used(410_000),
                corrects: None,
                detail: "five_hour read 70 percent, surface showed 41 percent".into(),
                created_at: UtcTimestamp::from_unix_nanos(510_000),
            },
        )
        .unwrap();
        let correction_id = insert_annotation(
            &conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Correction,
                comparison_id,
                observation_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                adapter_quota_used: used(700_000),
                authoritative_quota_used: used(410_000),
                corrects: Some(mismatch_id),
                detail: "adapter read the wrong field; corrected".into(),
                created_at: UtcTimestamp::from_unix_nanos(520_000),
            },
        )
        .unwrap();
        (comparison_id, mismatch_id, correction_id)
    };

    // Back the ledger up and restore it into a directory that does not exist.
    let archive = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000));
    let summary = create_archive(&state_dir, &archive, timeout(), &clock).unwrap();
    assert!(summary.verified, "the archive must verify");

    let restored = scratch.path().join("restored");
    let restore_summary = agent_usage_book::restore::restore_archive(
        &scratch.path().join("configured-unused"),
        &archive,
        &restored,
        None,
        timeout(),
        &FakeMountTable::new(),
        &clock,
    )
    .unwrap();
    assert!(restore_summary.archive_verified);

    // Read the restored ledger and assert every record and link survived.
    let restored_db = open(
        &restored.join(LEDGER_DATABASE_FILE),
        AccessMode::ReadOnly,
        &PragmaPolicy {
            busy_timeout: timeout(),
        },
    )
    .unwrap();

    assert_eq!(comparison_row_count(&restored_db).unwrap().value(), 1);
    assert_eq!(annotation_row_count(&restored_db).unwrap().value(), 2);

    let comparison = comparison_by_row_id(&restored_db, comparison_id)
        .unwrap()
        .expect("the comparison must survive");
    assert_eq!(
        comparison.verdict,
        AuthoritativeComparisonVerdict::UnresolvedMismatch
    );
    assert_eq!(comparison.adapter_quota_used, used(700_000));
    assert_eq!(comparison.authoritative_quota_used, used(410_000));
    assert_eq!(comparison.semantic_key.as_str(), "five_hour");
    assert_eq!(comparison.documented_granularity, granularity);

    let mismatch = annotation_by_row_id(&restored_db, mismatch_id)
        .unwrap()
        .expect("the mismatch must survive");
    assert_eq!(mismatch.kind, AnnotationKind::Mismatch);
    assert_eq!(mismatch.corrects, None);

    let correction = annotation_by_row_id(&restored_db, correction_id)
        .unwrap()
        .expect("the correction must survive");
    assert_eq!(correction.kind, AnnotationKind::Correction);
    assert_eq!(
        correction.corrects,
        Some(mismatch_id),
        "the correction still points at the mismatch it corrects"
    );

    // The mismatch is explained, so it is not an open finding, but it is still
    // stored: that is the recovery-path contract.
    assert!(
        open_semantic_mismatch_findings(&restored_db)
            .unwrap()
            .is_empty()
    );

    // Both classes are irreplaceable and retained forever.
    for class in [
        DurableClass::AuthoritativeSurfaceComparison,
        DurableClass::AdapterSemanticsAnnotation,
    ] {
        assert_eq!(class.category(), DurableClassCategory::Irreplaceable);
        assert!(class.is_forever());
        assert!(!class.is_prunable());
    }
}

/// Recording a comparison through `aub compare record`'s own mechanism
/// (`agent_usage_book::cli::record_adapter_semantics_comparison`, `aub-x2bq`)
/// with a surface value far outside the documented granularity opens a
/// mismatch annotation reachable through
/// `open_semantic_mismatch_findings`, the same path a hand-recorded mismatch
/// reaches. The planted negative: an agreeing comparison, recorded the same
/// way against the second window, must open no finding at all.
#[test]
fn recording_a_mismatch_through_the_command_mechanism_opens_a_reachable_finding() {
    let scratch = ScratchDir::new();
    let state_dir = scratch.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let (observation_id, windows) = seed_observation(&state_dir);
    let conn = open(
        &state_dir.join(LEDGER_DATABASE_FILE),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: timeout(),
        },
    )
    .expect("ledger must open");

    let granularity = DocumentedGranularity::new(QuotaFractionPpm::new(10_000).unwrap());

    // five_hour's adapter reading is 700_000 ppm; a surface reading of
    // 410_000 ppm disagrees by far more than the granularity, so this must
    // be an unresolved mismatch.
    let mismatch_outcome = record_adapter_semantics_comparison(
        &conn,
        &AdapterSemanticsComparisonRequest {
            observation_id,
            semantic_key: WindowSemanticKey::new("five_hour"),
            authoritative_surface: "console usage page".into(),
            surface_quota_used: used(410_000),
            documented_granularity: granularity,
            read_at: UtcTimestamp::from_unix_nanos(200_000),
            detail: None,
        },
    )
    .expect("the mismatched comparison must record");
    assert_eq!(
        mismatch_outcome.verdict,
        AuthoritativeComparisonVerdict::UnresolvedMismatch
    );
    let mismatch_annotation_id = mismatch_outcome
        .mismatch_annotation_id
        .expect("a mismatch verdict must open an annotation");

    // seven_day's adapter reading is 910_000 ppm; a surface reading that
    // agrees must open no finding.
    let agreement_outcome = record_adapter_semantics_comparison(
        &conn,
        &AdapterSemanticsComparisonRequest {
            observation_id,
            semantic_key: WindowSemanticKey::new("seven_day"),
            authoritative_surface: "console usage page".into(),
            surface_quota_used: used(910_000),
            documented_granularity: granularity,
            read_at: UtcTimestamp::from_unix_nanos(200_000),
            detail: None,
        },
    )
    .expect("the agreeing comparison must record");
    assert_eq!(
        agreement_outcome.verdict,
        AuthoritativeComparisonVerdict::AgreesWithinGranularity
    );
    assert_eq!(agreement_outcome.mismatch_annotation_id, None);

    let findings = open_semantic_mismatch_findings(&conn).expect("findings must read");
    assert_eq!(
        findings.len(),
        1,
        "only the five_hour mismatch is an open finding"
    );
    assert_eq!(findings[0].annotation_id, mismatch_annotation_id);
    assert_eq!(findings[0].observation_id, observation_id);
    assert_eq!(findings[0].semantic_key.as_str(), "five_hour");
    assert_eq!(findings[0].adapter_quota_used, used(700_000));
    assert_eq!(findings[0].authoritative_quota_used, used(410_000));
    let _ = windows;
}

/// A second comparison for a window that already carries one is refused,
/// naming the existing comparison rather than writing a duplicate row.
#[test]
fn record_refuses_a_second_comparison_for_an_already_compared_window() {
    let scratch = ScratchDir::new();
    let state_dir = scratch.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let (observation_id, _windows) = seed_observation(&state_dir);
    let conn = open(
        &state_dir.join(LEDGER_DATABASE_FILE),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: timeout(),
        },
    )
    .expect("ledger must open");

    let request = AdapterSemanticsComparisonRequest {
        observation_id,
        semantic_key: WindowSemanticKey::new("five_hour"),
        authoritative_surface: "console usage page".into(),
        surface_quota_used: used(700_000),
        documented_granularity: DocumentedGranularity::new(QuotaFractionPpm::new(10_000).unwrap()),
        read_at: UtcTimestamp::from_unix_nanos(200_000),
        detail: None,
    };
    let first = record_adapter_semantics_comparison(&conn, &request)
        .expect("the first comparison must record");

    let second_attempt = record_adapter_semantics_comparison(&conn, &request);
    let error = second_attempt.expect_err("a second comparison for the same window is refused");
    let message = error.to_string();
    assert!(
        message.contains(&format!("comparison #{}", first.comparison_id.value())),
        "the refusal must name the existing comparison: {message}"
    );
    assert!(
        message.contains(&observation_id.value().to_string()),
        "the refusal must name the observation: {message}"
    );

    // Exactly one comparison row exists for this window: the refusal never
    // wrote a duplicate.
    assert_eq!(comparison_row_count(&conn).unwrap().value(), 1);
}
