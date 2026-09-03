//! The provenance graph of the report models.
//!
//! Every quantitative field of every report model resolves to a
//! [`ProvenanceNode`] through a typed [`ReportField`] identifier, never
//! through a string or an index. The graph is owned by the report model and
//! populated at report construction, so a renderer explains any printed
//! quantity by reading the model alone and computes nothing on the ordinary
//! path.
//!
//! A node carries the manifest the provenance types produced, the canonical
//! member set that manifest hashes over, the source and observation counts
//! behind the value, and the arithmetic or conversion sequence that produced
//! it. The hash and expansion laws are inherited from `aub-rif.11`'s
//! [`ProvenanceManifest`] rather than restated here: [`ProvenanceNode::verify`]
//! delegates to the manifest's own expansion check.
//!
//! What counts as quantitative is a stated decision: a physical quantity a
//! renderer could be asked to explain. Meter readings, token counts, coverage,
//! row counts and calibration derivations qualify. Timestamps and ledger
//! generations are metadata about the report, not measured quantities, and
//! carry no node.
//!
//! How a quantitative field added without a provenance node is rejected is
//! also a stated decision: an enumeration test, not a compile-fail. The test
//! matches exhaustively over [`ReportField`] (so a new variant fails to
//! compile until the test is touched) and then resolves every variant against
//! canonical reports (so a variant the constructors do not populate fails the
//! run). A macro that generated the enum and the graph together would couple
//! the model shape to the graph shape; the test keeps the two decoupled and
//! still fails loudly.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::provenance::{EvidenceId, ProvenanceManifest, QuerySemantics, WitnessId};
use crate::logging::LogicalName;

/// A typed identifier for one quantitative field of one report model.
///
/// The parameterized variants name the account or group key they address, so
/// two readings of one report are two distinct fields and a renderer explains
/// each printed quantity by resolving exactly the field it is about to print.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReportField {
    /// The quota remaining in one account's meter reading.
    MeterQuotaRemaining { account: LogicalName },
    /// The token count of one spend group.
    SpendGroupTokens { key: LogicalName },
    /// The coverage completeness of a coverage report.
    CoverageCompleteness,
    /// The row count of an export report.
    ExportRows,
    /// The derived token count of a calibration report.
    CalibrationTokens,
}

impl ReportField {
    /// The canonical field label.
    pub fn label(&self) -> String {
        match self {
            ReportField::MeterQuotaRemaining { account } => {
                format!("meter_quota_remaining[{}]", account.as_str())
            }
            ReportField::SpendGroupTokens { key } => {
                format!("spend_group_tokens[{}]", key.as_str())
            }
            ReportField::CoverageCompleteness => "coverage_completeness".to_string(),
            ReportField::ExportRows => "export_rows".to_string(),
            ReportField::CalibrationTokens => "calibration_tokens".to_string(),
        }
    }

    /// The associated account or attribution key for this field.
    pub fn account_attribution(&self) -> &str {
        match self {
            ReportField::MeterQuotaRemaining { account } => account.as_str(),
            ReportField::SpendGroupTokens { key } => key.as_str(),
            ReportField::CoverageCompleteness => "all",
            ReportField::ExportRows => "export",
            ReportField::CalibrationTokens => "calibration",
        }
    }
}

/// The arithmetic or conversion sequence that produced a value.
///
/// `--explain` renders this so the reader sees whether a number was read,
/// summed, counted or converted, without recomputing any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueArithmetic {
    /// The value was read directly from a source; no arithmetic.
    Direct,
    /// The value is the sum of its member values.
    Sum,
    /// The value is a count of its members.
    Count,
    /// The value is a conversion from one unit to another.
    Converted { from: Unit, to: Unit },
}

impl ValueArithmetic {
    /// The canonical label for the arithmetic operation.
    pub fn label(&self) -> String {
        match self {
            ValueArithmetic::Direct => "direct".to_string(),
            ValueArithmetic::Sum => "sum".to_string(),
            ValueArithmetic::Count => "count".to_string(),
            ValueArithmetic::Converted { from, to } => {
                format!("converted from {} to {}", from.label(), to.label())
            }
        }
    }
}

/// A unit a value can be expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Tokens,
    Cost,
    Credits,
    QuotaFraction,
}

impl Unit {
    /// The unit identifier string.
    pub fn label(self) -> &'static str {
        match self {
            Unit::Tokens => "tokens",
            Unit::Cost => "cost",
            Unit::Credits => "credits",
            Unit::QuotaFraction => "quota_fraction",
        }
    }
}

/// A provenance node: the manifest, the canonical member set the manifest
/// hashes over, the source and observation counts behind the value, and the
/// arithmetic or conversion sequence that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceNode {
    manifest: ProvenanceManifest,
    members: BTreeSet<EvidenceId>,
    source_count: u64,
    observation_count: u64,
    arithmetic: ValueArithmetic,
}

impl ProvenanceNode {
    /// A node from the member set, witnesses, query semantics and counts.
    ///
    /// The manifest is built by the provenance types from the same members, so
    /// the hash and expansion laws are inherited from `aub-rif.11` rather than
    /// restated here; [`Self::verify`] delegates to the manifest.
    pub fn new(
        members: impl IntoIterator<Item = EvidenceId>,
        witnesses: impl IntoIterator<Item = WitnessId>,
        query_semantics: QuerySemantics,
        source_count: u64,
        observation_count: u64,
        arithmetic: ValueArithmetic,
    ) -> Self {
        let members: BTreeSet<EvidenceId> = members.into_iter().collect();
        let manifest = ProvenanceManifest::new(members.clone(), witnesses, query_semantics);
        Self {
            manifest,
            members,
            source_count,
            observation_count,
            arithmetic,
        }
    }

    /// The manifest the provenance types produced for this node.
    pub fn manifest(&self) -> &ProvenanceManifest {
        &self.manifest
    }

    /// The canonical member set the manifest hashes over.
    pub fn members(&self) -> &BTreeSet<EvidenceId> {
        &self.members
    }

    /// The number of sources behind the value.
    pub fn source_count(&self) -> u64 {
        self.source_count
    }

    /// The number of observations behind the value.
    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// The arithmetic or conversion sequence that produced the value.
    pub fn arithmetic(&self) -> &ValueArithmetic {
        &self.arithmetic
    }

    /// The expansion law, delegated to the manifest: the members are exactly
    /// the set whose canonical hash produced the manifest.
    pub fn verify(&self) -> bool {
        self.manifest.verify_expansion(&self.members)
    }
}

/// The provenance graph of one report model.
///
/// Every quantitative field the model contains is addressed by a typed
/// [`ReportField`] and resolves to a [`ProvenanceNode`]. The graph is assembled
/// by the model constructors from typed per-field material, so a renderer
/// never computes any part of it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProvenanceGraph {
    nodes: BTreeMap<ReportField, ProvenanceNode>,
}

impl ProvenanceGraph {
    /// A graph from typed field-to-node pairs.
    pub fn new(nodes: impl IntoIterator<Item = (ReportField, ProvenanceNode)>) -> Self {
        Self {
            nodes: nodes.into_iter().collect(),
        }
    }

    /// The node for a field, if the report carries one.
    pub fn resolve(&self, field: &ReportField) -> Option<&ProvenanceNode> {
        self.nodes.get(field)
    }

    /// The number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph carries no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The field-to-node pairs, in field order.
    pub fn iter(&self) -> impl Iterator<Item = (&ReportField, &ProvenanceNode)> {
        self.nodes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provenance::canonical_inputs_hash;

    fn evidence(ids: &[&str]) -> Vec<EvidenceId> {
        ids.iter().map(|s| EvidenceId::new(*s)).collect()
    }

    fn semantics() -> QuerySemantics {
        QuerySemantics::new("by-account", "all")
    }

    /// A node's manifest expands to exactly its canonical member set, and
    /// rehashing those members reproduces the manifest hash.
    #[test]
    fn node_manifest_expands_to_its_member_set() {
        let members: BTreeSet<EvidenceId> = evidence(&["a", "b", "c"]).into_iter().collect();
        let node = ProvenanceNode::new(
            members.clone(),
            [] as [WitnessId; 0],
            semantics(),
            2,
            5,
            ValueArithmetic::Sum,
        );

        assert!(node.verify(), "members must expand to the manifest");
        assert_eq!(node.members(), &members);
        assert_eq!(node.manifest().input_count(), 3);
        assert_eq!(
            node.manifest().inputs_hash(),
            canonical_inputs_hash(&members),
            "rehashing the members must reproduce the manifest hash"
        );
        assert_eq!(node.source_count(), 2);
        assert_eq!(node.observation_count(), 5);
        assert_eq!(node.arithmetic(), &ValueArithmetic::Sum);
    }

    /// A node whose members were corrupted after construction is detected by
    /// the manifest's own expansion law, not by a restated one.
    #[test]
    fn corrupting_a_member_is_detected_by_the_manifest_law() {
        let members: BTreeSet<EvidenceId> = evidence(&["a", "b"]).into_iter().collect();
        let node = ProvenanceNode::new(
            members.clone(),
            [] as [WitnessId; 0],
            semantics(),
            1,
            1,
            ValueArithmetic::Direct,
        );
        assert!(node.verify());

        let mut corrupted = members.clone();
        corrupted.remove(&EvidenceId::new("a"));
        corrupted.insert(EvidenceId::new("x"));
        assert!(
            !node.manifest().verify_expansion(&corrupted),
            "a corrupted member set must not verify against the manifest hash"
        );
    }

    /// The graph resolves exactly the fields it was given, and nothing else.
    #[test]
    fn graph_resolves_only_its_own_fields() {
        let node = ProvenanceNode::new(
            evidence(&["a"]),
            [] as [WitnessId; 0],
            semantics(),
            1,
            1,
            ValueArithmetic::Direct,
        );
        let graph = ProvenanceGraph::new([(ReportField::CoverageCompleteness, node)]);

        assert_eq!(graph.len(), 1);
        assert!(graph.resolve(&ReportField::CoverageCompleteness).is_some());
        assert!(
            graph.resolve(&ReportField::ExportRows).is_none(),
            "a field the graph was not given must not resolve"
        );
        assert!(!graph.is_empty());
    }

    /// An empty graph is the honest shape for a report with no quantitative
    /// fields.
    #[test]
    fn empty_graph_is_default() {
        let graph = ProvenanceGraph::default();
        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
    }
}
