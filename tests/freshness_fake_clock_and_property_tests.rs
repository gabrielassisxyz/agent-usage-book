//! Fake-clock sequence tests and property tests for freshness (`aub-me5.10`).
//!
//! Every test in this module drives [`compute_freshness`], the projection reader
//! ([`account_reading`]), or both, against a [`FakeClock`]. Nothing here ever reads the
//! real wall clock: `no_test_in_this_module_reads_the_system_clock` scans this file's own
//! source for that, so the guarantee is checked rather than merely intended.
//!
//! [`Snapshot`] describes one durable state exactly the way the status projection stores
//! it: an optional last successful observation and at most one latest attempt, with no
//! memory of any attempt before it. `run_pure` builds a [`FreshnessInput`] from a
//! `Snapshot` by hand and calls [`compute_freshness`] directly; `run_projection` builds
//! the equivalent [`ProjectedAccount`] and calls [`account_reading`], which calls
//! `compute_freshness` internally. Both derivations pass `None` for
//! `last_good_context`, `last_auth_failure_context` and `latest_auth_success_context`,
//! matching exactly what `account_reading` itself does (`src/projection/reader.rs`), so
//! `run_pure` and `run_projection` are held to identical inputs by construction and any
//! divergence between them is a real defect, not a difference in how this file built the
//! two calls.
//!
//! That construction also exposes a genuine gap rather than hiding it: the projection
//! format has no field for "the credential context of an earlier attempt that failed
//! authentication," so `account_reading` can never reconstruct the sticky auth condition
//! the design describes (PLAN.md section 7.5) once a *different* attempt has become the
//! latest one. `sequence_3_auth_required_then_same_context_transport_failure_then_success`
//! and the credential-context sequence both document this with a dedicated assertion on
//! `account_reading`'s actual (narrower) output, rather than asserting a result the
//! current projection cannot produce.

use agent_usage_book::domain::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted};
use agent_usage_book::domain::failure::{FailureClass, HttpStatusClass};
use agent_usage_book::domain::freshness::{
    Freshness, FreshnessInput, FreshnessKind, LatestAttempt as PureLatestAttempt, StaleReason,
    compute_freshness,
};
use agent_usage_book::domain::ids::CredentialContextId;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining, QuotaUsed};
use agent_usage_book::domain::time::{
    ClockSkewEnvelope, FakeClock, MeasurementBasis, MonotonicDuration, ProviderObservedAt,
    ReceivedAt, UtcTimestamp,
};
use agent_usage_book::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
};
use agent_usage_book::projection::reader::account_reading;
use agent_usage_book::projection::{
    LatestAttempt as ProjectedLatestAttempt, ProjectedAccount, ProjectedWindow,
    SuccessfulObservation, TerminalOutcome,
};
use agent_usage_book::store::account::AccountId;
use agent_usage_book::store::meter_evidence::ObservationRowId;

const SECOND: i64 = 1_000_000_000;

fn nanos(seconds: i64) -> i64 {
    seconds * SECOND
}

// --- shared state description, one shape both surfaces are fed from -------------------

/// One durable state, described exactly as the status projection can store it: an
/// optional last successful observation and at most one latest attempt.
#[derive(Clone)]
struct Snapshot {
    last_good_used_ppm: Option<i32>,
    provider_observed_at_nanos: Option<i64>,
    received_at_nanos: i64,
    latest_attempt: Option<AttemptSnapshot>,
    freshness_horizon: MonotonicDuration,
    command_horizon: MonotonicDuration,
    clock_skew: ClockSkewEnvelope,
}

#[derive(Clone)]
struct AttemptSnapshot {
    attempt_id: u64,
    started_at_nanos: i64,
    credential_context: String,
    /// `None` is a started attempt with no terminal result yet; `Some` carries the
    /// completion time and the outcome.
    result: Option<(i64, AttemptOutcome)>,
}

impl Snapshot {
    fn no_history(freshness_horizon_sec: i64, command_horizon_sec: i64, skew_sec: i64) -> Self {
        Self {
            last_good_used_ppm: None,
            provider_observed_at_nanos: None,
            received_at_nanos: 0,
            latest_attempt: None,
            freshness_horizon: MonotonicDuration::from_seconds(freshness_horizon_sec as u64),
            command_horizon: MonotonicDuration::from_seconds(command_horizon_sec as u64),
            clock_skew: ClockSkewEnvelope::new(MonotonicDuration::from_seconds(skew_sec as u64)),
        }
    }

    fn account_wide_window(&self, used_ppm: i32) -> ProjectedWindow {
        ProjectedWindow {
            semantic_key: "five_hour".to_string(),
            scope: WindowScope::AccountWide,
            quota_used_ppm: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            reported_resolution_ppm: ReportedResolution::new(
                QuotaFractionPpm::new(10_000).unwrap(),
            )
            .unwrap(),
            quantization: QuantizationSemantics::Exact,
            resets_at: UtcTimestamp::from_unix_nanos(nanos(999_999)),
            nominal_duration_nanos: NominalWindowDuration::from_nanos(18_000_000_000_000),
        }
    }

    fn to_projected_account(&self) -> ProjectedAccount {
        ProjectedAccount {
            account_id: AccountId::new(1),
            logical_name: "work".to_string(),
            provider: "anthropic".to_string(),
            last_successful_observation: self.last_good_used_ppm.map(|used_ppm| {
                SuccessfulObservation {
                    observation_id: ObservationRowId::new(1),
                    provider_observed_at: self
                        .provider_observed_at_nanos
                        .map(UtcTimestamp::from_unix_nanos),
                    received_at: UtcTimestamp::from_unix_nanos(self.received_at_nanos),
                    measurement_basis: MeasurementBasis::ProviderObserved,
                    windows: vec![self.account_wide_window(used_ppm)],
                }
            }),
            latest_attempt: self
                .latest_attempt
                .as_ref()
                .map(|attempt| ProjectedLatestAttempt {
                    attempt_id: AttemptId::new(attempt.attempt_id),
                    request_started_at: UtcTimestamp::from_unix_nanos(attempt.started_at_nanos),
                    credential_context_id: Some(attempt.credential_context.clone()),
                    result: attempt
                        .result
                        .map(|(completed_at, outcome)| TerminalOutcome {
                            completed_at: UtcTimestamp::from_unix_nanos(completed_at),
                            outcome,
                        }),
                }),
        }
    }
}

/// The pure function's own view of `snapshot.last_good_used_ppm`, expressed the same way
/// `account_reading` derives it (the complement of one account-wide window's used
/// fraction), so `run_pure` and `run_projection` start from the same value.
fn expected_remaining(snapshot: &Snapshot) -> Option<QuotaRemaining> {
    snapshot
        .last_good_used_ppm
        .map(|used_ppm| QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()).complement())
}

/// Runs `snapshot` through `compute_freshness` directly, deriving the input exactly the
/// way `account_reading` derives its own: `last_good_context`, `last_auth_failure_context`
/// and `latest_auth_success_context` are always `None` here, matching production
/// (`src/projection/reader.rs`). A test that needs the fuller, history-aware semantics
/// builds its own `FreshnessInput` instead of calling this helper.
fn run_pure(snapshot: &Snapshot, now_nanos: i64) -> Freshness<QuotaRemaining> {
    let last_good = expected_remaining(snapshot).map(|remaining| {
        agent_usage_book::domain::freshness::Observed::new(
            remaining,
            snapshot
                .provider_observed_at_nanos
                .map(|n| ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(n))),
            ReceivedAt::new(UtcTimestamp::from_unix_nanos(snapshot.received_at_nanos)),
            MeasurementBasis::ProviderObserved,
        )
    });

    let context_holder;
    let latest = match &snapshot.latest_attempt {
        Some(attempt) => {
            context_holder = CredentialContextId::new(attempt.credential_context.clone());
            let started = AttemptStarted::new(
                AttemptId::new(attempt.attempt_id),
                UtcTimestamp::from_unix_nanos(attempt.started_at_nanos),
            );
            let result = attempt.result.map(|(completed_at, outcome)| {
                let elapsed = (completed_at - attempt.started_at_nanos).max(0) as u64;
                AttemptResult::new(
                    AttemptId::new(attempt.attempt_id),
                    UtcTimestamp::from_unix_nanos(completed_at),
                    MonotonicDuration::from_nanos(elapsed),
                    outcome,
                )
            });
            Some(PureLatestAttempt::new(started, result, &context_holder))
        }
        None => None,
    };

    let input = FreshnessInput::new(
        last_good,
        None,
        latest,
        None,
        None,
        snapshot.freshness_horizon,
        snapshot.command_horizon,
        snapshot.clock_skew,
    );
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(now_nanos));
    compute_freshness(&input, &clock)
}

/// Runs `snapshot` through the projection reader: build the `ProjectedAccount` the
/// projection would store, then call the same `account_reading` `aub status` calls.
fn run_projection(snapshot: &Snapshot, now_nanos: i64) -> Freshness<QuotaRemaining> {
    let account = snapshot.to_projected_account();
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(now_nanos));
    account_reading(
        Some(&account),
        None,
        snapshot.freshness_horizon,
        snapshot.command_horizon,
        snapshot.clock_skew,
        &clock,
    )
    .freshness
}

// --- sequence 1: ageing and refresh, from PLAN.md 34.3 ---------------------------------

/// fresh observation at T; read at T + 1 minute -> fresh; read after freshness expiry ->
/// stale; new successful observation -> fresh. Driven through both surfaces at every
/// step against identical inputs.
#[test]
fn sequence_1_ageing_then_refresh_matches_on_both_surfaces() {
    let mut snapshot = Snapshot::no_history(300, 10, 10);
    snapshot.last_good_used_ppm = Some(200_000);
    snapshot.provider_observed_at_nanos = Some(0);
    snapshot.received_at_nanos = 0;
    snapshot.latest_attempt = Some(AttemptSnapshot {
        attempt_id: 1,
        started_at_nanos: 0,
        credential_context: "ctx-1".to_string(),
        result: Some((0, AttemptOutcome::Success)),
    });

    // Read at T + 1 minute: inside the 5-minute horizon.
    let now = nanos(60);
    assert_eq!(run_pure(&snapshot, now).kind(), FreshnessKind::Fresh);
    assert_eq!(run_projection(&snapshot, now).kind(), FreshnessKind::Fresh);
    assert_eq!(run_pure(&snapshot, now), run_projection(&snapshot, now));

    // Read after the freshness horizon expires.
    let now = nanos(301);
    let pure = run_pure(&snapshot, now);
    let projection = run_projection(&snapshot, now);
    assert_eq!(pure.kind(), FreshnessKind::Stale);
    assert_eq!(pure, projection);
    let Freshness::Stale {
        reason: StaleReason::AgeExceeded,
        ..
    } = pure
    else {
        panic!("expected Stale(AgeExceeded), got {pure:?}");
    };

    // A new successful observation replaces the latest attempt and the last good value.
    snapshot.last_good_used_ppm = Some(100_000);
    snapshot.provider_observed_at_nanos = Some(nanos(400));
    snapshot.received_at_nanos = nanos(400);
    snapshot.latest_attempt = Some(AttemptSnapshot {
        attempt_id: 2,
        started_at_nanos: nanos(400),
        credential_context: "ctx-1".to_string(),
        result: Some((nanos(400), AttemptOutcome::Success)),
    });
    let now = nanos(401);
    let pure = run_pure(&snapshot, now);
    let projection = run_projection(&snapshot, now);
    assert_eq!(pure.kind(), FreshnessKind::Fresh);
    assert_eq!(pure, projection);
}

// --- sequence 2: fresh, then a transport failure ---------------------------------------

/// fresh; transport failure -> stale, with the historical success still labeled
/// historical. Both surfaces must retain the old value under the new failure reason.
#[test]
fn sequence_2_transport_failure_after_fresh_retains_historical_value_on_both_surfaces() {
    let mut snapshot = Snapshot::no_history(300, 10, 10);
    snapshot.last_good_used_ppm = Some(300_000);
    snapshot.provider_observed_at_nanos = Some(0);
    snapshot.received_at_nanos = 0;
    snapshot.latest_attempt = Some(AttemptSnapshot {
        attempt_id: 2,
        started_at_nanos: nanos(100),
        credential_context: "ctx-1".to_string(),
        result: Some((
            nanos(100),
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        )),
    });

    let now = nanos(150);
    let pure = run_pure(&snapshot, now);
    let projection = run_projection(&snapshot, now);
    assert_eq!(pure, projection);
    let Freshness::Stale {
        last_good: Some(good),
        reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        ..
    } = pure
    else {
        panic!(
            "expected Stale(SourceUnreachable(ConnectTimeout)) with the old value, got {pure:?}"
        );
    };
    assert_eq!(good.value(), &expected_remaining(&snapshot).unwrap());
}

// --- sequence 3: auth_required, same-context transport failure, then success ----------

/// auth_required; transport failure in the *same* credential context -> the auth
/// condition remains unresolved; a successful authenticated response -> fresh.
///
/// The pure function is fed the full, history-aware input this sequence needs (the
/// credential context of the earlier auth failure carried forward as
/// `last_auth_failure_context`), which is exactly what a caller holding attempt history
/// would supply. `account_reading` cannot: the projection stores only the single latest
/// attempt, with no field for an earlier attempt's credential context, so it never learns
/// that the transport failure's context previously failed authentication. Its second step
/// is asserted against its actual, narrower output rather than against a result the
/// current projection format cannot produce.
#[test]
fn sequence_3_auth_required_then_same_context_transport_failure_then_success() {
    let ctx = CredentialContextId::new("ctx-1");
    let horizon = MonotonicDuration::from_seconds(300);
    let command_horizon = MonotonicDuration::from_seconds(10);
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

    // Step 1: auth_required. No history is needed for this step, so both surfaces agree.
    let snapshot_1 = {
        let mut s = Snapshot::no_history(300, 10, 10);
        s.latest_attempt = Some(AttemptSnapshot {
            attempt_id: 1,
            started_at_nanos: 0,
            credential_context: "ctx-1".to_string(),
            result: Some((0, AttemptOutcome::AuthRequired)),
        });
        s
    };
    let now_1 = nanos(10);
    assert_eq!(
        run_pure(&snapshot_1, now_1).kind(),
        FreshnessKind::AuthRequired
    );
    assert_eq!(
        run_projection(&snapshot_1, now_1).kind(),
        FreshnessKind::AuthRequired
    );

    // Step 2: transport failure under the same context. The pure function, given the
    // real history, retains the unresolved auth condition.
    let started_2 =
        AttemptStarted::new(AttemptId::new(2), UtcTimestamp::from_unix_nanos(nanos(20)));
    let result_2 = AttemptResult::new(
        AttemptId::new(2),
        UtcTimestamp::from_unix_nanos(nanos(20)),
        MonotonicDuration::from_seconds(0),
        AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
    );
    let input_2 = FreshnessInput::<u64>::new(
        None,
        None,
        Some(PureLatestAttempt::new(started_2, Some(result_2), &ctx)),
        Some(&ctx), // the earlier auth failure's own context, carried forward
        None,
        horizon,
        command_horizon,
        envelope,
    );
    let clock_2 = FakeClock::new(UtcTimestamp::from_unix_nanos(nanos(25)));
    let pure_2 = compute_freshness(&input_2, &clock_2);
    assert_eq!(
        pure_2.kind(),
        FreshnessKind::AuthRequired,
        "the pure function, given the earlier failure's context, retains the unresolved auth condition"
    );

    // account_reading sees only attempt 2 (a transport failure), with no memory of
    // attempt 1's auth_required: it cannot retain the auth condition and reports the
    // transport failure on its own terms instead. This is the documented gap.
    let snapshot_2 = {
        let mut s = Snapshot::no_history(300, 10, 10);
        s.latest_attempt = Some(AttemptSnapshot {
            attempt_id: 2,
            started_at_nanos: nanos(20),
            credential_context: "ctx-1".to_string(),
            result: Some((
                nanos(20),
                AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            )),
        });
        s
    };
    let projection_2 = run_projection(&snapshot_2, nanos(25));
    assert_eq!(
        projection_2.kind(),
        FreshnessKind::Stale,
        "known gap: the projection carries no cross-attempt auth-failure memory, so it \
         reports the transport failure as an ordinary stale reason instead of retaining \
         auth_required (see this file's module documentation)"
    );

    // Step 3: a successful authenticated response. Success is unconditional in
    // compute_freshness, so both surfaces agree again here.
    let snapshot_3 = {
        let mut s = Snapshot::no_history(300, 10, 10);
        s.last_good_used_ppm = Some(50_000);
        s.provider_observed_at_nanos = Some(nanos(30));
        s.received_at_nanos = nanos(30);
        s.latest_attempt = Some(AttemptSnapshot {
            attempt_id: 3,
            started_at_nanos: nanos(30),
            credential_context: "ctx-1".to_string(),
            result: Some((nanos(30), AttemptOutcome::Success)),
        });
        s
    };
    let now_3 = nanos(31);
    let pure_3 = run_pure(&snapshot_3, now_3);
    let projection_3 = run_projection(&snapshot_3, now_3);
    assert_eq!(pure_3.kind(), FreshnessKind::Fresh);
    assert_eq!(pure_3, projection_3);
}

// --- sequence 4: no prior success, a 503 ------------------------------------------------

/// no prior success; a 503 attempt -> stale, no numeric meter value, reason naming the
/// status. Both surfaces agree: neither needs any cross-attempt memory here.
#[test]
fn sequence_4_first_attempt_ever_is_a_503_yields_stale_with_no_value_on_both_surfaces() {
    let mut snapshot = Snapshot::no_history(60, 10, 10);
    snapshot.latest_attempt = Some(AttemptSnapshot {
        attempt_id: 1,
        started_at_nanos: 0,
        credential_context: "ctx-1".to_string(),
        result: Some((
            0,
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ServerError)),
        )),
    });

    let now = nanos(5);
    let pure = run_pure(&snapshot, now);
    let projection = run_projection(&snapshot, now);
    assert_eq!(pure, projection);
    let Freshness::Stale {
        last_good: None,
        reason:
            StaleReason::SourceUnreachable(FailureClass::HttpStatus(HttpStatusClass::ServerError)),
        ..
    } = pure
    else {
        panic!("expected Stale(SourceUnreachable(HttpStatus)) with no value, got {pure:?}");
    };
}

// --- sequence 5: fresh, then an auth rejection ------------------------------------------

/// fresh observation; auth rejection -> auth_required, with the previous numeric value
/// appearing only as historical. AuthRequired is unconditional on the outcome, so both
/// surfaces agree.
#[test]
fn sequence_5_auth_rejection_after_fresh_yields_auth_required_with_historical_value_on_both_surfaces()
 {
    let mut snapshot = Snapshot::no_history(300, 10, 10);
    snapshot.last_good_used_ppm = Some(150_000);
    snapshot.provider_observed_at_nanos = Some(0);
    snapshot.received_at_nanos = 0;
    snapshot.latest_attempt = Some(AttemptSnapshot {
        attempt_id: 2,
        started_at_nanos: nanos(50),
        credential_context: "ctx-1".to_string(),
        result: Some((nanos(50), AttemptOutcome::AuthRequired)),
    });

    let now = nanos(60);
    let pure = run_pure(&snapshot, now);
    let projection = run_projection(&snapshot, now);
    assert_eq!(pure, projection);
    let Freshness::AuthRequired {
        last_good: Some(good),
        ..
    } = pure
    else {
        panic!("expected AuthRequired with the historical value, got {pure:?}");
    };
    assert_eq!(good.value(), &expected_remaining(&snapshot).unwrap());
}

// --- credential-context sequence: PLAN.md 34.3's sixth scenario -----------------------

/// auth_required under credential context A; credential replaced to context B; transport
/// failure under B -> stale, reason `CredentialChangedUnverified`, not auth_required;
/// successful response under B -> fresh.
///
/// Same documented gap as sequence 3's second step: `account_reading` cannot see that the
/// transport failure under context B followed an auth failure under the *different*
/// context A, because the projection carries no memory of that earlier attempt. Its
/// second step is asserted against what it actually reports.
#[test]
fn credential_context_sequence_auth_required_replaced_transport_failure_then_success() {
    let ctx_a = CredentialContextId::new("ctx-a");
    let ctx_b = CredentialContextId::new("ctx-b");
    let horizon = MonotonicDuration::from_seconds(300);
    let command_horizon = MonotonicDuration::from_seconds(10);
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

    // Step 1: auth_required under context A.
    let snapshot_1 = {
        let mut s = Snapshot::no_history(300, 10, 10);
        s.latest_attempt = Some(AttemptSnapshot {
            attempt_id: 1,
            started_at_nanos: nanos(10),
            credential_context: "ctx-a".to_string(),
            result: Some((nanos(10), AttemptOutcome::AuthRequired)),
        });
        s
    };
    let now_1 = nanos(15);
    assert_eq!(
        run_pure(&snapshot_1, now_1).kind(),
        FreshnessKind::AuthRequired
    );
    assert_eq!(
        run_projection(&snapshot_1, now_1).kind(),
        FreshnessKind::AuthRequired
    );

    // Step 2: credential replaced to context B, transport failure under B. The pure
    // function, given the real history, reports the credential change as unverified.
    let started_2 =
        AttemptStarted::new(AttemptId::new(2), UtcTimestamp::from_unix_nanos(nanos(20)));
    let result_2 = AttemptResult::new(
        AttemptId::new(2),
        UtcTimestamp::from_unix_nanos(nanos(20)),
        MonotonicDuration::from_seconds(0),
        AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
    );
    let input_2 = FreshnessInput::<u64>::new(
        None,
        None,
        Some(PureLatestAttempt::new(started_2, Some(result_2), &ctx_b)),
        Some(&ctx_a), // the prior auth failure's context, carried forward
        None,         // context B has not yet succeeded
        horizon,
        command_horizon,
        envelope,
    );
    let clock_2 = FakeClock::new(UtcTimestamp::from_unix_nanos(nanos(25)));
    let pure_2 = compute_freshness(&input_2, &clock_2);
    let Freshness::Stale {
        last_good: None,
        reason: StaleReason::CredentialChangedUnverified,
        ..
    } = pure_2
    else {
        panic!("expected Stale(CredentialChangedUnverified), got {pure_2:?}");
    };

    // account_reading sees only attempt 2, with no memory of attempt 1's context: it
    // reports the transport failure on its own terms. This is the documented gap.
    let snapshot_2 = {
        let mut s = Snapshot::no_history(300, 10, 10);
        s.latest_attempt = Some(AttemptSnapshot {
            attempt_id: 2,
            started_at_nanos: nanos(20),
            credential_context: "ctx-b".to_string(),
            result: Some((
                nanos(20),
                AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            )),
        });
        s
    };
    let projection_2 = run_projection(&snapshot_2, nanos(25));
    let Freshness::Stale {
        last_good: None,
        reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        ..
    } = projection_2
    else {
        panic!(
            "known gap: account_reading has no cross-attempt credential-context memory, \
             expected Stale(SourceUnreachable(ConnectTimeout)), got {projection_2:?}"
        );
    };

    // Step 3: a successful response under context B. Success is unconditional, so both
    // surfaces agree again.
    let snapshot_3 = {
        let mut s = Snapshot::no_history(300, 10, 10);
        s.last_good_used_ppm = Some(10_000);
        s.provider_observed_at_nanos = Some(nanos(30));
        s.received_at_nanos = nanos(30);
        s.latest_attempt = Some(AttemptSnapshot {
            attempt_id: 3,
            started_at_nanos: nanos(30),
            credential_context: "ctx-b".to_string(),
            result: Some((nanos(30), AttemptOutcome::Success)),
        });
        s
    };
    let now_3 = nanos(31);
    let pure_3 = run_pure(&snapshot_3, now_3);
    let projection_3 = run_projection(&snapshot_3, now_3);
    assert_eq!(pure_3.kind(), FreshnessKind::Fresh);
    assert_eq!(pure_3, projection_3);
}

// --- property tests ---------------------------------------------------------------------

/// A deterministic pseudo-random generator, the same construction the rest of this
/// crate's own tests use, so the property tests below can be run in a hand-rolled sweep
/// (`time_alone_..._hand_picked`) as well as through `proptest`.
fn xorshift(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn generated_snapshot(rng: &mut impl FnMut() -> u64) -> Snapshot {
    let horizon_sec = (rng() % 300) + 10;
    let command_horizon_sec = (rng() % 30) + 5;
    let skew_sec = (rng() % 30) + 5;
    let mut snapshot = Snapshot::no_history(
        horizon_sec as i64,
        command_horizon_sec as i64,
        skew_sec as i64,
    );

    let base = nanos((rng() % 1_000) as i64 + 10);
    let has_last_good = rng().is_multiple_of(2);
    if has_last_good {
        snapshot.last_good_used_ppm = Some((rng() % 1_000_001) as i32);
        snapshot.provider_observed_at_nanos = Some(base);
        snapshot.received_at_nanos = base;
    }

    let has_attempt = rng().is_multiple_of(2);
    if has_attempt {
        let attempt_id = (rng() % 1_000) + 1;
        let ctx = if rng().is_multiple_of(2) {
            "ctx-a"
        } else {
            "ctx-b"
        };
        let has_result = rng().is_multiple_of(2);
        let result = if has_result {
            let outcome = match rng() % 4 {
                0 => AttemptOutcome::Success,
                1 => AttemptOutcome::AuthRequired,
                2 => AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                _ => AttemptOutcome::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ServerError,
                )),
            };
            Some((base, outcome))
        } else {
            None
        };
        snapshot.latest_attempt = Some(AttemptSnapshot {
            attempt_id,
            started_at_nanos: base,
            credential_context: ctx.to_string(),
            result,
        });
    }

    snapshot
}

/// Property: time alone can make fresh data stale, and time alone can never make stale or
/// auth_required data fresh. Asserted against the pure function and the projection reader
/// separately, over the same generated snapshot and the same pair of clock readings, so a
/// divergence in either surface's own ageing logic is caught.
#[test]
fn property_time_alone_ages_fresh_into_stale_and_never_reverses_on_either_surface() {
    let mut rng = xorshift(0x1234_5678_9ABC_DEF0);
    for _ in 0..500 {
        let snapshot = generated_snapshot(&mut rng);
        let t0 = nanos((rng() % 500) as i64);
        let delta = nanos(((rng() % 7200) + 1) as i64);
        let t1 = t0 + delta;

        for run in [
            run_pure as fn(&Snapshot, i64) -> Freshness<QuotaRemaining>,
            run_projection,
        ] {
            let initial = run(&snapshot, t0);
            let later = run(&snapshot, t1);
            match initial.kind() {
                FreshnessKind::Stale => assert_ne!(
                    later.kind(),
                    FreshnessKind::Fresh,
                    "time alone must never turn Stale into Fresh"
                ),
                FreshnessKind::AuthRequired => assert_ne!(
                    later.kind(),
                    FreshnessKind::Fresh,
                    "time alone must never turn AuthRequired into Fresh"
                ),
                FreshnessKind::Fresh => {
                    assert_ne!(
                        later.kind(),
                        FreshnessKind::AuthRequired,
                        "time alone must never turn Fresh into AuthRequired"
                    );
                    if delta > snapshot.freshness_horizon.as_nanos() as i64 {
                        assert_eq!(
                            later.kind(),
                            FreshnessKind::Stale,
                            "fresh data must age into stale past the freshness horizon"
                        );
                    }
                }
            }
        }
    }
}

/// Property: a historical value's own timestamps are never rewritten to the read-time
/// clock. Over generated snapshots and read times, whenever the result carries a
/// `last_good`/`observed` value, its `received_at` is the snapshot's own value, not
/// whatever `now` happened to be for that read. Asserted against both surfaces.
#[test]
fn property_historical_timestamps_are_never_rewritten_to_now_on_either_surface() {
    let mut rng = xorshift(0xFEED_FACE_C0FF_EE00);
    for _ in 0..500 {
        let mut snapshot = generated_snapshot(&mut rng);
        // Force a last-good value and a matching Success attempt, so the property
        // exercises the Fresh path (the one branch that actually clones the observed
        // value into the result) at least as often as the Stale paths.
        let base = nanos((rng() % 1_000) as i64 + 10);
        snapshot.last_good_used_ppm = Some((rng() % 1_000_001) as i32);
        snapshot.provider_observed_at_nanos = Some(base);
        snapshot.received_at_nanos = base;
        snapshot.latest_attempt = Some(AttemptSnapshot {
            attempt_id: (rng() % 1_000) + 1,
            started_at_nanos: base,
            credential_context: "ctx-1".to_string(),
            result: Some((base, AttemptOutcome::Success)),
        });

        // One read inside the freshness horizon (Fresh) and one past it (Stale), so
        // both branches that carry the observation forward are exercised every time.
        let horizon_nanos = snapshot.freshness_horizon.as_nanos() as i64;
        for now in [
            base + (rng() % horizon_nanos.max(1) as u64) as i64,
            base + horizon_nanos + nanos((rng() % 1_000) as i64 + 1),
        ] {
            for observed in [
                extract_observed(run_pure(&snapshot, now)),
                extract_observed(run_projection(&snapshot, now)),
            ]
            .into_iter()
            .flatten()
            {
                assert_eq!(
                    observed.received_at().as_utc().unix_nanos(),
                    snapshot.received_at_nanos,
                    "a historical received_at must never be rewritten to the read-time clock"
                );
            }
        }
    }
}

fn extract_observed(
    freshness: Freshness<QuotaRemaining>,
) -> Option<agent_usage_book::domain::freshness::Observed<QuotaRemaining>> {
    match freshness {
        Freshness::Fresh { observed, .. } => Some(observed),
        Freshness::Stale { last_good, .. } | Freshness::AuthRequired { last_good, .. } => last_good,
    }
}

/// Property: authentication cannot become resolved into Fresh without evidence of an
/// authenticated success. Over a generated two-attempt history where the first attempt is
/// auth_required, the second attempt's outcome (and whether its credential context
/// matches) is generated freely; the resulting state can only be Fresh when that second
/// attempt's own outcome was Success. This is the pure function's own guarantee: it is
/// the surface where the sticky auth condition actually lives (see this file's module
/// documentation for why `account_reading` cannot exercise the same multi-attempt
/// history).
#[test]
fn property_auth_required_resolves_into_fresh_only_through_an_authenticated_success() {
    let mut rng = xorshift(0x0BAD_C0DE_1234_5678);
    let horizon = MonotonicDuration::from_seconds(300);
    let command_horizon = MonotonicDuration::from_seconds(10);
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

    for _ in 0..500 {
        let ctx_a = CredentialContextId::new("ctx-a");
        let ctx_b = CredentialContextId::new("ctx-b");
        let contexts = [&ctx_a, &ctx_b];
        let first_ctx_idx = (rng() % 2) as usize;
        let second_ctx_idx = (rng() % 2) as usize;
        let second_ctx = contexts[second_ctx_idx];

        let outcome_choice = rng() % 4;
        let (outcome, expect_success) = match outcome_choice {
            0 => (AttemptOutcome::Success, true),
            1 => (AttemptOutcome::AuthRequired, false),
            2 => (
                AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                false,
            ),
            _ => (
                AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ServerError)),
                false,
            ),
        };

        let started_2 =
            AttemptStarted::new(AttemptId::new(2), UtcTimestamp::from_unix_nanos(nanos(20)));
        let result_2 = AttemptResult::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(nanos(20)),
            MonotonicDuration::from_seconds(0),
            outcome,
        );
        let input_2 = FreshnessInput::<u64>::new(
            None,
            None,
            Some(PureLatestAttempt::new(
                started_2,
                Some(result_2),
                second_ctx,
            )),
            Some(contexts[first_ctx_idx]),
            None,
            horizon,
            command_horizon,
            envelope,
        );
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(nanos(21)));
        let result = compute_freshness(&input_2, &clock);

        if result.kind() == FreshnessKind::Fresh {
            assert!(
                expect_success,
                "auth_required resolved into Fresh without an authenticated success: outcome was {outcome:?}"
            );
        }
    }
}

/// Property: the projection reader and the pure function produce identical freshness for
/// generated identical inputs. This is `run_pure` and `run_projection` compared directly
/// over a wide generated space, so the two surfaces are held to one state machine
/// mechanically rather than by two people remembering to keep them in sync.
#[test]
fn property_projection_reader_and_pure_function_agree_on_generated_identical_inputs() {
    let mut rng = xorshift(0xA5A5_5A5A_1122_3344);
    for _ in 0..1000 {
        let snapshot = generated_snapshot(&mut rng);
        let now = nanos((rng() % 5_000) as i64);
        assert_eq!(
            run_pure(&snapshot, now),
            run_projection(&snapshot, now),
            "the projection reader and the pure function must agree on identical inputs"
        );
    }
}

proptest::proptest! {
    /// The same identical-input parity property, run through `proptest` in addition to
    /// the hand-rolled sweep above, so a shrunk failing case is available if this ever
    /// regresses.
    #[test]
    fn prop_projection_reader_and_pure_function_agree(
        seed in proptest::prelude::any::<u64>(),
    ) {
        let mut rng = xorshift(seed);
        let snapshot = generated_snapshot(&mut rng);
        let now = nanos((rng() % 5_000) as i64);
        proptest::prop_assert_eq!(run_pure(&snapshot, now), run_projection(&snapshot, now));
    }
}

// --- exhaustive coverage, no wildcard arm -----------------------------------------------

/// Every `AttemptOutcome` variant maps into a named `FreshnessKind`, matched here with no
/// wildcard arm: a variant added to `AttemptOutcome` without a case added here fails to
/// compile before it fails anywhere else.
#[test]
fn every_attempt_outcome_variant_is_matched_exhaustively_with_no_wildcard_arm() {
    let ctx = CredentialContextId::new("ctx-exhaustive");
    let outcomes = [
        AttemptOutcome::Success,
        AttemptOutcome::AuthRequired,
        AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
    ];
    for outcome in outcomes {
        let started =
            AttemptStarted::new(AttemptId::new(1), UtcTimestamp::from_unix_nanos(nanos(1)));
        let result = AttemptResult::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(nanos(1)),
            MonotonicDuration::from_seconds(0),
            outcome,
        );
        let input: FreshnessInput<'_, u64> = FreshnessInput::new(
            None,
            None,
            Some(PureLatestAttempt::new(started, Some(result), &ctx)),
            None,
            Some(&ctx),
            MonotonicDuration::from_seconds(60),
            MonotonicDuration::from_seconds(10),
            ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10)),
        );
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(nanos(2)));
        let freshness = compute_freshness(&input, &clock);

        match outcome {
            AttemptOutcome::Success => {
                // No last_good exists in this scenario, so a success with nothing to
                // confirm is itself the design's NoSuccessfulObservation case.
                assert_eq!(freshness.kind(), FreshnessKind::Stale);
            }
            AttemptOutcome::AuthRequired => {
                assert_eq!(freshness.kind(), FreshnessKind::AuthRequired);
            }
            AttemptOutcome::Unreachable(_) => {
                assert_eq!(freshness.kind(), FreshnessKind::Stale);
            }
        }
    }
}

/// Every `StaleReason` variant is reachable through the production state machine, matched
/// here with no wildcard arm: a variant added to `StaleReason` without a case added here
/// fails to compile before it fails anywhere else.
#[test]
fn every_stale_reason_variant_is_reachable_with_no_wildcard_arm() {
    let ctx = CredentialContextId::new("ctx-a");
    let ctx_other = CredentialContextId::new("ctx-b");
    let horizon = MonotonicDuration::from_seconds(60);
    let command_horizon = MonotonicDuration::from_seconds(10);
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

    let cases: Vec<(&str, StaleReason, FreshnessInput<'_, u64>, i64)> = vec![
        (
            "age_exceeded",
            StaleReason::AgeExceeded,
            {
                let observed = agent_usage_book::domain::freshness::Observed::new(
                    7u64,
                    Some(ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(0))),
                    ReceivedAt::new(UtcTimestamp::from_unix_nanos(0)),
                    MeasurementBasis::ProviderObserved,
                );
                let started =
                    AttemptStarted::new(AttemptId::new(1), UtcTimestamp::from_unix_nanos(0));
                let result = AttemptResult::new(
                    AttemptId::new(1),
                    UtcTimestamp::from_unix_nanos(0),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Success,
                );
                FreshnessInput::new(
                    Some(observed),
                    None,
                    Some(PureLatestAttempt::new(started, Some(result), &ctx)),
                    None,
                    Some(&ctx),
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(120),
        ),
        (
            "no_successful_observation",
            StaleReason::NoSuccessfulObservation,
            FreshnessInput::new(
                None,
                None,
                None,
                None,
                None,
                horizon,
                command_horizon,
                envelope,
            ),
            nanos(1),
        ),
        (
            "source_unreachable",
            StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
            {
                let started =
                    AttemptStarted::new(AttemptId::new(2), UtcTimestamp::from_unix_nanos(0));
                let result = AttemptResult::new(
                    AttemptId::new(2),
                    UtcTimestamp::from_unix_nanos(0),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                );
                FreshnessInput::new(
                    None,
                    None,
                    Some(PureLatestAttempt::new(started, Some(result), &ctx)),
                    None,
                    Some(&ctx),
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(1),
        ),
        (
            "malformed_provider_response",
            StaleReason::MalformedProviderResponse,
            {
                let started =
                    AttemptStarted::new(AttemptId::new(3), UtcTimestamp::from_unix_nanos(0));
                let result = AttemptResult::new(
                    AttemptId::new(3),
                    UtcTimestamp::from_unix_nanos(0),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::MalformedBody),
                );
                FreshnessInput::new(
                    None,
                    None,
                    Some(PureLatestAttempt::new(started, Some(result), &ctx)),
                    None,
                    Some(&ctx),
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(1),
        ),
        (
            "rate_limited",
            StaleReason::RateLimited,
            {
                let started =
                    AttemptStarted::new(AttemptId::new(4), UtcTimestamp::from_unix_nanos(0));
                let result = AttemptResult::new(
                    AttemptId::new(4),
                    UtcTimestamp::from_unix_nanos(0),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::RateLimited { retry_after: None }),
                );
                FreshnessInput::new(
                    None,
                    None,
                    Some(PureLatestAttempt::new(started, Some(result), &ctx)),
                    None,
                    Some(&ctx),
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(1),
        ),
        (
            "sampling_gap",
            StaleReason::SamplingGap,
            {
                let observed = agent_usage_book::domain::freshness::Observed::new(
                    9u64,
                    Some(ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(0))),
                    ReceivedAt::new(UtcTimestamp::from_unix_nanos(0)),
                    MeasurementBasis::ProviderObserved,
                );
                FreshnessInput::new(
                    Some(observed),
                    None,
                    None,
                    None,
                    None,
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(1),
        ),
        (
            "clock_anomaly",
            StaleReason::ClockAnomaly,
            {
                let observed = agent_usage_book::domain::freshness::Observed::new(
                    11u64,
                    Some(ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(
                        nanos(100),
                    ))),
                    ReceivedAt::new(UtcTimestamp::from_unix_nanos(0)),
                    MeasurementBasis::ProviderObserved,
                );
                let started =
                    AttemptStarted::new(AttemptId::new(5), UtcTimestamp::from_unix_nanos(0));
                let result = AttemptResult::new(
                    AttemptId::new(5),
                    UtcTimestamp::from_unix_nanos(0),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Success,
                );
                FreshnessInput::new(
                    Some(observed),
                    None,
                    Some(PureLatestAttempt::new(started, Some(result), &ctx)),
                    None,
                    Some(&ctx),
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(1),
        ),
        (
            "collector_interrupted",
            StaleReason::CollectorInterrupted,
            {
                let started =
                    AttemptStarted::new(AttemptId::new(6), UtcTimestamp::from_unix_nanos(nanos(5)));
                FreshnessInput::new(
                    None,
                    None,
                    Some(PureLatestAttempt::new(started, None, &ctx)),
                    None,
                    Some(&ctx),
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(20),
        ),
        (
            "credential_changed_unverified",
            StaleReason::CredentialChangedUnverified,
            {
                let started =
                    AttemptStarted::new(AttemptId::new(7), UtcTimestamp::from_unix_nanos(0));
                let result = AttemptResult::new(
                    AttemptId::new(7),
                    UtcTimestamp::from_unix_nanos(0),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                );
                FreshnessInput::new(
                    None,
                    None,
                    Some(PureLatestAttempt::new(started, Some(result), &ctx_other)),
                    Some(&ctx),
                    None,
                    horizon,
                    command_horizon,
                    envelope,
                )
            },
            nanos(1),
        ),
    ];

    assert_eq!(
        cases.len(),
        9,
        "all nine StaleReason variants must be exercised here"
    );

    for (label, expected, input, now_nanos) in cases {
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(now_nanos));
        let freshness = compute_freshness(&input, &clock);
        let Freshness::Stale { reason, .. } = freshness else {
            panic!("case {label}: expected Stale, got {freshness:?}");
        };
        match reason {
            StaleReason::AgeExceeded
            | StaleReason::NoSuccessfulObservation
            | StaleReason::SourceUnreachable(_)
            | StaleReason::MalformedProviderResponse
            | StaleReason::RateLimited
            | StaleReason::SamplingGap
            | StaleReason::ClockAnomaly
            | StaleReason::CollectorInterrupted
            | StaleReason::CredentialChangedUnverified => {
                assert_eq!(reason, expected, "case {label} produced the wrong reason");
            }
        }
    }
}

// --- no test in this module reads the system clock --------------------------------------

/// Every clock in this module is a `FakeClock`. This scans the file's own source rather
/// than trusting that convention to hold, so a test added later that reaches for the real
/// wall clock is caught here instead of becoming a flaky suite.
#[test]
fn no_test_in_this_module_reads_the_system_clock() {
    let source = include_str!("freshness_fake_clock_and_property_tests.rs");
    // Each pattern is assembled from two halves at runtime so the literal, contiguous
    // string never appears in this file's own source: a plain string constant would
    // match its own forbidden-pattern list and fail this test against itself.
    let forbidden: [String; 5] = [
        ["System", "Time::now"].concat(),
        ["Instant", "::now"].concat(),
        ["RealClock", "::new"].concat(),
        ["RealClock", "::default"].concat(),
        ["chrono::", "Utc::now"].concat(),
    ];
    for pattern in &forbidden {
        assert!(
            !source.contains(pattern.as_str()),
            "this module must never read the real wall clock: found {pattern}"
        );
    }
}
