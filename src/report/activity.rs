//! Explicit-marker-backed live account activity (`aub-mgv.5`, PLAN.md 19.2, 43
//! Workflow 2).
//!
//! Meter movement, ambient credentials and the currently selected profile never
//! substitute for either of the two typed facts this module composes: an explicit
//! session/account marker (`session_account_marker`) and independent contemporary
//! liveness evidence (`session_heartbeat`, decided on `aub-mgv.6`: option B,
//! turn-end and throttled post-tool heartbeats). The live report may claim a
//! session is actively spending under an account only when both cover the exact
//! report instant and agree.
//!
//! This is a composed claim about the report, not an account-attribution evidence
//! class: [`AccountEvidenceClass`] ranks five kinds of evidence for splitting
//! historical usage, and only its highest rank (`ExplicitLauncherOrHook`) ever
//! qualifies here. A marker resting on provider identity, credential mapping or
//! temporal inference is no different from no marker at all for this question,
//! because Workflow 2 asks whether an explicit marker justifies "currently
//! spending," never whether some account can be guessed.

use std::collections::BTreeSet;

use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::store::session_account_marker::{EvidenceDesignation, SessionAccountMarker};
use crate::store::session_heartbeat::SessionHeartbeat;

/// The default freshness horizon a heartbeat counts as contemporary within
/// (`aub-mgv.6`: 15 minutes). Older than this, whatever the reason, is not live.
pub const DEFAULT_LIVENESS_HORIZON: MonotonicDuration = MonotonicDuration::from_seconds(15 * 60);

/// The typed active-activity state the live report carries.
///
/// Exhaustive over the four states PLAN.md 19.2 and 43 Workflow 2 name: an
/// explicit marker with contemporary liveness, no evidence at all, an
/// irreconcilable tie in the marker data itself, or a marker whose session the
/// liveness policy no longer finds live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveActivityState {
    /// An explicit marker and a fresh heartbeat both cover the report instant and
    /// agree: the session is actively spending under this account.
    ExplicitMarkerEvidence(ActiveSpendClaim),
    /// No explicit marker covers the report instant. Includes no session named,
    /// no marker at all, and a marker resting on weaker evidence than an explicit
    /// launcher or hook mark.
    NoEvidence,
    /// More than one explicit marker names a different account for the exact same
    /// instant, with no source ordering key to break the tie. Never resolved by
    /// picking one silently.
    ConflictingEvidence(Vec<String>),
    /// An explicit marker covers the instant, but the selected liveness policy
    /// finds the session no longer live.
    Inactive(InactiveSpendClaim),
}

/// Payload-free mirror of [`ActiveActivityState`], for callers that need only the
/// state name (JSON's `"state"` field, a rendered label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveActivityStateKind {
    ExplicitMarkerEvidence,
    NoEvidence,
    ConflictingEvidence,
    Inactive,
}

impl ActiveActivityState {
    pub fn kind(&self) -> ActiveActivityStateKind {
        match self {
            Self::ExplicitMarkerEvidence(_) => ActiveActivityStateKind::ExplicitMarkerEvidence,
            Self::NoEvidence => ActiveActivityStateKind::NoEvidence,
            Self::ConflictingEvidence(_) => ActiveActivityStateKind::ConflictingEvidence,
            Self::Inactive(_) => ActiveActivityStateKind::Inactive,
        }
    }
}

impl ActiveActivityStateKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitMarkerEvidence => "explicit_marker_evidence",
            Self::NoEvidence => "no_evidence",
            Self::ConflictingEvidence => "conflicting_evidence",
            Self::Inactive => "inactive",
        }
    }
}

/// The session and account a live report may claim is actively spending, with
/// the two provenance identifiers (`session_account_marker:<id>`,
/// `session_heartbeat:<id>`) that jointly justify the claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSpendClaim {
    pub logical_account: String,
    pub marker_reference: String,
    pub heartbeat_reference: String,
}

/// An explicit marker's account claim, held back from the report because the
/// liveness policy does not independently prove the session is currently live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactiveSpendClaim {
    pub logical_account: String,
    pub marker_reference: String,
    pub liveness_gap: LivenessGap,
}

/// Why the liveness policy would not prove a marked session live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessGap {
    /// No heartbeat was ever recorded for this session (the hook was never wired,
    /// or the session predates it).
    NeverObserved,
    /// A heartbeat exists but has aged past the freshness horizon.
    Aged {
        last_heartbeat_at: UtcTimestamp,
        heartbeat_reference: String,
    },
}

/// Composes the typed activity state for one session at one report instant.
///
/// `session_markers` is every marker recorded for the session under evaluation
/// (any evidence class; the explicit-only filter happens inside), and
/// `heartbeat` is that session's most recent liveness evidence, when any exists.
pub fn compose_active_activity(
    session_markers: &[SessionAccountMarker],
    heartbeat: Option<&SessionHeartbeat>,
    report_instant: UtcTimestamp,
    horizon: MonotonicDuration,
) -> ActiveActivityState {
    match explicit_claim_at(session_markers, report_instant) {
        ExplicitClaim::None => ActiveActivityState::NoEvidence,
        ExplicitClaim::Conflicting(logical_accounts) => {
            ActiveActivityState::ConflictingEvidence(logical_accounts)
        }
        ExplicitClaim::One {
            logical_account,
            marker_reference,
        } => match heartbeat {
            None => ActiveActivityState::Inactive(InactiveSpendClaim {
                logical_account,
                marker_reference,
                liveness_gap: LivenessGap::NeverObserved,
            }),
            Some(observation) => {
                if within_horizon(observation.last_heartbeat_at(), report_instant, horizon) {
                    ActiveActivityState::ExplicitMarkerEvidence(ActiveSpendClaim {
                        logical_account,
                        marker_reference,
                        heartbeat_reference: format!(
                            "session_heartbeat:{}",
                            observation.id().value()
                        ),
                    })
                } else {
                    ActiveActivityState::Inactive(InactiveSpendClaim {
                        logical_account,
                        marker_reference,
                        liveness_gap: LivenessGap::Aged {
                            last_heartbeat_at: observation.last_heartbeat_at(),
                            heartbeat_reference: format!(
                                "session_heartbeat:{}",
                                observation.id().value()
                            ),
                        },
                    })
                }
            }
        },
    }
}

/// What the explicit-marker timeline alone says about one instant, before
/// liveness is consulted.
enum ExplicitClaim {
    /// No rank-1 marker covers the instant: none exist, the instant precedes
    /// every one, or the only markers present rest on weaker evidence.
    None,
    /// Exactly one explicit marker interval covers the instant.
    One {
        logical_account: String,
        marker_reference: String,
    },
    /// More than one explicit marker shares the exact instant that covers the
    /// report instant and they name different accounts.
    Conflicting(Vec<String>),
}

/// Locates the explicit-marker (`ExplicitLauncherOrHook`) account claim covering
/// one instant, within one session's marker history.
///
/// Unlike `attribution::account_segment::assign`, which places usage events and
/// resolves a same-instant tie deterministically (arbitrarily, by input order)
/// because a historical usage split must land somewhere, this is a live claim
/// about "who is spending right now": a genuine tie is reported as
/// [`ExplicitClaim::Conflicting`] rather than resolved silently, because picking
/// one would be exactly the unjustified claim this bead exists to prevent.
fn explicit_claim_at(markers: &[SessionAccountMarker], instant: UtcTimestamp) -> ExplicitClaim {
    let mut explicit: Vec<&SessionAccountMarker> = markers
        .iter()
        .filter(|marker| {
            marker.evidence_designation() == EvidenceDesignation::ExplicitLauncherOrHook
        })
        .collect();
    if explicit.is_empty() {
        return ExplicitClaim::None;
    }
    explicit.sort_by(|a, b| {
        a.observed_at()
            .cmp(&b.observed_at())
            .then_with(|| a.source_ordering_key().cmp(&b.source_ordering_key()))
    });

    let mut covering: Option<usize> = None;
    for (index, marker) in explicit.iter().enumerate() {
        if marker.observed_at() <= instant {
            covering = Some(index);
        } else {
            break;
        }
    }
    let Some(index) = covering else {
        return ExplicitClaim::None;
    };
    let winner = explicit[index];

    let tied: Vec<&&SessionAccountMarker> = explicit
        .iter()
        .filter(|marker| {
            marker.observed_at() == winner.observed_at()
                && marker.source_ordering_key() == winner.source_ordering_key()
        })
        .collect();
    let distinct_accounts: BTreeSet<&str> =
        tied.iter().map(|marker| marker.logical_account()).collect();
    if distinct_accounts.len() > 1 {
        return ExplicitClaim::Conflicting(
            distinct_accounts.into_iter().map(str::to_owned).collect(),
        );
    }

    ExplicitClaim::One {
        logical_account: winner.logical_account().to_string(),
        marker_reference: format!("session_account_marker:{}", winner.id().value()),
    }
}

/// True when `observed_at` is within `horizon` of `at`, treating an observation
/// at or after `at` as trivially fresh (a clock's own jitter must never read as
/// staleness).
fn within_horizon(observed_at: UtcTimestamp, at: UtcTimestamp, horizon: MonotonicDuration) -> bool {
    if observed_at.unix_nanos() >= at.unix_nanos() {
        return true;
    }
    let age_nanos = (at.unix_nanos() - observed_at.unix_nanos()) as u64;
    age_nanos <= horizon.as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
    use crate::store::session_account_marker::{MarkerSource, SourceOrderingKey};
    use proptest::prelude::*;

    fn t(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    fn session() -> SessionId {
        SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-1"),
        )
    }

    // The store type's fields are private outside the store module; tests build
    // rows through the same round trip production code uses, an in-memory
    // connection, so this module's tests exercise the exact getters `compose_active_activity`
    // reads rather than a hand-rolled stand-in.
    fn fixture_conn() -> rusqlite::Connection {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &crate::domain::time::FakeClock::new(t(0)),
        )
        .unwrap();
        conn
    }

    fn insert_marker(
        conn: &rusqlite::Connection,
        session_id: &SessionId,
        observed_at: UtcTimestamp,
        ordering_key: Option<i64>,
        logical_account: &str,
        evidence: EvidenceDesignation,
    ) {
        crate::store::session_account_marker::insert_marker(
            conn,
            &crate::store::session_account_marker::NewSessionAccountMarker {
                session_id: session_id.clone(),
                observed_at,
                source_ordering_key: ordering_key.map(SourceOrderingKey::new),
                logical_account: logical_account.to_string(),
                resolved_account_id: None,
                marker_source: MarkerSource::new("hook"),
                run_id: None,
                evidence_designation: evidence,
            },
        )
        .unwrap();
    }

    fn markers_for(
        conn: &rusqlite::Connection,
        session_id: &SessionId,
    ) -> Vec<SessionAccountMarker> {
        crate::store::session_account_marker::markers_for_session(conn, session_id).unwrap()
    }

    fn record_heartbeat_row(
        conn: &rusqlite::Connection,
        session_id: &SessionId,
        observed_at: UtcTimestamp,
    ) -> SessionHeartbeat {
        crate::store::session_heartbeat::record_heartbeat(
            conn,
            session_id,
            observed_at,
            "turn_end",
        )
        .unwrap();
        crate::store::session_heartbeat::latest_heartbeat(conn, session_id)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn explicit_marker_with_fresh_heartbeat_is_spending() {
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "work",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let hb = record_heartbeat_row(&conn, &sess, t(100));

        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(200),
            DEFAULT_LIVENESS_HORIZON,
        );
        match state {
            ActiveActivityState::ExplicitMarkerEvidence(claim) => {
                assert_eq!(claim.logical_account, "work");
                assert_eq!(claim.marker_reference, "session_account_marker:1");
                assert_eq!(claim.heartbeat_reference, "session_heartbeat:1");
            }
            other => panic!("expected ExplicitMarkerEvidence, got {other:?}"),
        }
    }

    #[test]
    fn absent_marker_is_no_evidence() {
        let conn = fixture_conn();
        let sess = session();
        // A heartbeat with no marker at all still proves nothing about which
        // account, per the correctness invariant: liveness never substitutes
        // for association.
        let hb = record_heartbeat_row(&conn, &sess, t(100));
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(200),
            DEFAULT_LIVENESS_HORIZON,
        );
        assert_eq!(state, ActiveActivityState::NoEvidence);
    }

    #[test]
    fn delayed_meter_movement_without_a_marker_is_never_active_session_evidence() {
        // This module never sees meter readings at all: the caller passes only
        // markers and a heartbeat, so a moving meter with an empty marker slice
        // and no heartbeat composes to NoEvidence, not to some inferred account.
        let conn = fixture_conn();
        let sess = session();
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            None,
            t(500),
            DEFAULT_LIVENESS_HORIZON,
        );
        assert_eq!(state, ActiveActivityState::NoEvidence);
    }

    #[test]
    fn a_marker_without_any_heartbeat_is_inactive_never_observed() {
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "work",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            None,
            t(500),
            DEFAULT_LIVENESS_HORIZON,
        );
        match state {
            ActiveActivityState::Inactive(claim) => {
                assert_eq!(claim.logical_account, "work");
                assert_eq!(claim.liveness_gap, LivenessGap::NeverObserved);
            }
            other => panic!("expected Inactive/NeverObserved, got {other:?}"),
        }
    }

    #[test]
    fn a_marker_with_a_stale_heartbeat_is_inactive_aged() {
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "work",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let hb = record_heartbeat_row(&conn, &sess, t(0));
        let horizon = MonotonicDuration::from_seconds(60);
        // 120 seconds after a heartbeat 60 seconds wide: aged.
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            UtcTimestamp::from_unix_nanos(120_000_000_000),
            horizon,
        );
        match state {
            ActiveActivityState::Inactive(claim) => match claim.liveness_gap {
                LivenessGap::Aged {
                    last_heartbeat_at, ..
                } => assert_eq!(last_heartbeat_at, t(0)),
                other => panic!("expected Aged, got {other:?}"),
            },
            other => panic!("expected Inactive, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_markers_at_the_same_instant_report_conflicting_evidence() {
        // Planted negative: an implementation that silently applies its own
        // deterministic tie-break (as historical account segmentation
        // legitimately does) would report ExplicitMarkerEvidence for one of the
        // two accounts instead of surfacing the ambiguity.
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "account-a",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "account-b",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let hb = record_heartbeat_row(&conn, &sess, t(0));
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(0),
            DEFAULT_LIVENESS_HORIZON,
        );
        match state {
            ActiveActivityState::ConflictingEvidence(mut accounts) => {
                accounts.sort();
                assert_eq!(
                    accounts,
                    vec!["account-a".to_string(), "account-b".to_string()]
                );
            }
            other => panic!("expected ConflictingEvidence, got {other:?}"),
        }
    }

    #[test]
    fn an_ordering_key_resolves_what_would_otherwise_be_a_conflict() {
        // Same instant as the conflict test above, but this time the source gave
        // both markers an ordering key: no ambiguity, so the later-keyed one wins
        // cleanly instead of reporting a conflict.
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            Some(1),
            "account-a",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        insert_marker(
            &conn,
            &sess,
            t(0),
            Some(2),
            "account-b",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let hb = record_heartbeat_row(&conn, &sess, t(0));
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(0),
            DEFAULT_LIVENESS_HORIZON,
        );
        match state {
            ActiveActivityState::ExplicitMarkerEvidence(claim) => {
                assert_eq!(claim.logical_account, "account-b");
            }
            other => panic!("expected ExplicitMarkerEvidence(account-b), got {other:?}"),
        }
    }

    #[test]
    fn account_switch_moves_the_claim_at_the_documented_boundary() {
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "account-a",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        insert_marker(
            &conn,
            &sess,
            t(40),
            None,
            "account-b",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let hb = record_heartbeat_row(&conn, &sess, t(40));

        let just_before = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(39),
            DEFAULT_LIVENESS_HORIZON,
        );
        match just_before {
            ActiveActivityState::ExplicitMarkerEvidence(claim) => {
                assert_eq!(claim.logical_account, "account-a")
            }
            other => panic!("expected account-a just before the switch, got {other:?}"),
        }

        let at_boundary = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(40),
            DEFAULT_LIVENESS_HORIZON,
        );
        match at_boundary {
            ActiveActivityState::ExplicitMarkerEvidence(claim) => {
                assert_eq!(claim.logical_account, "account-b")
            }
            other => panic!("expected account-b at the switch boundary, got {other:?}"),
        }
    }

    #[test]
    fn a_lower_rank_marker_alone_is_no_evidence_not_a_guess() {
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "work",
            EvidenceDesignation::ConservativeTemporalInference,
        );
        let hb = record_heartbeat_row(&conn, &sess, t(0));
        let state = compose_active_activity(
            &markers_for(&conn, &sess),
            Some(&hb),
            t(0),
            DEFAULT_LIVENESS_HORIZON,
        );
        assert_eq!(state, ActiveActivityState::NoEvidence);
    }

    #[test]
    fn a_provider_observation_predating_an_account_switch_never_revives_the_prior_association() {
        // The switch happens at t(40); a heartbeat as of t(10) (before the
        // switch) with the report evaluated at t(50) must not read the prior
        // account back in: the current explicit claim (account-b, from t(40)
        // forward) is what liveness is checked against, not the marker that
        // happened to exist when the heartbeat was last seen.
        let conn = fixture_conn();
        let sess = session();
        insert_marker(
            &conn,
            &sess,
            t(0),
            None,
            "account-a",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        insert_marker(
            &conn,
            &sess,
            t(40),
            None,
            "account-b",
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        // Heartbeat recorded once, before the switch, and never refreshed since.
        let hb = record_heartbeat_row(&conn, &sess, t(10));
        let horizon = MonotonicDuration::from_nanos(5); // t(50) - t(10) = 40ns, past this horizon.

        let state = compose_active_activity(&markers_for(&conn, &sess), Some(&hb), t(50), horizon);
        match state {
            ActiveActivityState::Inactive(claim) => {
                assert_eq!(
                    claim.logical_account, "account-b",
                    "the current association is the post-switch account, even though liveness fails it"
                );
            }
            other => panic!("expected Inactive(account-b), got {other:?}"),
        }
    }

    proptest! {
        /// No combination of generated markers, heartbeat presence and report
        /// instant ever reaches [`ActiveActivityState::ExplicitMarkerEvidence`]
        /// unless an explicit (`ExplicitLauncherOrHook`) marker actually covers
        /// the report instant AND a heartbeat within the horizon backs it. This
        /// is the composition half of the report-level property ("no generated
        /// report in a non-explicit evidence state contains an active session or
        /// account claim"); `presentation::json` carries the other half, that
        /// the rendered text agrees.
        #[test]
        fn explicit_marker_evidence_requires_both_an_explicit_marker_and_a_fresh_heartbeat(
            marker_count in 0usize..4,
            marker_times in proptest::collection::vec(0i64..200, 0..4),
            marker_ranks in proptest::collection::vec(0u8..2, 0..4),
            heartbeat_present in proptest::bool::ANY,
            heartbeat_at in 0i64..200,
            report_instant in 0i64..200,
        ) {
            let conn = fixture_conn();
            let sess = session();
            let n = marker_count.min(marker_times.len()).min(marker_ranks.len());
            for i in 0..n {
                let evidence = if marker_ranks[i] == 0 {
                    EvidenceDesignation::ExplicitLauncherOrHook
                } else {
                    EvidenceDesignation::ConservativeTemporalInference
                };
                insert_marker(&conn, &sess, t(marker_times[i]), None, "account", evidence);
            }
            let heartbeat = if heartbeat_present {
                Some(record_heartbeat_row(&conn, &sess, t(heartbeat_at)))
            } else {
                None
            };
            let horizon = MonotonicDuration::from_seconds(50);

            let state = compose_active_activity(
                &markers_for(&conn, &sess),
                heartbeat.as_ref(),
                t(report_instant),
                horizon,
            );

            if let ActiveActivityState::ExplicitMarkerEvidence(_) = state {
                prop_assert!(heartbeat_present, "spending claimed with no heartbeat recorded at all");
                let hb_age_ok = report_instant - heartbeat_at <= horizon.as_nanos() as i64
                    || heartbeat_at >= report_instant;
                prop_assert!(hb_age_ok, "spending claimed from a heartbeat older than the horizon");
                let explicit_covers = (0..n).any(|i| {
                    marker_ranks[i] == 0 && marker_times[i] <= report_instant
                });
                prop_assert!(explicit_covers, "spending claimed with no explicit marker covering the instant");
            }
        }
    }
}
