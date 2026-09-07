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
use crate::error::Error;
use crate::meter::agy::{AgyAdapter, AgyReading};
use crate::meter::anthropic::{AnthropicAdapter, AnthropicReading};
use crate::meter::codex::{CodexAdapter, CodexReading};
use crate::meter::evidence::CapturedProviderResponse;
use crate::meter::ollama::{OllamaAdapter, OllamaReading};
use crate::meter::opencode::{OpenCodeAdapter, OpenCodeReading};
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
    /// The provider-side workspace scope the caller resolved from the
    /// account's configuration, for contracts that read a workspace-scoped
    /// page. `None` when the provider has no such scope.
    pub workspace_id: Option<String>,
    /// The provider home directory a file-backed meter reads its evidence
    /// from, resolved by the caller from the account's own configuration
    /// (`aub-cg6k`). This is the meter request's local source: the adapter
    /// turns it into local-file transport requests and never resolves a path
    /// of its own. `None` on every network meter, and no HTTP adapter reads
    /// it, so the HTTP adapters' shape is unchanged.
    pub local_home: Option<std::path::PathBuf>,
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

/// The provider keys [`adapter_for`] can dispatch on, in dispatch order.
///
/// Defined once and read everywhere a supported-provider list is rendered:
/// the unsupported-provider error joins this table, so a hand-maintained
/// copy of the list is the defect this constant exists to prevent.
pub const SUPPORTED_PROVIDERS: &[&str] = &["anthropic", "codex", "ollama", "opencode", "agy"];

/// Endpoint overrides the caller resolved from the environment, handed across
/// the boundary as data.
///
/// The meter never reads the environment itself: the caller reads
/// configuration and resolves credentials the same way, and passes the
/// resolved overrides in, the same handoff shape as [`CredentialHandle`].
/// `None` for a provider keeps that adapter's own default endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointConfig {
    /// Overrides the Anthropic usage endpoint URL (`AUB_ANTHROPIC_ENDPOINT`).
    pub anthropic: Option<String>,
    /// Overrides the OpenCode workspace page URL (`AUB_OPENCODE_ENDPOINT`):
    /// the full page the adapter fetches, so a synthetic server can stand in
    /// for `https://opencode.ai/workspace/<id>/go` in end-to-end runs. The
    /// workspace id itself is account configuration, not an environment
    /// override, and travels in [`MeterRequest::workspace_id`].
    pub opencode: Option<String>,
}

/// The orchestrator-facing reading of whichever adapter the dispatch chose.
///
/// One variant per adapter reading, so the batch pipeline stays generic over
/// [`AnyAdapter`] and never names a concrete adapter either. The
/// persistence-shaped surface ([`crate::meter::sampler::MeteredReading`]) is
/// implemented beside the orchestrator, like every adapter reading's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    Anthropic(AnthropicReading),
    OpenCode(OpenCodeReading),
    Codex(CodexReading),
    Ollama(OllamaReading),
    Agy(AgyReading),
}

/// The adapter choice the dispatch made, delegating [`ProviderAdapter`] to
/// the variant it holds.
///
/// The orchestrator and its batch types are generic over the adapter type,
/// and [`ProviderAdapter`] is not object-safe (an associated [`Reading`] type
/// and a `declarations()` constructor), so the dynamic choice is this closed
/// enum rather than a trait object. Adding an adapter is one arm in
/// [`adapter_for`], one variant here, and the delegation arms beside them.
pub enum AnyAdapter {
    Anthropic(AnthropicAdapter),
    OpenCode(OpenCodeAdapter),
    Codex(CodexAdapter),
    Ollama(OllamaAdapter),
    Agy(AgyAdapter),
}

impl ProviderAdapter for AnyAdapter {
    type Reading = Reading;

    fn declarations(&self) -> AdapterDeclarations {
        match self {
            AnyAdapter::Anthropic(adapter) => adapter.declarations(),
            AnyAdapter::OpenCode(adapter) => adapter.declarations(),
            AnyAdapter::Codex(adapter) => adapter.declarations(),
            AnyAdapter::Ollama(adapter) => adapter.declarations(),
            AnyAdapter::Agy(adapter) => adapter.declarations(),
        }
    }

    fn observe(
        &self,
        credential: &CredentialHandle,
        request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> ProviderObservation<Reading> {
        match self {
            AnyAdapter::Anthropic(adapter) => map_observation(
                adapter.observe(credential, request, transport, clock),
                Reading::Anthropic,
            ),
            AnyAdapter::OpenCode(adapter) => map_observation(
                adapter.observe(credential, request, transport, clock),
                Reading::OpenCode,
            ),
            AnyAdapter::Codex(adapter) => map_observation(
                adapter.observe(credential, request, transport, clock),
                Reading::Codex,
            ),
            AnyAdapter::Ollama(adapter) => map_observation(
                adapter.observe(credential, request, transport, clock),
                Reading::Ollama,
            ),
            AnyAdapter::Agy(adapter) => map_observation(
                adapter.observe(credential, request, transport, clock),
                Reading::Agy,
            ),
        }
    }

    fn observe_with_evidence(
        &self,
        credential: &CredentialHandle,
        request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> CapturedProviderResponse<Reading> {
        match self {
            AnyAdapter::Anthropic(adapter) => {
                let captured = adapter.observe_with_evidence(credential, request, transport, clock);
                CapturedProviderResponse {
                    observation: map_observation(captured.observation, Reading::Anthropic),
                    evidence: captured.evidence,
                    failed_body: captured.failed_body,
                }
            }
            AnyAdapter::OpenCode(adapter) => {
                let captured = adapter.observe_with_evidence(credential, request, transport, clock);
                CapturedProviderResponse {
                    observation: map_observation(captured.observation, Reading::OpenCode),
                    evidence: captured.evidence,
                    failed_body: captured.failed_body,
                }
            }
            AnyAdapter::Codex(adapter) => {
                let captured = adapter.observe_with_evidence(credential, request, transport, clock);
                CapturedProviderResponse {
                    observation: map_observation(captured.observation, Reading::Codex),
                    evidence: captured.evidence,
                    failed_body: captured.failed_body,
                }
            }
            AnyAdapter::Ollama(adapter) => {
                let captured = adapter.observe_with_evidence(credential, request, transport, clock);
                CapturedProviderResponse {
                    observation: map_observation(captured.observation, Reading::Ollama),
                    evidence: captured.evidence,
                    failed_body: captured.failed_body,
                }
            }
            AnyAdapter::Agy(adapter) => {
                let captured = adapter.observe_with_evidence(credential, request, transport, clock);
                CapturedProviderResponse {
                    observation: map_observation(captured.observation, Reading::Agy),
                    evidence: captured.evidence,
                    failed_body: captured.failed_body,
                }
            }
        }
    }
}

/// Relabels one observation's measured value while its failure arms pass
/// through unchanged: the dispatch exists at the adapter choice, not inside
/// the observation's arms.
fn map_observation<T, U>(
    observation: ProviderObservation<T>,
    relabel: impl FnOnce(T) -> U,
) -> ProviderObservation<U> {
    match observation {
        ProviderObservation::Measured(reading) => ProviderObservation::Measured(relabel(reading)),
        ProviderObservation::AuthRequired(reason) => ProviderObservation::AuthRequired(reason),
        ProviderObservation::Unreachable(class) => ProviderObservation::Unreachable(class),
    }
}

/// The one place a provider string becomes an adapter.
///
/// The commands call this instead of comparing provider strings themselves,
/// so the choice lives with the adapters it chooses between. The account name
/// travels in only for the unsupported-provider error's message: this module
/// cannot name the configuration types (rule `07`), so the account crosses as
/// a string the same way the resolved credential does.
pub fn adapter_for(
    provider: &str,
    account: &str,
    endpoint_overrides: &EndpointConfig,
) -> Result<AnyAdapter, Error> {
    match provider {
        "anthropic" => {
            let endpoint = endpoint_overrides
                .anthropic
                .as_deref()
                .unwrap_or(AnthropicAdapter::DEFAULT_ENDPOINT);
            Ok(AnyAdapter::Anthropic(AnthropicAdapter::with_endpoint(
                endpoint,
            )))
        }
        "opencode" => Ok(AnyAdapter::OpenCode(OpenCodeAdapter::new(
            endpoint_overrides.opencode.clone(),
        ))),
        "codex" => Ok(AnyAdapter::Codex(CodexAdapter::new())),
        "ollama" => Ok(AnyAdapter::Ollama(OllamaAdapter::new())),
        "agy" => Ok(AnyAdapter::Agy(AgyAdapter::new())),
        unsupported => Err(unsupported_provider_error(unsupported, account)),
    }
}

/// The unsupported-provider error, with the supported list joined from
/// [`SUPPORTED_PROVIDERS`] so the message and the dispatch arms cannot drift
/// apart.
fn unsupported_provider_error(provider: &str, account: &str) -> Error {
    Error::Usage(format!(
        "unsupported provider '{}' for account '{}' (supported: {})",
        provider,
        account,
        SUPPORTED_PROVIDERS.join(", "),
    ))
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

    #[test]
    fn adapter_for_dispatches_the_anthropic_arm_to_its_default_endpoint() {
        let adapter = adapter_for("anthropic", "work-primary", &EndpointConfig::default())
            .expect("anthropic is in the supported table");
        match adapter {
            AnyAdapter::Anthropic(anthropic) => {
                assert_eq!(anthropic.endpoint_url(), AnthropicAdapter::DEFAULT_ENDPOINT);
            }
            AnyAdapter::Codex(_)
            | AnyAdapter::Ollama(_)
            | AnyAdapter::OpenCode(_)
            | AnyAdapter::Agy(_) => {
                panic!("dispatch chose another arm for anthropic")
            }
        }
        assert!(SUPPORTED_PROVIDERS.contains(&"anthropic"));
    }

    /// The endpoint override the caller resolved crosses into the chosen
    /// adapter; the default holds only when the override is absent.
    #[test]
    fn adapter_for_honours_the_resolved_endpoint_override() {
        let overrides = EndpointConfig {
            anthropic: Some("http://127.0.0.1:9".to_string()),
            opencode: None,
        };
        let adapter = adapter_for("anthropic", "work-primary", &overrides)
            .expect("anthropic is in the supported table");
        match adapter {
            AnyAdapter::Anthropic(anthropic) => {
                assert_eq!(anthropic.endpoint_url(), "http://127.0.0.1:9");
            }
            AnyAdapter::Codex(_)
            | AnyAdapter::Ollama(_)
            | AnyAdapter::OpenCode(_)
            | AnyAdapter::Agy(_) => {
                panic!("dispatch chose another arm for anthropic")
            }
        }
    }

    /// The opencode arm dispatches to its adapter, carrying the endpoint
    /// override the caller resolved and none when absent.
    #[test]
    fn adapter_for_dispatches_the_opencode_arm() {
        let adapter = adapter_for("opencode", "go-primary", &EndpointConfig::default())
            .expect("opencode is in the supported table");
        match adapter {
            AnyAdapter::OpenCode(opencode) => {
                assert!(opencode.endpoint_override().is_none());
            }
            AnyAdapter::Anthropic(_)
            | AnyAdapter::Codex(_)
            | AnyAdapter::Ollama(_)
            | AnyAdapter::Agy(_) => {
                panic!("the opencode dispatch cannot yield another arm")
            }
        }
        let overrides = EndpointConfig {
            anthropic: None,
            opencode: Some("http://127.0.0.1:9/workspace/wrk_x/go".to_string()),
        };
        let adapter = adapter_for("opencode", "go-primary", &overrides)
            .expect("opencode is in the supported table");
        match adapter {
            AnyAdapter::OpenCode(opencode) => {
                assert_eq!(
                    opencode.endpoint_override(),
                    Some("http://127.0.0.1:9/workspace/wrk_x/go")
                );
            }
            AnyAdapter::Anthropic(_)
            | AnyAdapter::Codex(_)
            | AnyAdapter::Ollama(_)
            | AnyAdapter::Agy(_) => {
                panic!("the opencode dispatch cannot yield another arm")
            }
        }
        assert!(SUPPORTED_PROVIDERS.contains(&"opencode"));
    }

    /// The codex arm dispatches to the rollout adapter and its declarations
    /// name the contract the bead fixed, reachable without a provider call.
    #[test]
    fn adapter_for_dispatches_the_codex_arm_to_the_rollout_adapter() {
        let adapter = adapter_for("codex", "codex-primary", &EndpointConfig::default())
            .expect("codex is in the supported table");
        match adapter {
            AnyAdapter::Codex(codex) => {
                let declarations = codex.declarations();
                assert_eq!(
                    declarations.provider_contract_id.as_str(),
                    "openai-codex-rollout-rate-limits-v1"
                );
                assert_eq!(
                    declarations.meter_semantics_id.as_str(),
                    "openai-chatgpt-subscription-v1"
                );
                assert!(declarations.required_window_kinds.contains("primary"));
                assert!(declarations.required_window_kinds.contains("secondary"));
            }
            AnyAdapter::Anthropic(_)
            | AnyAdapter::Ollama(_)
            | AnyAdapter::OpenCode(_)
            | AnyAdapter::Agy(_) => {
                panic!("dispatch chose another arm for codex")
            }
        }
        assert!(SUPPORTED_PROVIDERS.contains(&"codex"));
    }

    /// `aub-ud17`: the ollama arm dispatches to its default endpoint, the
    /// same shape the anthropic dispatch test above proves.
    #[test]
    fn adapter_for_dispatches_the_ollama_arm_to_its_default_endpoint() {
        let adapter = adapter_for("ollama", "work-k1", &EndpointConfig::default())
            .expect("ollama is in the supported table");
        match adapter {
            AnyAdapter::Ollama(ollama) => {
                assert_eq!(
                    ollama.endpoint_url(),
                    crate::meter::ollama::OllamaAdapter::DEFAULT_ENDPOINT
                );
            }
            AnyAdapter::Anthropic(_)
            | AnyAdapter::Codex(_)
            | AnyAdapter::OpenCode(_)
            | AnyAdapter::Agy(_) => {
                panic!("expected the ollama arm")
            }
        }
        assert!(SUPPORTED_PROVIDERS.contains(&"ollama"));
    }

    /// `aub-n8yx`: the agy arm dispatches to the Antigravity adapter, the
    /// same shape the ollama dispatch test above proves.
    #[test]
    fn adapter_for_dispatches_the_agy_arm_to_the_quota_summary_adapter() {
        let adapter = adapter_for("agy", "agy-primary", &EndpointConfig::default())
            .expect("agy is in the supported table");
        match adapter {
            AnyAdapter::Agy(agy) => {
                let declarations = agy.declarations();
                assert_eq!(
                    declarations.provider_contract_id.as_str(),
                    crate::meter::agy::AgyAdapter::CONTRACT_ID
                );
                assert_eq!(
                    declarations.meter_semantics_id.as_str(),
                    crate::meter::agy::AgyAdapter::SEMANTICS_ID
                );
            }
            AnyAdapter::Anthropic(_)
            | AnyAdapter::Codex(_)
            | AnyAdapter::Ollama(_)
            | AnyAdapter::OpenCode(_) => {
                panic!("expected the agy arm")
            }
        }
        assert!(SUPPORTED_PROVIDERS.contains(&"agy"));
    }

    /// The negative: an unsupported provider yields the usage error that
    /// names the account and the list joined from the constant, never a
    /// hand-written provider list.
    #[test]
    fn adapter_for_unsupported_names_account_and_joined_supported_table() {
        let error = match adapter_for("nope", "work-primary", &EndpointConfig::default()) {
            Ok(_) => panic!("nope is not in the supported table"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "unsupported provider 'nope' for account 'work-primary' (supported: anthropic, codex, ollama, opencode, agy)"
        );
        assert!(matches!(error, Error::Usage(_)));
    }

    /// AnyAdapter stands where the orchestrator's generic parameter stands,
    /// so it must satisfy exactly the bounds `run<A>` states.
    #[test]
    fn any_adapter_satisfies_the_bounds_the_orchestrator_requires() {
        fn assert_orchestrator_bounds<A>()
        where
            A: ProviderAdapter + Sync,
            A::Reading: crate::meter::sampler::MeteredReading + Send,
        {
        }
        assert_orchestrator_bounds::<AnyAdapter>();
    }
}
