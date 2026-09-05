//! The provider adapter trait and its two inward-facing ports.
//!
//! An adapter takes a resolved credential handle, request parameters, an HTTP
//! transport and a clock, and returns a typed provider observation. It does
//! not write files, does not touch SQLite, and does not resolve credential
//! paths for itself. Those three prohibitions keep provider code from growing
//! its own persistence and its own idea of identity; `aub-lveh` adds the
//! mechanical check for the file-write half (boundary rule `17`), and rules
//! `03` and `07` already hold for the other two.
//!
//! The boundary owns its port types rather than importing them, so an adapter
//! never names the modules that acquire credentials or read configuration.
//! The credential module's resolved-material type is converted into a
//! [`CredentialHandle`] by the sampling orchestration, never by the adapter
//! and never inside this module (boundary rule `07` forbids the reference in
//! either direction).
//!
//! May not depend on:
//! - SQLite (rule `03`)
//! - credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`, which confines it to the transport module)
//! - presentation

use crate::domain::failure::{AuthReason, FailureClass};
use crate::domain::ids::{MeterSemanticsId, ProviderContractId};
use crate::domain::time::{Clock, MeasurementBasis};
use crate::domain::window::ModelId;
use crate::meter::evidence::CapturedProviderResponse;
use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse};

/// The authentication material an adapter authenticates a request with.
///
/// Resolved by the credential layer upstream; the adapter interprets it
/// (which header, which scheme) according to the provider contract it
/// implements, and the failure sanitizer (`aub-rif.13`) strips it from any
/// error text before that text enters a classification.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialHandle(CredentialMaterial);

impl CredentialHandle {
    /// Wraps already-resolved authentication material.
    ///
    /// The caller is the sampling orchestration: it owns the conversion from
    /// whatever the credential source resolved to. Adapters never construct
    /// handles themselves and never resolve one from the filesystem.
    pub fn new(material: impl Into<String>) -> Self {
        Self(CredentialMaterial::new(material.into()))
    }

    /// The material for the provider request itself. Reaching for provider
    /// credential *paths* from here is a boundary violation (rule `07`); the
    /// handle is the resolved end of credential resolution, and interpretation
    /// of what the string contains belongs to the provider adapter contract
    /// (`aub-eun.4`).
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CredentialHandle")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// The opaque secret payload of a [`CredentialHandle`], wrapped so deriving
/// `Debug` on the handle can never print what it carries.
#[derive(Clone, PartialEq, Eq)]
struct CredentialMaterial(String);

impl CredentialMaterial {
    fn new(raw: String) -> Self {
        Self(raw)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CredentialMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// The request parameters the sampling orchestration passes in with a
/// credential. Never resolved from configuration inside the adapter (rule
/// `07`): the caller reads configuration, resolves the credential, and does
/// both handoffs here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MeterRequest {
    /// Restrict the observation to one model where the provider can serve a
    /// model-specific query; `None` means every window the contract exposes.
    /// The adapter contract suite (section 34.8) exercises both forms.
    pub model: Option<ModelId>,
}

/// One provider-defined constraint kind an adapter requires in a successful
/// response. Keeping the kind typed prevents a parser from silently treating
/// a generic field name as a window identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequiredWindowKind(String);

impl RequiredWindowKind {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The required provider constraint kinds declared by one adapter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequiredWindowKinds(Vec<RequiredWindowKind>);

impl RequiredWindowKinds {
    pub fn from_values(values: &[&str]) -> Self {
        Self(
            values
                .iter()
                .map(|value| RequiredWindowKind::new(*value))
                .collect(),
        )
    }

    pub fn contains(&self, value: &str) -> bool {
        self.0.iter().any(|kind| kind.as_str() == value)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RequiredWindowKind> {
        self.0.iter()
    }
}

/// What an adapter hands back for one observation attempt.
///
/// The failure arms are data, not errors. Every variant here is an outcome
/// the evidence substrate persists (an unreachable source is recorded rather
/// than omitted, and a failed attempt is never silently dropped), so this is
/// a plain sum and not a `Result`: there is no call that "fails and reports
/// nothing". The mapping into the persisted attempt vocabulary is the
/// sampler's job and deliberately not a `From`: the attempt lifecycle carries
/// its two timings, which the adapter does not own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderObservation<T> {
    /// The provider reported a usable reading, already typed by the adapter.
    Measured(T),
    /// The provider rejected or invalidated the credential. This is the
    /// sticky authentication conclusion, decided by provider-specific logic:
    /// an ambiguous 403 is never classified here, it arrives as an unreachable
    /// observation with an HTTP status class instead (section 34.8).
    AuthRequired(AuthReason),
    /// The source was unreachable or its answer untrustworthy, classified into
    /// the shared failure vocabulary. Every variant here maps to exactly one
    /// existing freshness reason and never adds a fourth user-facing state.
    Unreachable(FailureClass),
}

/// The declarations every adapter must make, readable without performing a
/// provider call.
///
/// These are not incidental metadata: calibration applicability is decided
/// against the two semantic identifiers, and reading freshness is decided
/// against the basis. They are deliberately not derived from software version
/// numbers (section 7.7): an adapter refactor that changes no physical
/// meaning must not invalidate a calibration, and a provider that changes how
/// a window works must invalidate it even when the code still parses.
///
/// A required method returning this struct, rather than associated constants,
/// because the semantic identifier types construct from `impl Into<String>`
/// and their constructors are not `const`; widening `domain/ids.rs` for const
/// construction is outside this bead's blast radius.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterDeclarations {
    /// Which clock the provider contract documents as the measurement time.
    pub measurement_basis: MeasurementBasis,
    /// The provider endpoint schema this adapter parses against.
    pub provider_contract_id: ProviderContractId,
    /// What a reading from this adapter physically means.
    pub meter_semantics_id: MeterSemanticsId,
    /// Provider constraint kinds that must be present for a measured reading.
    pub required_window_kinds: RequiredWindowKinds,
}

impl AdapterDeclarations {
    pub fn new(
        measurement_basis: MeasurementBasis,
        provider_contract_id: ProviderContractId,
        meter_semantics_id: MeterSemanticsId,
    ) -> Self {
        Self {
            measurement_basis,
            provider_contract_id,
            meter_semantics_id,
            required_window_kinds: RequiredWindowKinds::default(),
        }
    }

    pub fn with_required_window_kinds(mut self, kinds: RequiredWindowKinds) -> Self {
        self.required_window_kinds = kinds;
        self
    }
}

/// The inward-facing HTTP port. Adapters issue requests through it and never
/// construct driver clients directly (rule `12` keeps every driver reference
/// inside the transport module, where the real implementation lives).
pub trait HttpTransport {
    /// Executes one request, clipped to the command-wide budget, and returns
    /// the response or the transport-level failure classification. A status
    /// like 429 or 403 arrives as an `Ok` response: interpreting status codes
    /// into the shared vocabulary is the adapter's decision, not the
    /// transport's.
    fn send(
        &self,
        request: &HttpRequest,
        budget: &CommandBudget,
        clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass>;
}

/// Every transport is usable through a shared reference, so one transport can
/// be lent to every scoped-thread worker at once without cloning it.
impl<T: HttpTransport + ?Sized> HttpTransport for &T {
    fn send(
        &self,
        request: &HttpRequest,
        budget: &CommandBudget,
        clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        (**self).send(request, budget, clock)
    }
}

/// The provider adapter contract (sections 9 and 33 Phase 2).
///
/// One method per attempt. The adapter receives what it needs from the
/// boundary, decides how its provider's answer maps into the shared
/// vocabulary, and returns a typed observation. It receives no store, no
/// filesystem, and no configuration, and returns nothing that outlives the
/// attempt it belongs to: persistence is the caller's job.
pub trait ProviderAdapter {
    /// The typed reading this adapter produces on success, already shaped by
    /// the domain vocabulary that table `meter_observation` persists.
    type Reading;

    /// The adapter's declarations, reachable without a provider call, so
    /// calibration-applicability decisions (`aub-c0b.10`) read them from any
    /// registered adapter without waiting on the network.
    fn declarations(&self) -> AdapterDeclarations;

    /// One observation attempt against the provider.
    fn observe(
        &self,
        credential: &CredentialHandle,
        request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> ProviderObservation<Self::Reading>;

    /// One observation attempt with response evidence captured before the
    /// adapter interprets it. Adapters without a response capsule keep the
    /// legacy semantic result; response-capturing adapters override this seam.
    fn observe_with_evidence(
        &self,
        credential: &CredentialHandle,
        request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> CapturedProviderResponse<Self::Reading> {
        CapturedProviderResponse::without_response(
            self.observe(credential, request, transport, clock),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-only proof that the contract is implementable with no store, no
    /// filesystem, and no credential resolution behind it (the bead's fourth
    /// acceptance criterion). The declarations path is exercised because that
    /// is the consumer `aub-c0b.10` has; `observe` compiles against a fake
    /// transport and is never called on a live path by this bead.
    struct NoopAdapter;

    impl ProviderAdapter for NoopAdapter {
        type Reading = ();

        fn declarations(&self) -> AdapterDeclarations {
            AdapterDeclarations::new(
                MeasurementBasis::LocallyReceived,
                ProviderContractId::new("test-endpoint-v1"),
                MeterSemanticsId::new("test-meter-v1"),
            )
        }

        fn observe(
            &self,
            _credential: &CredentialHandle,
            _request: &MeterRequest,
            _transport: &impl HttpTransport,
            _clock: &impl Clock,
        ) -> ProviderObservation<()> {
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        }
    }

    #[allow(dead_code)]
    struct FakeTransport;

    impl HttpTransport for FakeTransport {
        fn send(
            &self,
            _request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    #[test]
    fn noop_adapter_declares_without_a_provider_call() {
        let adapter = NoopAdapter;
        let declarations = adapter.declarations();
        assert_eq!(
            declarations.measurement_basis,
            MeasurementBasis::LocallyReceived
        );
        assert_eq!(
            declarations.provider_contract_id.as_str(),
            "test-endpoint-v1"
        );
        assert_eq!(declarations.meter_semantics_id.as_str(), "test-meter-v1");
    }

    /// The credential handle never advertises its material through Debug.
    #[test]
    fn credential_handle_debug_redacts_material() {
        let handle = CredentialHandle::new("sk-super-secret");
        let rendered = format!("{handle:?}");
        assert!(!rendered.contains("sk-super-secret"));
        assert!(rendered.contains("REDACTED"));
    }
}
