//! Integration test for aub-mgv.5: a persisted marker timeline joined with a
//! persisted heartbeat, alongside a fresh meter observation, and read back
//! through the status renderers.
//!
//! Unlike `tests/e2e/cases/022-now-explicit-marker-liveness.sh` (the release
//! binary, a real process boundary), this exercises the store, the domain
//! composition and the two presentation entry points in one process: real
//! SQLite, real repository functions, real render and JSON functions, no
//! spawned binary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::freshness::{Freshness, Observed};
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{MeasurementBasis, ReceivedAt, UtcTimestamp};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{status_json, validate_status_report_json};
use agent_usage_book::presentation::render::render_status_report;
use agent_usage_book::report::{
    ActiveActivityState, ActiveSpendClaim, InactiveSpendClaim, LedgerGeneration, LivenessGap,
    MeterAccount, ProjectionReadState, ReportMetadata, StatusReport, compose_active_activity,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker, markers_for_session,
};
use agent_usage_book::store::session_heartbeat::{latest_heartbeat, record_heartbeat};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-now-activity-integration-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("scratch dir must be creatable");
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

#[test]
fn a_persisted_marker_and_heartbeat_join_a_fresh_observation_and_survive_both_renderers() {
    let scratch = ScratchDir::new();
    let conn = open(
        &scratch.path().join("ledger.db"),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: agent_usage_book::domain::time::MonotonicDuration::from_millis(1000),
        },
    )
    .expect("open");
    let mut conn = conn;
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &agent_usage_book::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrate");

    let session = SessionId::new(
        SourceNamespace::new("claude-code"),
        NativeSessionId::new("sess-integration-1"),
    );

    // Persist the explicit marker and the heartbeat exactly as production
    // code would: through the store's own insert functions, never a
    // hand-rolled row.
    insert_marker(
        &conn,
        &NewSessionAccountMarker {
            session_id: session.clone(),
            observed_at: UtcTimestamp::from_unix_nanos(1_000_000_000),
            source_ordering_key: None,
            logical_account: "work-a".to_string(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        },
    )
    .expect("insert marker");
    record_heartbeat(
        &conn,
        &session,
        UtcTimestamp::from_unix_nanos(1_800_000_000),
        "turn_end",
    )
    .expect("record heartbeat");

    let report_instant = UtcTimestamp::from_unix_nanos(2_000_000_000);
    let markers = markers_for_session(&conn, &session).expect("read markers");
    let heartbeat = latest_heartbeat(&conn, &session).expect("read heartbeat");
    let activity = compose_active_activity(
        &markers,
        heartbeat.as_ref(),
        report_instant,
        agent_usage_book::report::activity::DEFAULT_LIVENESS_HORIZON,
    );

    let claim = match &activity {
        ActiveActivityState::ExplicitMarkerEvidence(claim) => claim,
        other => {
            panic!("expected ExplicitMarkerEvidence from a real store round trip, got {other:?}")
        }
    };
    assert_eq!(claim.logical_account, "work-a");
    assert_eq!(claim.marker_reference, "session_account_marker:1");
    assert_eq!(claim.heartbeat_reference, "session_heartbeat:1");

    // A fresh meter observation joins the same report: activity evidence and
    // meter evidence are two independent facts on one status report, and this
    // proves neither renderer conflates or drops either.
    let account = MeterAccount::new(
        LogicalName::new("work-a"),
        Freshness::Fresh {
            observed: Observed::new(
                QuotaRemaining::new(QuotaFractionPpm::new(250_000).unwrap()),
                None,
                ReceivedAt::new(report_instant),
                MeasurementBasis::ProviderObserved,
            ),
            latest_attempt: AttemptId::new(1),
        },
    );
    let metadata = ReportMetadata::new(
        report_instant,
        report_instant,
        LedgerGeneration::new(1),
        None,
    );
    let report = StatusReport::new(
        metadata,
        vec![account],
        Vec::new(),
        ProjectionReadState::Read,
    )
    .with_activity(activity);

    // Human text: the meter line and the activity line both survive, and the
    // activity line carries the evidence class label and both provenance
    // identifiers.
    let envelope = agent_usage_book::domain::time::ClockSkewEnvelope::new(
        agent_usage_book::domain::time::MonotonicDuration::from_seconds(60),
    );
    let text = render_status_report(
        &report,
        report_instant,
        envelope,
        agent_usage_book::presentation::Style::plain(),
    );
    assert!(text.contains("  work-a  "), "meter line missing: {text}");
    assert!(
        text.contains(
            "aub session: spending account=work-a marker=session_account_marker:1 heartbeat=session_heartbeat:1"
        ),
        "activity line missing or wrong shape: {text}"
    );

    // JSON: the same two provenance identifiers and the evidence class, next
    // to the account's own freshness.
    let run = RunId::new(report_instant);
    let json = status_json(&report, run);
    validate_status_report_json(&json).expect("status JSON with activity conforms to schema v5");
    assert!(json.contains("\"account\":\"work-a\""));
    assert!(json.contains("\"freshness\":\"fresh\""));
    assert!(
        json.contains(
            "\"activity\":{\"state\":\"explicit_marker_evidence\",\"account\":\"work-a\",\"marker\":\"session_account_marker:1\",\"heartbeat\":\"session_heartbeat:1\"}"
        ),
        "activity JSON missing or wrong shape: {json}"
    );
}

#[test]
fn status_rendering_and_json_keep_all_four_activity_states() {
    let report_instant = UtcTimestamp::from_unix_nanos(1_000);
    let metadata = ReportMetadata::new(
        report_instant,
        report_instant,
        LedgerGeneration::new(1),
        None,
    );
    let report = StatusReport::new(metadata, Vec::new(), Vec::new(), ProjectionReadState::Read);
    let envelope = agent_usage_book::domain::time::ClockSkewEnvelope::new(
        agent_usage_book::domain::time::MonotonicDuration::from_seconds(60),
    );
    let cases = [
        (
            ActiveActivityState::ExplicitMarkerEvidence(ActiveSpendClaim {
                logical_account: "work-a".to_string(),
                marker_reference: "marker-1".to_string(),
                heartbeat_reference: "heartbeat-1".to_string(),
            }),
            Some("aub session: spending account=work-a marker=marker-1 heartbeat=heartbeat-1"),
        ),
        (ActiveActivityState::NoEvidence, None),
        (
            ActiveActivityState::ConflictingEvidence(vec![
                "account-a".to_string(),
                "account-b".to_string(),
            ]),
            Some("aub session: conflicting_evidence accounts=[account-a, account-b]"),
        ),
        (
            ActiveActivityState::Inactive(InactiveSpendClaim {
                logical_account: "work-a".to_string(),
                marker_reference: "marker-1".to_string(),
                liveness_gap: LivenessGap::Aged {
                    last_heartbeat_at: report_instant,
                    heartbeat_reference: "heartbeat-1".to_string(),
                },
            }),
            Some(
                "aub session: inactive account=work-a marker=marker-1 liveness=aged last_heartbeat=1000",
            ),
        ),
    ];

    for (activity, expected_text) in cases {
        let with_activity = report.clone().with_activity(activity);
        let text = render_status_report(
            &with_activity,
            report_instant,
            envelope,
            agent_usage_book::presentation::Style::plain(),
        );
        match expected_text {
            Some(expected) => assert!(text.contains(expected), "{text}"),
            None => assert!(!text.contains("aub session:"), "{text}"),
        }
        let json = status_json(&with_activity, RunId::new(report_instant));
        validate_status_report_json(&json).expect("every activity state conforms to schema v5");
    }
}
