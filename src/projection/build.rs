//! Mapping the durable-state snapshot onto the projection file format.
//!
//! Every field the design lists (PLAN.md section 16.1) is copied from the
//! store's typed rows, and nothing else is: the mapping is the boundary where
//! "the projection contains exactly the listed fields" is kept true. The six
//! the design forbids have no source here, so they cannot reach the file.

use crate::domain::attempt::AttemptId;
use crate::store::ledger_generation::Generation;
use crate::store::projection_source::{AccountMeterState, SuccessfulObservation as StoredSuccess};

use super::{
    LatestAttempt, ProjectedAccount, ProjectedWindow, Projection, SuccessfulObservation,
    TerminalOutcome,
};

/// Builds the projection value from one account-state snapshot.
///
/// Accounts carry their storage order (account-identity order), so the file
/// is deterministic over unchanged database state.
pub(crate) fn projection(generation: Generation, states: &[AccountMeterState]) -> Projection {
    Projection {
        ledger_generation: generation,
        accounts: states.iter().map(projected_account).collect(),
    }
}

fn projected_account(state: &AccountMeterState) -> ProjectedAccount {
    let account = &state.account;
    ProjectedAccount {
        account_id: account.id(),
        logical_name: account.logical_name().to_owned(),
        provider: account.provider_key().to_owned(),
        last_successful_observation: state.last_success.as_ref().map(projected_success),
        latest_attempt: state.latest_attempt.as_ref().map(|latest| LatestAttempt {
            attempt_id: attempt_id_of(&latest.attempt.row_id),
            request_started_at: latest.attempt.request_started_at,
            credential_context_id: latest.attempt.credential_context_id.clone(),
            result: latest.result.as_ref().map(|stored| TerminalOutcome {
                completed_at: stored.completed_at,
                outcome: stored.outcome,
            }),
        }),
    }
}

fn projected_success(success: &StoredSuccess) -> SuccessfulObservation {
    SuccessfulObservation {
        observation_id: success.observation.row_id,
        provider_observed_at: success.observation.provider_observed_at,
        received_at: success.observation.received_at,
        measurement_basis: success.observation.measurement_basis,
        windows: success.windows.iter().map(projected_window).collect(),
    }
}

fn projected_window(window: &crate::store::meter_evidence::StoredMeterWindow) -> ProjectedWindow {
    ProjectedWindow {
        semantic_key: window.semantic_key.as_str().to_owned(),
        scope: window.scope.clone(),
        quota_used_ppm: window.quota_used,
        reported_resolution_ppm: window.reported_resolution,
        quantization: window.quantization,
        resets_at: window.resets_at,
        nominal_duration_nanos: window.nominal_duration,
    }
}

/// The attempt rowid the store uses, as the domain attempt identity the
/// projection names. The two are the same number by this schema's definition
/// (`MeterAttemptRowId::as_attempt_id`), and a negative rowid cannot occur
/// through this crate's write path; a corrupt one is refused rather than
/// silently narrowed.
fn attempt_id_of(row_id: &crate::store::meter_attempt::MeterAttemptRowId) -> AttemptId {
    row_id
        .as_attempt_id()
        .expect("a meter_attempt rowid read back from the database is a valid attempt identity")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptOutcome;
    use crate::domain::failure::FailureClass;
    use crate::domain::time::{MeasurementBasis, MonotonicDuration, UtcTimestamp};
    use crate::store::ledger_generation::Generation;
    use crate::store::meter_attempt::MeterAttemptRowId;
    use crate::store::projection_source::{account_meter_states, test_support::fixture};

    #[test]
    fn the_mapping_carries_the_observation_inputs_and_attempt_state_unchanged() {
        let mut fixture = fixture("mapping-full");
        let attempt = fixture.start_attempt();
        fixture.commit_success_bundle(attempt);

        let states = account_meter_states(&fixture.conn).unwrap();
        let built = projection(Generation::new(5), &states);

        assert_eq!(built.ledger_generation, Generation::new(5));
        assert_eq!(built.accounts.len(), 1);
        let projected = &built.accounts[0];
        assert_eq!(projected.account_id, fixture.account_id);
        assert_eq!(projected.logical_name, "work");
        assert_eq!(projected.provider, "anthropic");

        let success = projected.last_successful_observation.as_ref().unwrap();
        assert_eq!(success.observation_id.value(), 1);
        assert_eq!(
            success.provider_observed_at,
            Some(UtcTimestamp::from_unix_nanos(10_500_003_000))
        );
        assert_eq!(
            success.received_at,
            UtcTimestamp::from_unix_nanos(10_500_003_000)
        );
        assert_eq!(
            success.measurement_basis,
            MeasurementBasis::ProviderObserved
        );
        assert_eq!(success.windows.len(), 1);

        let attempt = projected.latest_attempt.as_ref().unwrap();
        assert_eq!(
            attempt.attempt_id,
            crate::domain::attempt::AttemptId::new(1)
        );
        assert_eq!(
            attempt.request_started_at,
            UtcTimestamp::from_unix_nanos(10_000_003_000)
        );
        assert_eq!(
            attempt.credential_context_id.as_deref(),
            Some("credential-context-v1")
        );
        assert!(matches!(
            &attempt.result.as_ref().unwrap().outcome,
            AttemptOutcome::Success
        ));
    }

    #[test]
    fn the_mapping_of_an_empty_account_carries_the_absence_facts() {
        let fixture = fixture("mapping-empty");
        let states = account_meter_states(&fixture.conn).unwrap();
        let built = projection(Generation::new(0), &states);
        let projected = &built.accounts[0];
        assert!(projected.last_successful_observation.is_none());
        assert!(projected.latest_attempt.is_none());
    }

    #[test]
    fn a_started_attempt_with_no_result_maps_to_a_result_less_latest_attempt() {
        let mut fixture = fixture("mapping-started-no-result");
        fixture.start_attempt();
        let states = account_meter_states(&fixture.conn).unwrap();
        let built = projection(Generation::new(1), &states);
        let attempt = built.accounts[0].latest_attempt.as_ref().unwrap();
        assert!(
            attempt.result.is_none(),
            "the fact that the attempt has no terminal outcome is itself carried"
        );
        assert!(built.accounts[0].last_successful_observation.is_none());
    }

    #[test]
    fn a_failure_only_history_maps_the_newer_failed_attempt_and_keeps_the_older_success() {
        let mut fixture = fixture("mapping-failure-after-success");
        let first = fixture.start_attempt();
        fixture.commit_success_bundle(first);
        let second = fixture.start_attempt();
        fixture.commit_failure(second, FailureClass::ConnectTimeout);

        let states = account_meter_states(&fixture.conn).unwrap();
        let built = projection(Generation::new(3), &states);
        let projected = &built.accounts[0];
        assert_eq!(
            projected
                .last_successful_observation
                .as_ref()
                .unwrap()
                .observation_id
                .value(),
            1,
            "the last success stays anchored to the successful attempt"
        );
        let latest = projected.latest_attempt.as_ref().unwrap();
        assert_eq!(latest.attempt_id, crate::domain::attempt::AttemptId::new(2));
        assert!(matches!(
            &latest.result.as_ref().unwrap().outcome,
            AttemptOutcome::Unreachable(FailureClass::ReadTimeout)
        ));
    }

    #[test]
    fn the_mapped_attempt_identity_is_the_domain_identity_of_the_row() {
        assert_eq!(
            attempt_id_of(&MeterAttemptRowId::new(42)),
            crate::domain::attempt::AttemptId::new(42),
            "the projection must name the attempt by the identity the database gave it"
        );
        let _ = MonotonicDuration::from_nanos(1);
    }
}
