//! The synthetic provider adapter (`aub-me5.1`), test-only.
//!
//! Implements [`ProviderAdapter`] from `aub-lveh` through a deterministic script: a
//! sequence of [`ScriptedResponse`] values, one per call to [`SyntheticAdapter::observe`],
//! with no randomness anywhere. A real provider will not time out when a test needs it
//! to, will not return a malformed body on request, and will not change its reset
//! timestamp unexpectedly; every one of those is a first-class state this design gives a
//! name to, and a state that has never been produced is a state nobody has tested.
//!
//! This module is `#[cfg(test)]`-gated in `crate::meter`, so it does not exist in a
//! release build at all: the compiler excludes it entirely rather than merely hiding it
//! behind a runtime flag.
//!
//! May not depend on:
//! - SQLite (rule `03`)
//! - credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`)

use std::cell::Cell;

use crate::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
use crate::domain::time::{Clock, MonotonicDuration, ProviderObservedAt};
use crate::domain::window::MeterWindow;
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation,
};

/// The typed success reading this adapter can be scripted to return: the windows a
/// success response carries, plus the provider's own reported observation time where a
/// script wants to exercise freshness handling downstream (`aub-rif.9`). `None` means
/// the source documents no measurement time of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticReading {
    pub windows: Vec<MeterWindow>,
    pub provider_observed_at: Option<ProviderObservedAt>,
}

impl SyntheticReading {
    pub fn new(windows: Vec<MeterWindow>) -> Self {
        Self {
            windows,
            provider_observed_at: None,
        }
    }

    pub fn with_provider_observed_at(mut self, observed_at: ProviderObservedAt) -> Self {
        self.provider_observed_at = Some(observed_at);
        self
    }
}

/// One entry in a synthetic adapter's script: exactly the outcome the next `observe`
/// call returns. A success case (windows, zero percentage, multiple windows, a
/// model-specific window, a stale server timestamp, a changed reset timestamp, and an
/// unknown additional field tolerated upstream) is a data variation of [`Self::Success`]
/// rather than a separate variant: this adapter receives already-typed readings, never
/// raw provider bytes, so "an unknown field" and "a stale timestamp" are properties of
/// the scripted data, not of a parsing step this boundary does not perform. Parsing raw
/// provider bytes into a typed reading is the real adapter's job (`aub-eun.4`); this
/// adapter proves what every caller of the trait must handle once parsing has already
/// happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedResponse {
    /// A usable reading.
    Success(SyntheticReading),
    /// A sticky authentication conclusion (401, or a provider-declared expiry).
    Unauthorized(AuthReason),
    /// An ambiguous 403: not classified as authentication here (see
    /// [`crate::meter::adapter::ProviderObservation::AuthRequired`]'s doc), just an
    /// unreachable client-error status.
    AmbiguousForbidden,
    /// A 429, with the provider's advertised retry delay where one was given.
    RateLimited {
        retry_after: Option<MonotonicDuration>,
    },
    /// The provider did not answer in time.
    Timeout,
    /// The response body could not be parsed at all.
    MalformedBody,
    /// The response parsed, but a field the contract requires was absent.
    MissingRequiredField,
}

/// A provider adapter whose every response is programmed in advance.
///
/// `observe` advances through the script deterministically: the same script produces
/// the same sequence of outcomes on every run, which is what makes the failure cases
/// usable as fixtures. Calling `observe` more times than the script has entries is a
/// test-authoring error, not a reachable production state, so it panics rather than
/// silently repeating or wrapping.
pub struct SyntheticAdapter {
    declarations: AdapterDeclarations,
    script: Vec<ScriptedResponse>,
    next: Cell<usize>,
}

impl SyntheticAdapter {
    pub fn new(declarations: AdapterDeclarations, script: Vec<ScriptedResponse>) -> Self {
        Self {
            declarations,
            script,
            next: Cell::new(0),
        }
    }

    /// How many scripted responses have been consumed so far.
    pub fn calls_made(&self) -> usize {
        self.next.get()
    }
}

impl ProviderAdapter for SyntheticAdapter {
    type Reading = SyntheticReading;

    fn declarations(&self) -> AdapterDeclarations {
        self.declarations.clone()
    }

    fn observe(
        &self,
        _credential: &CredentialHandle,
        _request: &MeterRequest,
        _transport: &impl HttpTransport,
        _clock: &impl Clock,
    ) -> ProviderObservation<Self::Reading> {
        let index = self.next.get();
        let response = self.script.get(index).unwrap_or_else(|| {
            panic!("synthetic adapter script exhausted: call {index} has no scripted response")
        });
        self.next.set(index + 1);
        match response {
            ScriptedResponse::Success(reading) => ProviderObservation::Measured(reading.clone()),
            ScriptedResponse::Unauthorized(reason) => ProviderObservation::AuthRequired(*reason),
            ScriptedResponse::AmbiguousForbidden => ProviderObservation::Unreachable(
                FailureClass::HttpStatus(HttpStatusClass::ClientError),
            ),
            ScriptedResponse::RateLimited { retry_after } => {
                ProviderObservation::Unreachable(FailureClass::RateLimited {
                    retry_after: *retry_after,
                })
            }
            ScriptedResponse::Timeout => {
                ProviderObservation::Unreachable(FailureClass::ReadTimeout)
            }
            ScriptedResponse::MalformedBody => {
                ProviderObservation::Unreachable(FailureClass::MalformedBody)
            }
            ScriptedResponse::MissingRequiredField => {
                ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{MeterSemanticsId, ProviderContractId};
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::{FakeClock, MeasurementBasis, UtcTimestamp};
    use crate::domain::window::{
        ModelId, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
        WindowSemanticKey,
    };
    use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse};

    struct UnusedTransport;

    impl HttpTransport for UnusedTransport {
        fn send(
            &self,
            _request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            panic!("the synthetic adapter never calls its transport")
        }
    }

    fn declarations() -> AdapterDeclarations {
        AdapterDeclarations::new(
            MeasurementBasis::ProviderObserved,
            ProviderContractId::new("synthetic-v1"),
            MeterSemanticsId::new("synthetic-v1"),
        )
    }

    fn window(key: &str, scope: WindowScope, used_ppm: i32, resets_at_nanos: i64) -> MeterWindow {
        MeterWindow::new(
            WindowSemanticKey::new(key),
            scope,
            QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(1).unwrap()).unwrap(),
            QuantizationSemantics::Exact,
            UtcTimestamp::from_unix_nanos(resets_at_nanos),
            NominalWindowDuration::from_nanos(3_600_000_000_000),
        )
    }

    fn observe(adapter: &SyntheticAdapter) -> ProviderObservation<SyntheticReading> {
        adapter.observe(
            &CredentialHandle::new("synthetic"),
            &MeterRequest::default(),
            &UnusedTransport,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
    }

    /// Case 1: a valid success with one account-wide window.
    #[test]
    fn produces_a_valid_success() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Success(SyntheticReading::new(vec![
                window("primary", WindowScope::AccountWide, 250_000, 1_000),
            ]))],
        );
        let ProviderObservation::Measured(reading) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        assert_eq!(reading.windows.len(), 1);
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 250_000);
    }

    /// Case 2: a zero percentage is a real, explicit measurement, not an absence.
    #[test]
    fn produces_a_zero_percentage() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Success(SyntheticReading::new(vec![
                window("primary", WindowScope::AccountWide, 0, 1_000),
            ]))],
        );
        let ProviderObservation::Measured(reading) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 0);
    }

    /// Case 3: multiple independently calibrated windows in one reading.
    #[test]
    fn produces_multiple_windows() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Success(SyntheticReading::new(vec![
                window("five-hour", WindowScope::AccountWide, 100_000, 1_000),
                window("weekly", WindowScope::AccountWide, 400_000, 2_000),
            ]))],
        );
        let ProviderObservation::Measured(reading) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        assert_eq!(reading.windows.len(), 2);
    }

    /// Case 4: a window scoped to one model rather than the whole account.
    #[test]
    fn produces_a_model_specific_window() {
        let model = ModelId::new("opus");
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Success(SyntheticReading::new(vec![
                window(
                    "opus-window",
                    WindowScope::ModelSpecific(model.clone()),
                    50_000,
                    1_000,
                ),
            ]))],
        );
        let ProviderObservation::Measured(reading) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        assert_eq!(reading.windows[0].scope().scoped_model(), Some(&model));
    }

    /// Case 5: a 401 is a sticky authentication conclusion.
    #[test]
    fn produces_a_401() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Unauthorized(
                AuthReason::CredentialRejected,
            )],
        );
        assert_eq!(
            observe(&adapter),
            ProviderObservation::AuthRequired(AuthReason::CredentialRejected)
        );
    }

    /// Case 6: the provider itself declared the credential's authentication expired.
    #[test]
    fn produces_a_provider_declared_authentication_expiry() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Unauthorized(
                AuthReason::ProviderDeclaredExpiry,
            )],
        );
        assert_eq!(
            observe(&adapter),
            ProviderObservation::AuthRequired(AuthReason::ProviderDeclaredExpiry)
        );
    }

    /// Case 7: an ambiguous 403 is never classified as authentication here; it is an
    /// unreachable observation with an HTTP status class instead.
    #[test]
    fn produces_an_ambiguous_403() {
        let adapter =
            SyntheticAdapter::new(declarations(), vec![ScriptedResponse::AmbiguousForbidden]);
        assert_eq!(
            observe(&adapter),
            ProviderObservation::Unreachable(FailureClass::HttpStatus(
                HttpStatusClass::ClientError
            ))
        );
    }

    /// Case 8: a 429, with the provider's advertised retry delay.
    #[test]
    fn produces_a_429() {
        let retry_after = MonotonicDuration::from_seconds(30);
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::RateLimited {
                retry_after: Some(retry_after),
            }],
        );
        assert_eq!(
            observe(&adapter),
            ProviderObservation::Unreachable(FailureClass::RateLimited {
                retry_after: Some(retry_after)
            })
        );
    }

    /// Case 9: the provider did not answer in time.
    #[test]
    fn produces_a_timeout() {
        let adapter = SyntheticAdapter::new(declarations(), vec![ScriptedResponse::Timeout]);
        assert_eq!(
            observe(&adapter),
            ProviderObservation::Unreachable(FailureClass::ReadTimeout)
        );
    }

    /// Case 10: the response body could not be parsed at all.
    #[test]
    fn produces_malformed_json() {
        let adapter = SyntheticAdapter::new(declarations(), vec![ScriptedResponse::MalformedBody]);
        assert_eq!(
            observe(&adapter),
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        );
    }

    /// Case 11: the response parsed, but a required field was absent.
    #[test]
    fn produces_a_missing_expected_field() {
        let adapter =
            SyntheticAdapter::new(declarations(), vec![ScriptedResponse::MissingRequiredField]);
        assert_eq!(
            observe(&adapter),
            ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
        );
    }

    /// Case 12: an unknown additional field is tolerated upstream (`aub-eun.4` parses
    /// it away), so the reading this boundary receives is an ordinary success; nothing
    /// about the extra field survives to this adapter's own contract.
    #[test]
    fn produces_a_success_standing_in_for_an_unknown_additional_field_tolerated_upstream() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Success(SyntheticReading::new(vec![
                window("primary", WindowScope::AccountWide, 10_000, 1_000),
            ]))],
        );
        assert!(matches!(
            observe(&adapter),
            ProviderObservation::Measured(_)
        ));
    }

    /// Case 13: a stale server timestamp, old enough relative to a caller's clock that
    /// downstream freshness handling (`aub-rif.9`) must classify it as stale rather
    /// than fresh.
    #[test]
    fn produces_a_stale_server_timestamp() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![ScriptedResponse::Success(
                SyntheticReading::new(vec![window(
                    "primary",
                    WindowScope::AccountWide,
                    10_000,
                    1_000,
                )])
                .with_provider_observed_at(ProviderObservedAt::new(
                    UtcTimestamp::from_unix_nanos(0),
                )),
            )],
        );
        let ProviderObservation::Measured(reading) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        assert_eq!(
            reading.provider_observed_at,
            Some(ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(0)))
        );
    }

    /// Case 14: the provider's declared reset timestamp changes between two calls for
    /// what a caller identifies as the same window.
    #[test]
    fn produces_a_changed_reset_timestamp_across_two_calls() {
        let adapter = SyntheticAdapter::new(
            declarations(),
            vec![
                ScriptedResponse::Success(SyntheticReading::new(vec![window(
                    "primary",
                    WindowScope::AccountWide,
                    10_000,
                    1_000,
                )])),
                ScriptedResponse::Success(SyntheticReading::new(vec![window(
                    "primary",
                    WindowScope::AccountWide,
                    10_000,
                    9_999,
                )])),
            ],
        );
        let ProviderObservation::Measured(first) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        let ProviderObservation::Measured(second) = observe(&adapter) else {
            panic!("expected a measured reading");
        };
        assert_ne!(
            first.windows[0].resets_at(),
            second.windows[0].resets_at(),
            "the reset timestamp must differ across the two calls"
        );
    }

    /// The same script, run twice from two fresh adapters, produces the same sequence
    /// of outcomes: determinism is what makes the failure cases usable as fixtures.
    #[test]
    fn identical_scripts_produce_identical_output() {
        let script = || {
            vec![
                ScriptedResponse::Success(SyntheticReading::new(vec![window(
                    "primary",
                    WindowScope::AccountWide,
                    10_000,
                    1_000,
                )])),
                ScriptedResponse::Timeout,
                ScriptedResponse::Unauthorized(AuthReason::CredentialRejected),
            ]
        };
        let a = SyntheticAdapter::new(declarations(), script());
        let b = SyntheticAdapter::new(declarations(), script());
        for _ in 0..3 {
            assert_eq!(observe(&a), observe(&b));
        }
    }

    /// A script run past its last entry is a test-authoring error, not a silent
    /// wraparound or a fabricated success.
    #[test]
    #[should_panic(expected = "script exhausted")]
    fn calling_past_the_script_panics() {
        let adapter = SyntheticAdapter::new(declarations(), vec![ScriptedResponse::Timeout]);
        observe(&adapter);
        observe(&adapter);
    }
}
