//! Provenance manifests and derivation identifiers.
//!
//! Every derived quantity carries a [`ProvenanceManifest`] so the question
//! "why does `aub` believe this number?" is answerable without reading Rust.
//! The manifest holds a content-addressed hash of its input evidence, the
//! input count, the witness identifiers, and the query semantics. The hash is
//! what makes the full expansion verifiable rather than merely plausible: the
//! expanded member set must be exactly the set whose canonical hash produced
//! the manifest.

use std::collections::BTreeSet;

/// A content hash, 64-bit FNV-1a.
///
/// Deterministic and order-independent by construction: the hash is computed
/// over a canonical (sorted) serialization, never over insertion order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(u64);

impl Digest {
    /// The FNV-1a 64-bit hash of `bytes`.
    fn of_bytes(bytes: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Digest(hash)
    }

    /// The raw 64-bit value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// A typed evidence identifier.
///
/// The manifest is constructible only from this type, never from a bare
/// string, so an identifier from one source cannot be silently passed where an
/// identifier from another is expected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvidenceId(String);

impl EvidenceId {
    pub fn new(value: impl Into<String>) -> Self {
        EvidenceId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A cost-model identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CostModelId(String);

impl CostModelId {
    pub fn new(value: impl Into<String>) -> Self {
        CostModelId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A window-calibration identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowCalibrationId(String);

impl WindowCalibrationId {
    pub fn new(value: impl Into<String>) -> Self {
        WindowCalibrationId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A rate-card version identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RateCardId(String);

impl RateCardId {
    pub fn new(value: impl Into<String>) -> Self {
        RateCardId(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A witness identifier: one of the three witness types a derivation can name.
///
/// The three variants are distinct types, so a cost-model ID can never be
/// passed where a rate-card version is expected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WitnessId {
    CostModel(CostModelId),
    WindowCalibration(WindowCalibrationId),
    RateCard(RateCardId),
}

/// The query semantics a derivation was computed under.
///
/// Two reports over the same evidence with different grouping or filtering are
/// different derivations even when the input set is identical, so the
/// semantics are part of the manifest and of the derivation identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuerySemantics {
    grouping: String,
    filtering: String,
}

impl QuerySemantics {
    pub fn new(grouping: impl Into<String>, filtering: impl Into<String>) -> Self {
        QuerySemantics {
            grouping: grouping.into(),
            filtering: filtering.into(),
        }
    }

    /// The canonical serialization, used to hash the semantics.
    fn as_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(self.grouping.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.filtering.as_bytes());
        bytes
    }
}

/// A provenance manifest: the content-addressed statement of what a derived
/// quantity was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceManifest {
    inputs_hash: Digest,
    input_count: usize,
    witnesses: BTreeSet<WitnessId>,
    query_semantics: QuerySemantics,
}

impl ProvenanceManifest {
    /// A manifest from typed evidence identifiers, witness identifiers, and
    /// query semantics.
    ///
    /// The inputs are collected into a sorted set before hashing, so the hash
    /// is canonical: the same evidence in any order produces the same hash.
    pub fn new(
        inputs: impl IntoIterator<Item = EvidenceId>,
        witnesses: impl IntoIterator<Item = WitnessId>,
        query_semantics: QuerySemantics,
    ) -> Self {
        let inputs: BTreeSet<EvidenceId> = inputs.into_iter().collect();
        let input_count = inputs.len();
        let inputs_hash = canonical_inputs_hash(&inputs);
        let witnesses: BTreeSet<WitnessId> = witnesses.into_iter().collect();
        ProvenanceManifest {
            inputs_hash,
            input_count,
            witnesses,
            query_semantics,
        }
    }

    /// The content-addressed hash of the input evidence set.
    pub fn inputs_hash(&self) -> Digest {
        self.inputs_hash
    }

    /// The number of input evidence identifiers.
    pub fn input_count(&self) -> usize {
        self.input_count
    }

    /// The witness identifiers.
    pub fn witnesses(&self) -> &BTreeSet<WitnessId> {
        &self.witnesses
    }

    /// The query semantics.
    pub fn query_semantics(&self) -> &QuerySemantics {
        &self.query_semantics
    }

    /// Verifies that `members` is exactly the set whose canonical hash
    /// produced this manifest.
    ///
    /// A corrupted member changes the hash and is detected here, so an
    /// expansion can never be merely plausible.
    pub fn verify_expansion(&self, members: &BTreeSet<EvidenceId>) -> bool {
        members.len() == self.input_count && canonical_inputs_hash(members) == self.inputs_hash
    }
}

/// The canonical hash of an evidence set, independent of insertion order.
pub fn canonical_inputs_hash(inputs: &BTreeSet<EvidenceId>) -> Digest {
    let mut bytes = Vec::new();
    for id in inputs {
        bytes.extend_from_slice(id.as_str().as_bytes());
        bytes.push(0);
    }
    Digest::of_bytes(&bytes)
}

/// A derivation identifier, stable across runs for the same inputs and
/// semantics.
///
/// Derived from the manifest's inputs hash and query semantics, so it changes
/// when either changes and never depends on wall-clock time or process state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DerivationId(Digest);

impl DerivationId {
    /// The derivation identifier for a manifest.
    pub fn from_manifest(manifest: &ProvenanceManifest) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&manifest.inputs_hash().as_u64().to_le_bytes());
        bytes.extend_from_slice(&manifest.query_semantics().as_bytes());
        DerivationId(Digest::of_bytes(&bytes))
    }
}

/// A derived quantity: a value plus the provenance manifest that says where it
/// came from.
///
/// The only constructor requires a manifest, so a derived quantity with no
/// provenance cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived<T> {
    value: T,
    manifest: ProvenanceManifest,
}

impl<T> Derived<T> {
    pub fn new(value: T, manifest: ProvenanceManifest) -> Self {
        Derived { value, manifest }
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn manifest(&self) -> &ProvenanceManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(ids: &[&str]) -> Vec<EvidenceId> {
        ids.iter().map(|s| EvidenceId::new(*s)).collect()
    }

    fn semantics() -> QuerySemantics {
        QuerySemantics::new("by-account", "last-7-days")
    }

    /// The inputs hash is canonical: the same evidence set in any order
    /// produces the same hash.
    #[test]
    fn inputs_hash_is_canonical_across_orderings() {
        let forward = ProvenanceManifest::new(
            evidence(&["a", "b", "c"]),
            [],
            semantics(),
        );
        let reversed = ProvenanceManifest::new(
            evidence(&["c", "b", "a"]),
            [],
            semantics(),
        );
        assert_eq!(forward.inputs_hash(), reversed.inputs_hash());
        assert_eq!(forward.input_count(), reversed.input_count());
    }

    /// A derivation identifier is stable across runs for the same inputs and
    /// semantics, and changes when either changes.
    #[test]
    fn derivation_id_is_stable_and_sensitive() {
        let base = ProvenanceManifest::new(evidence(&["a", "b"]), [], semantics());
        let same = ProvenanceManifest::new(evidence(&["b", "a"]), [], semantics());
        assert_eq!(
            DerivationId::from_manifest(&base),
            DerivationId::from_manifest(&same),
            "same inputs and semantics must produce the same derivation id"
        );

        let different_inputs = ProvenanceManifest::new(evidence(&["a", "c"]), [], semantics());
        assert_ne!(
            DerivationId::from_manifest(&base),
            DerivationId::from_manifest(&different_inputs),
            "different inputs must change the derivation id"
        );

        let different_semantics = ProvenanceManifest::new(
            evidence(&["a", "b"]),
            [],
            QuerySemantics::new("by-account", "last-30-days"),
        );
        assert_ne!(
            DerivationId::from_manifest(&base),
            DerivationId::from_manifest(&different_semantics),
            "different semantics must change the derivation id"
        );
    }

    /// Expanding a manifest yields exactly the set whose canonical hash equals
    /// the manifest's hash.
    #[test]
    fn expansion_matches_the_manifest_hash() {
        let members: BTreeSet<EvidenceId> = evidence(&["a", "b", "c"]).into_iter().collect();
        let manifest = ProvenanceManifest::new(members.clone(), [], semantics());
        assert!(manifest.verify_expansion(&members));
    }

    /// Corrupting one member of a manifest is detected by the hash comparison.
    #[test]
    fn corrupting_one_member_is_detected() {
        let members: BTreeSet<EvidenceId> = evidence(&["a", "b", "c"]).into_iter().collect();
        let manifest = ProvenanceManifest::new(members.clone(), [], semantics());

        let mut corrupted = members.clone();
        corrupted.remove(&EvidenceId::new("b"));
        corrupted.insert(EvidenceId::new("x"));
        assert!(
            !manifest.verify_expansion(&corrupted),
            "a corrupted member set must not verify against the manifest hash"
        );
    }

    /// The three witness identifier types are distinct from one another.
    #[test]
    fn witness_identifier_types_are_distinct() {
        let cost = WitnessId::CostModel(CostModelId::new("cm-1"));
        let calib = WitnessId::WindowCalibration(WindowCalibrationId::new("wc-1"));
        let rate = WitnessId::RateCard(RateCardId::new("rc-1"));
        assert_ne!(cost, calib);
        assert_ne!(calib, rate);
        assert_ne!(rate, cost);
    }

    /// A derived quantity carries its manifest and value.
    #[test]
    fn derived_quantity_carries_its_manifest() {
        let manifest = ProvenanceManifest::new(evidence(&["a"]), [], semantics());
        let derived = Derived::new(42u64, manifest.clone());
        assert_eq!(*derived.value(), 42);
        assert_eq!(derived.manifest(), &manifest);
    }
}
