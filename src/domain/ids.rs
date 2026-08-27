//! Namespaced identifiers and the semantic identifiers.
//!
//! Two collisions are prevented here. Textual: a native session ID from one CLI
//! must never collide with an identical string from another, so every
//! externally-originated identifier carries a [`SourceNamespace`]. Conceptual:
//! the three semantic identifiers ([`MeterSemanticsId`], [`BillingSemanticsId`],
//! [`ProviderContractId`]) are distinct from every software version number, so
//! calibration applicability is decided against physical and billing semantics
//! rather than against release numbers.

/// The source a native identifier came from (e.g., "claude-code", "codex", "pi").
///
/// Opaque: the inner value is private, so a namespace can only be built through
/// [`SourceNamespace::new`] and read through [`SourceNamespace::as_str`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceNamespace(String);

impl SourceNamespace {
    pub fn new(value: impl Into<String>) -> Self {
        SourceNamespace(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A session identifier exactly as the source tool produced it, before namespacing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeSessionId(String);

impl NativeSessionId {
    pub fn new(value: impl Into<String>) -> Self {
        NativeSessionId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A task identifier exactly as the source tool produced it, before namespacing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeTaskId(String);

impl NativeTaskId {
    pub fn new(value: impl Into<String>) -> Self {
        NativeTaskId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A run identifier exactly as the source tool produced it, before namespacing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeRunId(String);

impl NativeRunId {
    pub fn new(value: impl Into<String>) -> Self {
        NativeRunId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A session identifier namespaced by its source.
///
/// Two identical native values from different sources are unequal, so the
/// session join never merges unrelated sessions from two tools that happen to
/// pick the same ID format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId {
    source: SourceNamespace,
    native: NativeSessionId,
}

impl SessionId {
    pub fn new(source: SourceNamespace, native: NativeSessionId) -> Self {
        SessionId { source, native }
    }

    pub fn source(&self) -> &SourceNamespace {
        &self.source
    }

    pub fn native(&self) -> &NativeSessionId {
        &self.native
    }
}

/// A task identifier namespaced by its source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId {
    source: SourceNamespace,
    native: NativeTaskId,
}

impl TaskId {
    pub fn new(source: SourceNamespace, native: NativeTaskId) -> Self {
        TaskId { source, native }
    }

    pub fn source(&self) -> &SourceNamespace {
        &self.source
    }

    pub fn native(&self) -> &NativeTaskId {
        &self.native
    }
}

/// A run identifier namespaced by its source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId {
    source: SourceNamespace,
    native: NativeRunId,
}

impl RunId {
    pub fn new(source: SourceNamespace, native: NativeRunId) -> Self {
        RunId { source, native }
    }

    pub fn source(&self) -> &SourceNamespace {
        &self.source
    }

    pub fn native(&self) -> &NativeRunId {
        &self.native
    }
}

/// Physical quota semantics: what a meter reading means physically (which
/// window, which account tier). Distinct from the adapter implementation
/// version and from the billing semantics, so an adapter refactor that changes
/// no physical meaning does not invalidate a calibration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeterSemanticsId(String);

impl MeterSemanticsId {
    pub fn new(value: impl Into<String>) -> Self {
        MeterSemanticsId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Billing semantics: how usage is priced against a subscription. Distinct from
/// the meter semantics, so a provider that changes how a window works
/// invalidates a calibration even when the Rust code parses the same JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BillingSemanticsId(String);

impl BillingSemanticsId {
    pub fn new(value: impl Into<String>) -> Self {
        BillingSemanticsId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The provider endpoint contract (schema) a reading was parsed against.
/// Distinct from the meter and billing semantics and from the adapter version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderContractId(String);

impl ProviderContractId {
    pub fn new(value: impl Into<String>) -> Self {
        ProviderContractId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The adapter implementation version, a software release number.
///
/// Deliberately a separate type from the three semantic identifiers: nothing
/// derives a semantic identifier from this, and an adapter upgrade alone never
/// invalidates a calibration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdapterVersion(String);

impl AdapterVersion {
    pub fn new(value: impl Into<String>) -> Self {
        AdapterVersion(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An identifier for the credential context under which an attempt was made.
///
/// Distinct from credential secrets: carries no secret bytes. Used to determine
/// if two attempts ran under the same or different credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialContextId(String);

impl CredentialContextId {
    pub fn new(value: impl Into<String>) -> Self {
        CredentialContextId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_native_values_in_different_namespaces_are_unequal() {
        let a = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("abc123"),
        );
        let b = SessionId::new(
            SourceNamespace::new("codex"),
            NativeSessionId::new("abc123"),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn identical_native_values_in_same_namespace_are_equal() {
        let a = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("abc123"),
        );
        let b = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("abc123"),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn task_and_run_ids_are_namespaced_too() {
        let task_a = TaskId::new(SourceNamespace::new("claude-code"), NativeTaskId::new("t1"));
        let task_b = TaskId::new(SourceNamespace::new("codex"), NativeTaskId::new("t1"));
        assert_ne!(task_a, task_b);

        let run_a = RunId::new(SourceNamespace::new("claude-code"), NativeRunId::new("r1"));
        let run_b = RunId::new(SourceNamespace::new("codex"), NativeRunId::new("r1"));
        assert_ne!(run_a, run_b);
    }

    #[test]
    fn semantic_identifiers_round_trip_their_value() {
        let meter = MeterSemanticsId::new("account-5h-v2");
        let billing = BillingSemanticsId::new("model-x-subscription-v4");
        let contract = ProviderContractId::new("endpoint-schema-v3");
        let adapter = AdapterVersion::new("v14");

        assert_eq!(meter.as_str(), "account-5h-v2");
        assert_eq!(billing.as_str(), "model-x-subscription-v4");
        assert_eq!(contract.as_str(), "endpoint-schema-v3");
        assert_eq!(adapter.as_str(), "v14");
    }

    #[test]
    fn credential_context_id_round_trips_its_value() {
        let ctx = CredentialContextId::new("anthropic-oauth-user-1");
        assert_eq!(ctx.as_str(), "anthropic-oauth-user-1");
    }
}
