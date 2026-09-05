//! Typed report models for every command.
//!
//! The report model is the seam that keeps the renderer honest: every quantity
//! field is a qualified value or a [`Derivation`], never a bare newtype, and a
//! meter reading carries exactly one freshness variant. A renderer cannot produce
//! an unqualified number because it never holds one.

use std::collections::{BTreeMap, BTreeSet};

use crate::attribution::account_segment::AccountEvidenceClass;
use crate::config::CoverageFloor;
use crate::coverage::CoverageFraction;
use crate::domain::attempt::AttemptOutcome;
use crate::domain::credits::Credits;
use crate::domain::freshness::Freshness;
use crate::domain::ids::{NativeRunId, ProviderContractId};
use crate::domain::interval::Interval;
use crate::domain::money::Usd;
use crate::domain::provenance::{CostModelId, DerivationId, RateCardId, WindowCalibrationId};
use crate::domain::quota::{PercentagePoints, QuotaRemaining};
use crate::domain::time::{MonotonicDuration, UtcDate, UtcTimestamp};
use crate::domain::tokens::{TokenCount, UsageVector};
use crate::domain::window::{
    ModelId, NominalWindowDuration, WindowResetState, WindowScope, WindowSeverity,
};
use crate::evidence::{
    CoverageCompleteness, Derivation, EvidenceQuality, MissingRequiredFacts, Provenance,
    RequiredFact,
};
use crate::logging::LogicalName;
use crate::report::activity::ActiveActivityState;
use crate::report::provenance::{ProvenanceGraph, ProvenanceNode, ReportField};
pub use crate::store::export::{ExportKey, ExportRow, UsageByTokenClass};
pub use crate::store::task_identity::TaskIdentityRow;
use crate::valuation::ValuationOutcome;

/// A monotonically increasing ledger generation.
///
/// The database advances this on every transaction that changes projection-relevant
/// durable state, and a report records the exact generation it was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerGeneration(u64);

impl LedgerGeneration {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// A monotonically increasing transcript-ingestion generation.
///
/// Present only on reports that consume transcript-derived material; a report over
/// meter evidence alone has no ingestion generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IngestionGeneration(u64);

impl IngestionGeneration {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Report-level metadata shared by every command.
///
/// `generated_at` says when the report was rendered and `knowledge_at` says which
/// witness set it was rendered against, which are different facts once a corrected
/// rate card or calibration lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportMetadata {
    pub generated_at: UtcTimestamp,
    pub knowledge_at: UtcTimestamp,
    pub ledger_generation: LedgerGeneration,
    pub ingestion_generation: Option<IngestionGeneration>,
    pub rate_card_version: Option<RateCardId>,
}

impl ReportMetadata {
    pub fn new(
        generated_at: UtcTimestamp,
        knowledge_at: UtcTimestamp,
        ledger_generation: LedgerGeneration,
        ingestion_generation: Option<IngestionGeneration>,
    ) -> Self {
        Self {
            generated_at,
            knowledge_at,
            ledger_generation,
            ingestion_generation,
            rate_card_version: None,
        }
    }

    pub fn with_rate_card_version(mut self, rate_card_version: Option<RateCardId>) -> Self {
        self.rate_card_version = rate_card_version;
        self
    }
}

/// A named account with a meter reading carrying exactly one freshness variant.
///
/// The reading is a [`Freshness`] over the remaining quota, so a renderer always
/// knows whether the number is fresh, stale or auth-required and never has to infer
/// staleness from a timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterAccount {
    pub account: LogicalName,
    pub reading: Freshness<QuotaRemaining>,
    /// The window whose remaining fraction the reading reports, when the
    /// reading was computed from a projection's windows. A reading without one
    /// has no window to name, and the renderer shows the value bare.
    pub limiting_window: Option<LimitingWindow>,
    /// Every window scope included in the reading, when the reading came from
    /// a projection; empty for a reading with no window context.
    pub included_scopes: Vec<WindowScope>,
    /// The model a `--model` selector chose, reported so the output identifies
    /// the selection the reading was computed under.
    pub selected_model: Option<ModelId>,
    /// Provider facts available to the explain renderer for a projected
    /// observation. This is absent for reports assembled without meter state.
    pub meter_explanation: Option<MeterExplanation>,
}

impl MeterAccount {
    pub fn new(account: LogicalName, reading: Freshness<QuotaRemaining>) -> Self {
        Self {
            account,
            reading,
            limiting_window: None,
            included_scopes: Vec::new(),
            selected_model: None,
            meter_explanation: None,
        }
    }

    /// A reading computed from a projection: it names the window it is limited
    /// by, the scopes it included and the model selector it was computed under.
    pub fn from_projection(
        account: LogicalName,
        reading: Freshness<QuotaRemaining>,
        limiting_window: Option<LimitingWindow>,
        included_scopes: Vec<WindowScope>,
        selected_model: Option<ModelId>,
    ) -> Self {
        Self {
            account,
            reading,
            limiting_window,
            included_scopes,
            selected_model,
            meter_explanation: None,
        }
    }

    pub fn with_meter_explanation(mut self, explanation: MeterExplanation) -> Self {
        self.meter_explanation = Some(explanation);
        self
    }
}

/// Provider contract and raw window facts shown by `--explain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterExplanation {
    pub provider_contract_id: ProviderContractId,
    pub windows: Vec<MeterWindowExplanation>,
}

/// The non-derived provider facts behind one explained window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterWindowExplanation {
    pub semantic_key: String,
    pub scope: WindowScope,
    pub is_active: bool,
    pub severity: WindowSeverity,
}

/// The window behind a reading: its scope and the nominal length the design's
/// fresh rendering shows beside the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitingWindow {
    pub scope: WindowScope,
    pub nominal_duration: NominalWindowDuration,
    pub reset_state: WindowResetState,
}

/// Whether the status command could read the projection, and why not when it
/// could not. A report whose projection is unavailable carries no readings:
/// the fact travels here instead of as a fabricated account value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionReadState {
    Read,
    Unavailable { state: &'static str, reason: String },
}

/// Provenance material for one account's meter reading.
///
/// The report constructor assembles the graph from this, so the renderer never
/// computes any part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterReadingProvenance {
    pub account: LogicalName,
    pub node: ProvenanceNode,
}

impl MeterReadingProvenance {
    pub fn new(account: LogicalName, node: ProvenanceNode) -> Self {
        Self { account, node }
    }
}

/// The status projection: the current compact meter picture.
/// The status projection: the current compact meter picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub metadata: ReportMetadata,
    pub accounts: Vec<MeterAccount>,
    pub provenance: ProvenanceGraph,
    /// Whether the projection behind every reading could be read.
    pub projection_state: ProjectionReadState,
}

impl StatusReport {
    pub fn new(
        metadata: ReportMetadata,
        accounts: Vec<MeterAccount>,
        readings: Vec<MeterReadingProvenance>,
        projection_state: ProjectionReadState,
    ) -> Self {
        let provenance = ProvenanceGraph::new(readings.into_iter().map(|reading| {
            (
                ReportField::MeterQuotaRemaining {
                    account: reading.account,
                },
                reading.node,
            )
        }));
        Self {
            metadata,
            accounts,
            provenance,
            projection_state,
        }
    }
}

/// The live meter report for `aub now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowReport {
    pub metadata: ReportMetadata,
    pub accounts: Vec<MeterAccount>,
    pub provenance: ProvenanceGraph,
    /// Explicit marker-backed live account activity (`aub-mgv.5`), separate from
    /// the meter readings above: a moving meter never substitutes for it.
    pub activity: ActiveActivityState,
}

impl NowReport {
    pub fn new(
        metadata: ReportMetadata,
        accounts: Vec<MeterAccount>,
        readings: Vec<MeterReadingProvenance>,
    ) -> Self {
        let provenance = ProvenanceGraph::new(readings.into_iter().map(|reading| {
            (
                ReportField::MeterQuotaRemaining {
                    account: reading.account,
                },
                reading.node,
            )
        }));
        Self {
            metadata,
            accounts,
            provenance,
            activity: ActiveActivityState::NoEvidence,
        }
    }

    /// Attaches the composed activity state. A report built without evaluating
    /// any session (no `--session-id` given) keeps the [`NowReport::new`]
    /// default of [`ActiveActivityState::NoEvidence`], which is the correct
    /// disposition rather than an omission: nothing was named to claim.
    pub fn with_activity(mut self, activity: ActiveActivityState) -> Self {
        self.activity = activity;
        self
    }
}

/// One group of a spend report, keyed by day, session, project, repository, account
/// or task. The usage is a vector over token kinds, never one collapsed count,
/// and it carries its own coverage and evidence quality; the provenance names the
/// files the group was read from and the derivation identifier binds the group to
/// the manifest it was computed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendGroup {
    pub key: LogicalName,
    pub usage: UsageVector,
    pub valuation: Option<ValuationOutcome<Usd>>,
    pub provenance: Provenance,
    pub derivation_id: DerivationId,
    /// Subscription credits when the caller explicitly requested conversion.
    /// A refusal stays alongside tokens, rather than suppressing the token report.
    pub credits: Option<Derivation<Credits>>,
    /// The optional calibrated movement represented by this group's qualified credits.
    /// A refusal stays alongside both credits and tokens, so an unavailable calibration
    /// never erases the independently reportable spend dimensions.
    pub window_equivalent: Option<WindowEquivalentDerivation>,
    /// Groups requested after this one. A report with more than one grouping
    /// dimension is a tree, so every parent subtotal has the same typed usage
    /// vector as its children rather than a lossy scalar total.
    pub children: Vec<SpendGroup>,
}

impl SpendGroup {
    pub fn new(
        key: LogicalName,
        usage: UsageVector,
        provenance: Provenance,
        derivation_id: DerivationId,
    ) -> Self {
        Self {
            key,
            usage,
            valuation: None,
            provenance,
            derivation_id,
            credits: None,
            window_equivalent: None,
            children: Vec::new(),
        }
    }

    pub fn with_valuation(mut self, valuation: Option<ValuationOutcome<Usd>>) -> Self {
        self.valuation = valuation;
        self
    }

    pub fn with_children(mut self, children: Vec<SpendGroup>) -> Self {
        self.children = children;
        self
    }

    pub fn with_credits(mut self, credits: Derivation<Credits>) -> Self {
        self.credits = Some(credits);
        self
    }

    pub fn with_window_equivalent(mut self, window_equivalent: WindowEquivalentDerivation) -> Self {
        self.window_equivalent = Some(window_equivalent);
        self
    }
}

/// A supported canonical-ledger grouping dimension. `Task` groups by the
/// same temporal-segmentation attribution `aub task report` and
/// `aub task overhead` read back (`aub-eu7.4`), never by grouping logic of
/// its own. Future credits, calibrated-window and valuation work extends
/// this enum in its own bead.
///
/// `Account` is not a property of one canonical event the way the other
/// dimensions are: it is decided by the session's account-marker timeline, so
/// the report layer resolves it through [`crate::attribution::account_segment`]
/// before grouping and never reasons about markers itself (aub-mgv.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpendGrouping {
    Day,
    Session,
    Project,
    Repository,
    Task,
    Account,
}

impl SpendGrouping {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Session => "session",
            Self::Project => "project",
            Self::Repository => "repository",
            Self::Task => "task",
            Self::Account => "account",
        }
    }
}

/// The label the report and the presentation layer use for usage that no account
/// marker could justify. It is a group in its own right, never omitted and never
/// merged into an attributed account (PLAN.md 19.2).
pub const UNKNOWN_ACCOUNT_LABEL: &str = "unknown-account";

/// One account group's attribution provenance, surfaced under `--explain`: the
/// evidence class the marker interval carried and the exact markers that
/// produced it. Carried on the report rather than folded into a group's usage,
/// because attribution confidence and token-measurement confidence are distinct
/// axes (correctness invariant 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupExplain {
    /// The account-dimension key, `account=<label>`, matching the group key
    /// segment regardless of where the account dimension sits in the tree.
    pub key: LogicalName,
    /// The effective evidence class of the interval that attributed this account.
    pub evidence_class: AccountEvidenceClass,
    /// The markers that produced the attribution, in a deterministic order. Empty
    /// for the unknown-account group: no marker justified that usage.
    pub markers: Vec<AccountMarkerReference>,
}

/// A stable reference to one persisted account marker and the evidence it carried.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccountMarkerReference {
    /// `session_account_marker:<id>`, the row's durable identity.
    pub reference: String,
    /// The account the marker named.
    pub logical_account: String,
    /// The marker's effective evidence class.
    pub evidence_class: AccountEvidenceClass,
}

/// What ingestion did to produce a spend report, so the counts never stand alone:
/// a report reads as complete only when the reader can see nothing was quarantined,
/// nothing was unreadable and nothing fell outside the window unexplained.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngestSummary {
    /// Whether this report asked the ingest path to refresh the canonical ledger.
    pub refresh_attempted: bool,
    /// A refresh failure that left a previously known canonical subtotal readable.
    pub refresh_failure: Option<String>,
    /// Files opened and parsed.
    pub files_read: u64,
    /// Files skipped because their modification time predates the window; an
    /// append-only transcript that has not changed since the window opened holds no
    /// event inside it.
    pub files_skipped_before_window: u64,
    /// Files that could not be read, by path. Non-empty makes the report incomplete.
    pub unreadable_files: Vec<String>,
    /// Quarantined records, by the parser's quarantine class name.
    pub quarantined_by_class: BTreeMap<String, u64>,
    /// Occurrences that collapsed into an already-counted identity.
    pub replayed_occurrences: u64,
    /// Occurrences sharing an identity with a counted event but disagreeing with it.
    pub collisions: u64,
    /// Canonical events that carried no strong identity.
    pub without_identity: u64,
    /// Distinct heuristic identities retained in the canonical ledger. This is
    /// diagnostic evidence, separate from canonical records and replays.
    pub heuristic_identities: u64,
    /// Canonical events whose record carried no readable timestamp; never placed
    /// in a day.
    pub undated_events: u64,
    /// Canonical events dated outside the requested window.
    pub events_outside_window: u64,
    /// Canonical events inside the window, which is what the groups sum.
    pub events_in_window: u64,
}

/// Provenance material for one spend group's token count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendGroupProvenance {
    pub key: LogicalName,
    pub node: ProvenanceNode,
}

/// A non-quantity spend diagnostic that is only rendered under `--explain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendDiagnostic {
    CanonicalRecords,
    ReplayedOccurrences,
    HeuristicIdentities,
}

impl SpendDiagnostic {
    fn report_field(self) -> ReportField {
        match self {
            Self::CanonicalRecords => ReportField::SpendCanonicalRecords,
            Self::ReplayedOccurrences => ReportField::SpendReplayedOccurrences,
            Self::HeuristicIdentities => ReportField::SpendHeuristicIdentities,
        }
    }
}

/// Provenance for a separately counted canonical-ledger diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendDiagnosticProvenance {
    pub diagnostic: SpendDiagnostic,
    pub node: ProvenanceNode,
}

impl SpendGroupProvenance {
    pub fn new(key: LogicalName, node: ProvenanceNode) -> Self {
        Self { key, node }
    }
}

/// Provenance material for one spend group's requested credit conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendGroupCreditsProvenance {
    pub key: LogicalName,
    pub node: ProvenanceNode,
}

impl SpendGroupCreditsProvenance {
    pub fn new(key: LogicalName, node: ProvenanceNode) -> Self {
        Self { key, node }
    }
}

/// A qualified interval of quota-window percentage-point movement. The calibration
/// identifier travels with the interval because the same credit subtotal can map to
/// a different movement under another window or another fitted result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEquivalentValue {
    pub interval: Interval<PercentagePoints>,
    pub calibration_id: WindowCalibrationId,
    pub coverage: CoverageCompleteness,
    pub quality: EvidenceQuality<PercentagePoints>,
    pub provenance: Provenance,
}

/// The calibrated window-equivalent result for one spend group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowEquivalentDerivation {
    /// A bounded movement interval with its calibration and evidence witnesses.
    Available(WindowEquivalentValue),
    /// A requested conversion that could not be justified. The token and credit
    /// derivations remain available independently of this refusal.
    Unavailable {
        missing: BTreeSet<RequiredFact>,
        provenance: Provenance,
    },
}

impl WindowEquivalentDerivation {
    /// Constructs a refusal only when at least one missing fact is named.
    pub fn unavailable(
        missing: impl IntoIterator<Item = RequiredFact>,
        provenance: Provenance,
    ) -> Result<Self, MissingRequiredFacts> {
        let missing = missing.into_iter().collect::<BTreeSet<_>>();
        if missing.is_empty() {
            return Err(MissingRequiredFacts);
        }
        Ok(Self::Unavailable {
            missing,
            provenance,
        })
    }

    /// The missing facts in a refusal, or `None` for an available interval.
    pub fn missing(&self) -> Option<&BTreeSet<RequiredFact>> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable { missing, .. } => Some(missing),
        }
    }

    /// The provenance carried by either an interval or its refusal.
    pub fn provenance(&self) -> &Provenance {
        match self {
            Self::Available(value) => &value.provenance,
            Self::Unavailable { provenance, .. } => provenance,
        }
    }

    /// The calibration witness when one was resolved for this result.
    pub fn calibration_id(&self) -> Option<&WindowCalibrationId> {
        match self {
            Self::Available(value) => Some(&value.calibration_id),
            Self::Unavailable { .. } => None,
        }
    }
}

/// Provenance for one spend group's requested calibrated window-equivalent result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendGroupWindowEquivalentProvenance {
    pub key: LogicalName,
    pub node: ProvenanceNode,
}

impl SpendGroupWindowEquivalentProvenance {
    pub fn new(key: LogicalName, node: ProvenanceNode) -> Self {
        Self { key, node }
    }
}

/// What `aub clear-diagnostics` removed, as the renderer sees it.
///
/// Separate from `store::retention`'s own result of the same name, and deliberately so:
/// the store owns what happened on disk, this owns what is reported, and presentation may
/// only see the second. `IngestReport` carries the same split for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearDiagnosticsReport {
    pub entries_removed: u64,
    pub bytes_removed: u64,
    pub provider_filter: Option<String>,
}

/// The spend report for `aub spend`: the window it covers, the groups, the
/// provenance graph and the ingestion summary the groups were built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendReport {
    pub metadata: ReportMetadata,
    /// The first UTC day of the window, inclusive.
    pub since: UtcDate,
    /// The first UTC day after the window, exclusive.
    pub until: UtcDate,
    /// The ordered dimensions the group tree follows.
    pub grouping: Vec<SpendGrouping>,
    pub groups: Vec<SpendGroup>,
    pub provenance: ProvenanceGraph,
    pub ingest: IngestSummary,
    pub stale_rate_card_note: Option<String>,
    /// The cost model the credit conversion was requested against, when it was
    /// requested at all. Carried on the report rather than read back out of a
    /// group's provenance so that a window whose every group refused conversion
    /// still names the model the refusal was measured against.
    pub credit_model: Option<CostModelId>,
    /// The semantic window requested for calibrated spend conversion, when that
    /// optional dimension was requested.
    pub window_equivalent_window: Option<String>,
    /// One entry per distinct account group when `--group-by account` was
    /// requested, naming the marker evidence behind each attribution. Empty
    /// otherwise.
    pub account_explain: Vec<AccountGroupExplain>,
}

impl SpendReport {
    pub fn new(
        metadata: ReportMetadata,
        since: UtcDate,
        until: UtcDate,
        groups: Vec<SpendGroup>,
        group_provenance: Vec<SpendGroupProvenance>,
        ingest: IngestSummary,
    ) -> Self {
        let provenance = ProvenanceGraph::new(
            group_provenance
                .into_iter()
                .map(|group| (ReportField::SpendGroupTokens { key: group.key }, group.node)),
        );
        Self {
            metadata,
            since,
            until,
            grouping: vec![SpendGrouping::Day],
            groups,
            provenance,
            ingest,
            stale_rate_card_note: None,
            credit_model: None,
            window_equivalent_window: None,
            account_explain: Vec::new(),
        }
    }

    pub fn with_account_explain(mut self, account_explain: Vec<AccountGroupExplain>) -> Self {
        self.account_explain = account_explain;
        self
    }

    pub fn with_stale_rate_card_note(mut self, note: Option<String>) -> Self {
        self.stale_rate_card_note = note;
        self
    }

    pub fn with_grouping(mut self, grouping: Vec<SpendGrouping>) -> Self {
        self.grouping = grouping;
        self
    }

    pub fn with_credit_model(mut self, model: Option<CostModelId>) -> Self {
        self.credit_model = model;
        self
    }

    pub fn with_window_equivalent_window(mut self, window: Option<String>) -> Self {
        self.window_equivalent_window = window;
        self
    }

    pub fn with_credit_provenance(mut self, credits: Vec<SpendGroupCreditsProvenance>) -> Self {
        self.provenance = self
            .provenance
            .with_added(credits.into_iter().map(|credit| {
                (
                    ReportField::SpendGroupCredits { key: credit.key },
                    credit.node,
                )
            }));
        self
    }

    pub fn with_window_equivalent_provenance(
        mut self,
        values: Vec<SpendGroupWindowEquivalentProvenance>,
    ) -> Self {
        self.provenance = self.provenance.with_added(values.into_iter().map(|value| {
            (
                ReportField::SpendGroupWindowEquivalent { key: value.key },
                value.node,
            )
        }));
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: Vec<SpendDiagnosticProvenance>) -> Self {
        self.provenance = self.provenance.with_added(
            diagnostics
                .into_iter()
                .map(|diagnostic| (diagnostic.diagnostic.report_field(), diagnostic.node)),
        );
        self
    }
}

/// The coverage report for `aub coverage`: one row per covered account over
/// the report's interval, the failure classes behind each account's numbers,
/// and the threshold verdict over exactly the accounts the report shows.
///
/// Each account's numbers arrive in [`CoverageAccount`] as the coverage
/// engine's own output. The engine's `Option` fields and `CoverageFraction`
/// carry the refusal semantics (no policy snapshot in force, a zero
/// denominator, no terminal attempt) into both renderings, so neither can
/// invent a number where the engine refused one, and the renderer holds no
/// quantity it could recompute.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageReport {
    pub metadata: ReportMetadata,
    /// The half-open interval `[since, until)` the report covers, in UTC.
    pub since: UtcTimestamp,
    pub until: UtcTimestamp,
    /// Whether the command line asked for severe accounts only.
    pub severe_only: bool,
    /// The configured floors and the verdict over the accounts in this report.
    pub threshold: CoverageThreshold,
    /// One row per covered account, in the order the report covers them.
    pub accounts: Vec<CoverageAccount>,
    pub provenance: ProvenanceGraph,
}

impl CoverageReport {
    pub fn new(
        metadata: ReportMetadata,
        since: UtcTimestamp,
        until: UtcTimestamp,
        severe_only: bool,
        threshold: CoverageThreshold,
        accounts: Vec<CoverageAccount>,
    ) -> Self {
        let provenance = ProvenanceGraph::new(accounts.iter().map(|account| {
            (
                ReportField::Coverage {
                    account: account.name.clone(),
                },
                account.provenance.clone(),
            )
        }));
        Self {
            metadata,
            since,
            until,
            severe_only,
            threshold,
            accounts,
            provenance,
        }
    }
}

/// One account's coverage over the report's interval.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageAccount {
    pub name: LogicalName,
    /// The engine's own report over the interval.
    pub engine: crate::coverage::CoverageReport,
    /// Terminal failures grouped by the four classes PLAN.md section 15
    /// distinguishes in measurement coverage.
    pub failures: crate::report::coverage::CoverageFailureTally,
    /// The known quota resets that fell inside a no-attempt gap, each with the
    /// nominal length of the window that reported it.
    pub resets_in_gaps: Vec<CoverageReset>,
    /// Historical observations imported from the legacy series. They are
    /// visible here, but excluded from the scheduler's attempt denominator.
    pub legacy_evidence_present: bool,
    /// True when the account matches an account in the active configuration.
    /// An unconfigured account is excluded from the coverage threshold verdict.
    pub configured: bool,
    /// The provenance node for this account's coverage quantities.
    pub provenance: ProvenanceNode,
}

/// A known quota reset that fell inside a no-attempt gap, with the nominal
/// length of the window that reported it. The window length is what the
/// detail block names when the report says a reset was lost to a blind gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageReset {
    pub at: UtcTimestamp,
    pub window_length: MonotonicDuration,
}

/// The configured floors and the verdict over the accounts the report covers.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageThreshold {
    pub attempt_floor: CoverageFloor,
    pub measurement_floor: CoverageFloor,
    /// True when no account in this report breaches either floor. The
    /// selectors are part of the verdict: only the accounts the report shows
    /// are judged, so `--account` and `--severe` scope the exit decision to
    /// exactly what the operator asked to see.
    pub met: bool,
    /// Every breach, in account order. Empty when `met`.
    pub breaches: Vec<CoverageBreach>,
}

/// One floor breached by one account.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageBreach {
    pub account: LogicalName,
    pub dimension: CoverageBreachDimension,
    pub coverage: CoverageFraction,
    pub floor: CoverageFloor,
}

/// Which floor a breach is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageBreachDimension {
    Attempt,
    Measurement,
}

impl CoverageBreachDimension {
    /// The stable JSON key of this dimension.
    pub fn key(self) -> &'static str {
        match self {
            CoverageBreachDimension::Attempt => "attempt",
            CoverageBreachDimension::Measurement => "measurement",
        }
    }
}

/// One sampling attempt outcome in a sample report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleAttempt {
    pub account: LogicalName,
    pub outcome: AttemptOutcome,
}

impl SampleAttempt {
    pub fn new(account: LogicalName, outcome: AttemptOutcome) -> Self {
        Self { account, outcome }
    }
}

/// The sample report for `aub sample`.
///
/// Sampling outcomes are not quantities, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleReport {
    pub metadata: ReportMetadata,
    pub attempts: Vec<SampleAttempt>,
    pub provenance: ProvenanceGraph,
}

impl SampleReport {
    pub fn new(metadata: ReportMetadata, attempts: Vec<SampleAttempt>) -> Self {
        Self {
            metadata,
            attempts,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The ingest report for `aub ingest`.
///
/// The ingestion generation is ledger metadata, not a measured quantity, so
/// the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    pub metadata: ReportMetadata,
    pub ingestion_generation: IngestionGeneration,
    pub provenance: ProvenanceGraph,
}

impl IngestReport {
    pub fn new(metadata: ReportMetadata, ingestion_generation: IngestionGeneration) -> Self {
        Self {
            metadata,
            ingestion_generation,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The backup report for `aub backup`.
///
/// A boolean verdict is not a quantity, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    pub metadata: ReportMetadata,
    pub verified: bool,
    pub provenance: ProvenanceGraph,
}

impl BackupReport {
    pub fn new(metadata: ReportMetadata, verified: bool) -> Self {
        Self {
            metadata,
            verified,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The doctor report for `aub doctor`.
///
/// Check names are not quantities, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub metadata: ReportMetadata,
    pub checks: Vec<LogicalName>,
    pub provenance: ProvenanceGraph,
}

impl DoctorReport {
    pub fn new(metadata: ReportMetadata, checks: Vec<LogicalName>) -> Self {
        Self {
            metadata,
            checks,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// A share of an overhead total, in parts per million. Not a domain quantity
/// requiring calibration semantics (`aub-eu7.4`'s "magnitude and share"
/// criterion is about a report-rendering ratio, not a billed or measured
/// value), so it lives here rather than in `domain`, following the same
/// parts-per-million idiom `QuotaFractionPpm` established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SharePpm(u32);

impl SharePpm {
    pub const MAX: u32 = 1_000_000;

    /// Computes `part`'s share of `total`, in parts per million. A zero total
    /// has no defined share and reports zero rather than dividing by zero:
    /// the caller (an overhead report with no usage at all) has nothing to
    /// apportion in the first place.
    pub fn of(part: u64, total: u64) -> Self {
        if total == 0 {
            return Self(0);
        }
        let ppm = (u128::from(part) * u128::from(Self::MAX)) / u128::from(total);
        Self(ppm.min(u128::from(Self::MAX)) as u32)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// What `aub task ingest` did to the durable event history, as the renderer
/// sees it.
///
/// Separate from `store::task_event`'s own [`IngestSummary`](crate::store::task_event::IngestSummary)
/// of the same name, and deliberately so: the store owns what happened against the
/// tracker connection, this owns what is reported, and presentation may only see
/// the second. [`IngestReport`] carries the same split for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskIngestReport {
    pub events_inserted: u64,
    pub events_already_present: u64,
    pub quarantines_inserted: u64,
    pub quarantines_already_present: u64,
}

/// One session's contribution to a task's total usage, for `aub task
/// report`'s session listing. `run` is the session's own run identifier,
/// retained where the session carries one so `aub` can emit it for the
/// external friction-ledger join (`aub-eu7.4`; `aub-xus.7` owns emitting it
/// through `aub export`). `aub` never interprets what the run identifier
/// means beyond carrying it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSessionUsage {
    pub session: LogicalName,
    pub run: Option<NativeRunId>,
    pub usage: UsageVector,
}

/// The task report for `aub task report TASK-ID`: the task's total usage
/// across every session that contributed to it, its resolved task-kind
/// identity, subscription credits where a complete cost model exists, and
/// the sessions the total is made of.
///
/// `task_kind` is `None` when the tracker supplied no kind-asserting
/// evidence for this task at all, distinct from
/// [`crate::attribution::TaskIdentityState::Unknown`] (evidence existed and
/// asserted no kind under the current mapping): the two are different facts
/// and a renderer that collapsed them would misreport "the tracker never
/// mentioned this task's kind" as "the tracker's evidence was ambiguous".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReport {
    pub metadata: ReportMetadata,
    pub task_id: LogicalName,
    pub task_kind: Option<TaskIdentityRow>,
    pub usage: UsageVector,
    pub credits: Derivation<Credits>,
    pub sessions: Vec<TaskSessionUsage>,
    pub provenance: ProvenanceGraph,
}

impl TaskReport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        metadata: ReportMetadata,
        task_id: LogicalName,
        task_kind: Option<TaskIdentityRow>,
        usage: UsageVector,
        credits: Derivation<Credits>,
        sessions: Vec<TaskSessionUsage>,
        usage_node: ProvenanceNode,
        credits_node: ProvenanceNode,
    ) -> Self {
        let provenance = ProvenanceGraph::new([
            (
                ReportField::TaskUsage {
                    task_id: task_id.clone(),
                },
                usage_node,
            ),
            (
                ReportField::TaskCredits {
                    task_id: task_id.clone(),
                },
                credits_node,
            ),
        ]);
        Self {
            metadata,
            task_id,
            task_kind,
            usage,
            credits,
            sessions,
            provenance,
        }
    }
}

/// One overhead bucket's magnitude and its share of the total overhead usage
/// in the report's window (`aub-eu7.4`'s "every overhead bucket with its
/// magnitude and its share" criterion). `share` is computed over the input
/// token count, the dimension every overhead reason's usage is compared on
/// consistently across buckets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOverheadBucket {
    pub reason: LogicalName,
    pub usage: UsageVector,
    pub share: SharePpm,
}

/// The task overhead report for `aub task overhead --since`: every overhead
/// bucket usage landed in over the report's window, alongside task-attributed
/// consumption (`aub-eu7.3`'s restored criterion: overhead renders alongside
/// task consumption, not behind a flag), so the total task-attributed and
/// total overhead usage in the window are both visible on one report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOverheadReport {
    pub metadata: ReportMetadata,
    pub since: UtcDate,
    pub until: UtcDate,
    pub task_usage: UsageVector,
    pub buckets: Vec<TaskOverheadBucket>,
    pub provenance: ProvenanceGraph,
}

impl TaskOverheadReport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        metadata: ReportMetadata,
        since: UtcDate,
        until: UtcDate,
        task_usage: UsageVector,
        task_usage_node: ProvenanceNode,
        buckets: Vec<TaskOverheadBucket>,
        bucket_nodes: Vec<(LogicalName, ProvenanceNode)>,
    ) -> Self {
        let mut nodes = vec![(ReportField::TaskOverheadTaskUsage, task_usage_node)];
        nodes.extend(
            bucket_nodes
                .into_iter()
                .map(|(reason, node)| (ReportField::TaskOverheadBucket { reason }, node)),
        );
        let provenance = ProvenanceGraph::new(nodes);
        Self {
            metadata,
            since,
            until,
            task_usage,
            buckets,
            provenance,
        }
    }
}

/// The calibration report for the `aub calibrate` command family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateReport {
    pub metadata: ReportMetadata,
    pub derivation: Derivation<TokenCount>,
    pub provenance: ProvenanceGraph,
}

impl CalibrateReport {
    pub fn new(
        metadata: ReportMetadata,
        derivation: Derivation<TokenCount>,
        node: ProvenanceNode,
    ) -> Self {
        let provenance = ProvenanceGraph::new([(ReportField::CalibrationTokens, node)]);
        Self {
            metadata,
            derivation,
            provenance,
        }
    }
}

/// The export report for `aub export`.
///
/// The rows are the typed records the store assembled; the renderer never
/// recomputes any of them. The unresolved-event count is an operational
/// counter describing what the assembly could not attribute, not a
/// measurement it reports: it exists to say the export is incomplete, exactly
/// as the ingest summary's counters say a spend report is incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub metadata: ReportMetadata,
    /// Which shared identifier the rows are keyed on.
    pub key: ExportKey,
    /// Whether logical project and repository identifiers were included in the
    /// rows, recorded here so the header can say what the export carries.
    pub included_logical_ids: bool,
    pub rows: Vec<ExportRow>,
    /// Usage events that could not be attributed to one namespaced session and
    /// so appear in no row.
    pub unresolved_events: u64,
    pub provenance: ProvenanceGraph,
}

impl ExportReport {
    pub fn new(
        metadata: ReportMetadata,
        key: ExportKey,
        included_logical_ids: bool,
        rows: Vec<ExportRow>,
        unresolved_events: u64,
        node: ProvenanceNode,
    ) -> Self {
        let provenance = ProvenanceGraph::new([(ReportField::ExportRows, node)]);
        Self {
            metadata,
            key,
            included_logical_ids,
            rows,
            unresolved_events,
            provenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::FreshnessKind;
    use crate::domain::provenance::{EvidenceId, QuerySemantics, WitnessId};
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::UtcTimestamp;
    use crate::evidence::CoverageCompleteness;
    use crate::report::provenance::{ProvenanceNode, ReportField, ValueArithmetic};

    fn metadata() -> ReportMetadata {
        ReportMetadata::new(
            UtcTimestamp::from_unix_nanos(2_000),
            UtcTimestamp::from_unix_nanos(1_000),
            LedgerGeneration::new(7),
            Some(IngestionGeneration::new(3)),
        )
    }

    fn remaining(ppm: u32) -> QuotaRemaining {
        QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap())
    }

    /// A canonical provenance node for tests: one member, one source, one
    /// observation, read directly.
    fn day() -> UtcDate {
        UtcDate::parse("2026-08-25").unwrap()
    }

    fn usage_vector() -> UsageVector {
        use crate::domain::tokens::{
            CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
        };
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(100),
                OutputTokens::new(50),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            crate::evidence::EvidenceQuality::Measured,
        )
    }

    fn node() -> ProvenanceNode {
        ProvenanceNode::new(
            [EvidenceId::new("ev-1")],
            [] as [WitnessId; 0],
            QuerySemantics::new("by-account", "all"),
            1,
            1,
            ValueArithmetic::Direct,
        )
    }

    /// A canonical coverage report for tests: one account whose engine report
    /// carries no numbers and whose graph resolves the account's field, so the
    /// enumeration sweep can resolve every variant against a real model.
    fn test_coverage_report(metadata: ReportMetadata) -> CoverageReport {
        let name = LogicalName::new("research");
        let node = ProvenanceNode::new(
            [] as [EvidenceId; 0],
            [] as [WitnessId; 0],
            QuerySemantics::new("coverage", "interval"),
            1,
            0,
            ValueArithmetic::Count,
        );
        CoverageReport::new(
            metadata,
            UtcTimestamp::from_unix_nanos(0),
            UtcTimestamp::from_unix_nanos(1),
            false,
            CoverageThreshold {
                attempt_floor: crate::config::CoverageFloor::new(0.98).unwrap(),
                measurement_floor: crate::config::CoverageFloor::new(0.95).unwrap(),
                met: true,
                breaches: Vec::new(),
            },
            vec![CoverageAccount {
                name: name.clone(),
                engine: crate::coverage::CoverageReport {
                    expected_opportunities: None,
                    attempted_opportunities: 0,
                    successful_observations: 0,
                    started_without_terminal_result: 0,
                    attempt_coverage: None,
                    measurement_coverage: None,
                    longest_no_attempt_gap: None,
                    longest_no_observation_gap: None,
                    reset_spanning_gaps: Vec::new(),
                    most_recent_timer_run: None,
                    most_recent_successful_observation: None,
                    severe: false,
                },
                failures: crate::report::coverage::CoverageFailureTally::default(),
                resets_in_gaps: Vec::new(),
                legacy_evidence_present: false,
                configured: true,
                provenance: node,
            }],
        )
    }

    /// The uniform accessor the metadata tests enumerate over.
    ///
    /// Implemented per model rather than derived, so a new report model has to be
    /// named here to be covered, and a model that drops its `metadata` field stops
    /// compiling instead of quietly leaving the enumeration.
    trait CarriesMetadata {
        fn metadata(&self) -> &ReportMetadata;
    }

    macro_rules! carries_metadata {
        ($($model:ty),+ $(,)?) => {
            $(impl CarriesMetadata for $model {
                fn metadata(&self) -> &ReportMetadata {
                    &self.metadata
                }
            })+
        };
    }

    carries_metadata!(
        StatusReport,
        NowReport,
        SpendReport,
        CoverageReport,
        SampleReport,
        IngestReport,
        BackupReport,
        DoctorReport,
        TaskReport,
        TaskOverheadReport,
        CalibrateReport,
        ExportReport,
    );

    /// One instance of every command's report model, each labelled with its command.
    fn every_model(m: ReportMetadata) -> Vec<(&'static str, Box<dyn CarriesMetadata>)> {
        let account = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(1),
            },
        );

        vec![
            (
                "status",
                Box::new(StatusReport::new(
                    m.clone(),
                    vec![account.clone()],
                    vec![MeterReadingProvenance::new(
                        LogicalName::new("work-a"),
                        node(),
                    )],
                    crate::report::ProjectionReadState::Read,
                )) as Box<dyn CarriesMetadata>,
            ),
            (
                "now",
                Box::new(NowReport::new(
                    m.clone(),
                    vec![account.clone()],
                    vec![MeterReadingProvenance::new(
                        LogicalName::new("work-a"),
                        node(),
                    )],
                )),
            ),
            (
                "spend",
                Box::new(SpendReport::new(
                    m.clone(),
                    day(),
                    day(),
                    vec![],
                    vec![],
                    IngestSummary::default(),
                )),
            ),
            ("coverage", Box::new(test_coverage_report(m.clone()))),
            ("sample", Box::new(SampleReport::new(m.clone(), vec![]))),
            (
                "ingest",
                Box::new(IngestReport::new(m.clone(), IngestionGeneration::new(3))),
            ),
            ("backup", Box::new(BackupReport::new(m.clone(), true))),
            ("doctor", Box::new(DoctorReport::new(m.clone(), vec![]))),
            (
                "task",
                Box::new(TaskReport::new(
                    m.clone(),
                    LogicalName::new("beads-a:aub-1"),
                    None,
                    usage_vector(),
                    Derivation::Unavailable {
                        missing: [crate::evidence::RequiredFact::new("active cost model")]
                            .into_iter()
                            .collect(),
                        provenance: crate::evidence::Provenance::new([]),
                    },
                    vec![],
                    node(),
                    node(),
                )),
            ),
            (
                "task-overhead",
                Box::new(TaskOverheadReport::new(
                    m.clone(),
                    day(),
                    day(),
                    usage_vector(),
                    node(),
                    vec![],
                    vec![],
                )),
            ),
            (
                "calibrate",
                Box::new(CalibrateReport::new(
                    m.clone(),
                    Derivation::Unavailable {
                        missing: Default::default(),
                        provenance: crate::evidence::Provenance::new([]),
                    },
                    node(),
                )),
            ),
            (
                "export",
                Box::new(ExportReport::new(
                    m.clone(),
                    ExportKey::Session,
                    false,
                    vec![],
                    0,
                    node(),
                )),
            ),
        ]
    }

    /// Every command's report model carries the report-level metadata, asserted
    /// field by field on each model.
    ///
    /// The previous version of this test built the same eleven models and asserted
    /// `len() == 11`, which is true of any eleven values and would have passed with
    /// every metadata field wrong or absent. What the criterion asks for is the
    /// assertion below: the generation time, the knowledge time and the ledger
    /// generation on each model, plus the transcript ingestion generation carried
    /// through wherever the caller supplies one.
    #[test]
    fn every_report_model_carries_the_metadata() {
        let models = every_model(metadata());
        assert_eq!(models.len(), 12, "every command must have a report model");

        for (command, model) in &models {
            let carried = model.metadata();
            assert_eq!(
                carried.generated_at,
                UtcTimestamp::from_unix_nanos(2_000),
                "the {command} model lost the generation time"
            );
            assert_eq!(
                carried.knowledge_at,
                UtcTimestamp::from_unix_nanos(1_000),
                "the {command} model lost the knowledge time"
            );
            assert_eq!(
                carried.ledger_generation,
                LedgerGeneration::new(7),
                "the {command} model lost the ledger generation"
            );
            assert_eq!(
                carried.ingestion_generation,
                Some(IngestionGeneration::new(3)),
                "the {command} model lost the transcript ingestion generation"
            );
        }
    }

    /// A model built without a transcript ingestion generation still carries the
    /// three unconditional fields.
    ///
    /// This is the near-identical negative of the test above: it differs in exactly
    /// the one dimension the "where relevant" wording covers, so a model that
    /// silently dropped the other three whenever ingestion was absent would pass
    /// there and fail here.
    #[test]
    fn a_model_with_no_ingestion_generation_still_carries_the_other_three() {
        let without = ReportMetadata::new(
            UtcTimestamp::from_unix_nanos(2_000),
            UtcTimestamp::from_unix_nanos(1_000),
            LedgerGeneration::new(7),
            None,
        );

        for (command, model) in &every_model(without) {
            let carried = model.metadata();
            assert_eq!(
                carried.generated_at,
                UtcTimestamp::from_unix_nanos(2_000),
                "the {command} model lost the generation time"
            );
            assert_eq!(
                carried.knowledge_at,
                UtcTimestamp::from_unix_nanos(1_000),
                "the {command} model lost the knowledge time"
            );
            assert_eq!(
                carried.ledger_generation,
                LedgerGeneration::new(7),
                "the {command} model lost the ledger generation"
            );
            assert_eq!(
                carried.ingestion_generation, None,
                "the {command} model invented an ingestion generation"
            );
        }
    }

    /// A meter reading carries exactly one freshness variant: the three kinds are
    /// exhaustive and a reading is always one of them.
    #[test]
    fn meter_readings_carry_exactly_one_freshness_variant() {
        let fresh = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::Fresh {
                observed: crate::domain::freshness::Observed::new(
                    remaining(500_000),
                    None,
                    crate::domain::time::ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
                    crate::domain::time::MeasurementBasis::ProviderObserved,
                ),
                latest_attempt: AttemptId::new(1),
            },
        );
        assert_eq!(fresh.reading.kind(), FreshnessKind::Fresh);

        let stale = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::Stale {
                last_good: None,
                latest_attempt: AttemptId::new(2),
                reason: crate::domain::freshness::StaleReason::AgeExceeded,
            },
        );
        assert_eq!(stale.reading.kind(), FreshnessKind::Stale);

        let auth = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(3),
            },
        );
        assert_eq!(auth.reading.kind(), FreshnessKind::AuthRequired);
    }

    /// The spend group's usage is a qualified vector over token kinds, never a bare
    /// newtype and never one collapsed count: the only constructor takes a
    /// `UsageVector`, whose coverage and quality are readable on the group.
    #[test]
    fn spend_group_usage_is_a_qualified_vector() {
        let group = SpendGroup::new(
            LogicalName::new("by-day"),
            usage_vector(),
            Provenance::new(["file.jsonl".to_string()]),
            DerivationId::from_manifest(&crate::domain::provenance::ProvenanceManifest::new(
                [],
                [],
                crate::domain::provenance::QuerySemantics::new("by-day", "all"),
            )),
        );
        assert_eq!(group.usage.coverage(), &CoverageCompleteness::Complete);
        assert_eq!(group.usage.known().input().value(), 100);
    }

    /// Every quantitative field of every report model resolves to a provenance
    /// node, enumerated exhaustively rather than sampled.
    ///
    /// The exhaustive match below is the compile half of the stated rejection
    /// mechanism: a [`ReportField`] variant added to the enum fails to compile
    /// here until the test is touched. The resolution sweep is the run half: a
    /// variant the constructors do not populate fails the test. Together they
    /// reject a quantitative field added without a provenance node.
    #[test]
    fn every_quantitative_field_resolves_to_a_provenance_node() {
        let m = metadata();
        let account = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::Fresh {
                observed: crate::domain::freshness::Observed::new(
                    remaining(500_000),
                    None,
                    crate::domain::time::ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
                    crate::domain::time::MeasurementBasis::ProviderObserved,
                ),
                latest_attempt: AttemptId::new(1),
            },
        );
        let group = SpendGroup::new(
            LogicalName::new("by-day"),
            usage_vector(),
            Provenance::new(["file.jsonl".to_string()]),
            DerivationId::from_manifest(&crate::domain::provenance::ProvenanceManifest::new(
                [],
                [],
                QuerySemantics::new("by-day", "all"),
            )),
        );

        let status = StatusReport::new(
            m.clone(),
            vec![account.clone()],
            vec![MeterReadingProvenance::new(
                LogicalName::new("work-a"),
                node(),
            )],
            crate::report::ProjectionReadState::Read,
        );
        let now = NowReport::new(
            m.clone(),
            vec![account.clone()],
            vec![MeterReadingProvenance::new(
                LogicalName::new("work-a"),
                node(),
            )],
        );
        let spend = SpendReport::new(
            m.clone(),
            day(),
            day(),
            vec![group],
            vec![SpendGroupProvenance::new(
                LogicalName::new("by-day"),
                node(),
            )],
            IngestSummary::default(),
        )
        .with_credit_provenance(vec![SpendGroupCreditsProvenance::new(
            LogicalName::new("by-day"),
            node(),
        )])
        .with_window_equivalent_provenance(vec![SpendGroupWindowEquivalentProvenance::new(
            LogicalName::new("by-day"),
            node(),
        )])
        .with_diagnostics(vec![
            SpendDiagnosticProvenance {
                diagnostic: SpendDiagnostic::CanonicalRecords,
                node: node(),
            },
            SpendDiagnosticProvenance {
                diagnostic: SpendDiagnostic::ReplayedOccurrences,
                node: node(),
            },
            SpendDiagnosticProvenance {
                diagnostic: SpendDiagnostic::HeuristicIdentities,
                node: node(),
            },
        ]);
        let coverage = test_coverage_report(m.clone());
        let export = ExportReport::new(m.clone(), ExportKey::Run, true, vec![], 0, node());
        let calibrate = CalibrateReport::new(
            m.clone(),
            Derivation::Unavailable {
                missing: Default::default(),
                provenance: crate::evidence::Provenance::new([]),
            },
            node(),
        );
        let task = TaskReport::new(
            m.clone(),
            LogicalName::new("beads-a:aub-1"),
            None,
            usage_vector(),
            Derivation::Unavailable {
                missing: [crate::evidence::RequiredFact::new("active cost model")]
                    .into_iter()
                    .collect(),
                provenance: crate::evidence::Provenance::new([]),
            },
            vec![],
            node(),
            node(),
        );
        let task_overhead = TaskOverheadReport::new(
            m.clone(),
            day(),
            day(),
            usage_vector(),
            node(),
            vec![],
            vec![(LogicalName::new("contended"), node())],
        );

        // The exhaustive match: adding a variant to the enum fails to compile
        // until this match is extended, which is the compile half of the guard.
        let field_kinds = [
            ReportField::MeterQuotaRemaining {
                account: LogicalName::new("work-a"),
            },
            ReportField::SpendGroupTokens {
                key: LogicalName::new("by-day"),
            },
            ReportField::SpendGroupCredits {
                key: LogicalName::new("by-day"),
            },
            ReportField::SpendGroupWindowEquivalent {
                key: LogicalName::new("by-day"),
            },
            ReportField::SpendCanonicalRecords,
            ReportField::SpendReplayedOccurrences,
            ReportField::SpendHeuristicIdentities,
            ReportField::Coverage {
                account: LogicalName::new("research"),
            },
            ReportField::ExportRows,
            ReportField::CalibrationTokens,
            ReportField::TaskUsage {
                task_id: LogicalName::new("beads-a:aub-1"),
            },
            ReportField::TaskCredits {
                task_id: LogicalName::new("beads-a:aub-1"),
            },
            ReportField::TaskOverheadTaskUsage,
            ReportField::TaskOverheadBucket {
                reason: LogicalName::new("contended"),
            },
        ];
        for field in &field_kinds {
            match field {
                ReportField::MeterQuotaRemaining { account } => {
                    assert!(status.provenance.resolve(field).is_some(), "{account:?}");
                    assert!(now.provenance.resolve(field).is_some(), "{account:?}");
                }
                ReportField::SpendGroupTokens { key }
                | ReportField::SpendGroupCredits { key }
                | ReportField::SpendGroupWindowEquivalent { key } => {
                    assert!(spend.provenance.resolve(field).is_some(), "{key:?}");
                }
                ReportField::SpendCanonicalRecords
                | ReportField::SpendReplayedOccurrences
                | ReportField::SpendHeuristicIdentities => {
                    assert!(spend.provenance.resolve(field).is_some(), "{field:?}");
                }
                ReportField::Coverage { account } => {
                    assert!(coverage.provenance.resolve(field).is_some(), "{account:?}");
                }
                ReportField::ExportRows => {
                    assert!(export.provenance.resolve(field).is_some());
                }
                ReportField::CalibrationTokens => {
                    assert!(calibrate.provenance.resolve(field).is_some());
                }
                ReportField::TaskUsage { task_id } | ReportField::TaskCredits { task_id } => {
                    assert!(task.provenance.resolve(field).is_some(), "{task_id:?}");
                }
                ReportField::TaskOverheadTaskUsage => {
                    assert!(task_overhead.provenance.resolve(field).is_some());
                }
                ReportField::TaskOverheadBucket { reason } => {
                    assert!(
                        task_overhead.provenance.resolve(field).is_some(),
                        "{reason:?}"
                    );
                }
            }
        }

        // Reports with no quantitative fields own an empty graph, so a renderer
        // can rely on the graph being present on every model.
        assert!(SampleReport::new(m.clone(), vec![]).provenance.is_empty());
        assert!(
            IngestReport::new(m.clone(), IngestionGeneration::new(3))
                .provenance
                .is_empty()
        );
        assert!(BackupReport::new(m.clone(), true).provenance.is_empty());
        assert!(DoctorReport::new(m.clone(), vec![]).provenance.is_empty());
    }

    /// The detection half of the rejection mechanism: a field the constructor
    /// did not populate is visible as a missing node, so the enumeration sweep
    /// above fails rather than silently rendering an unexplained quantity.
    #[test]
    fn an_unpopulated_field_is_detectable_as_a_missing_node() {
        let m = metadata();
        let account = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(1),
            },
        );
        // No provenance material: the constructor assembles an empty graph.
        let report = StatusReport::new(
            m,
            vec![account],
            vec![],
            crate::report::ProjectionReadState::Read,
        );
        assert!(
            report
                .provenance
                .resolve(&ReportField::MeterQuotaRemaining {
                    account: LogicalName::new("work-a"),
                })
                .is_none(),
            "a reading without a node must not resolve"
        );
    }

    /// Every node a report carries verifies against its own manifest: the
    /// expansion law is inherited from the provenance types, not restated.
    #[test]
    fn every_node_in_a_report_graph_verifies() {
        let m = metadata();
        let report = ExportReport::new(m, ExportKey::Session, false, vec![], 0, node());
        for (_, node) in report.provenance.iter() {
            assert!(node.verify());
        }
    }

    /// Every domain quantity this module could name bare. A field whose type
    /// mentions one of these and no qualifying wrapper is an unqualified number in
    /// a report model, which is exactly what this seam exists to prevent.
    const BARE_QUANTITIES: [&str; 16] = [
        "u8",
        "u16",
        "u32",
        "u64",
        "u128",
        "usize",
        "i8",
        "i16",
        "i32",
        "i64",
        "isize",
        "f32",
        "f64",
        "TokenCount",
        "UsageVector",
        "RowCount",
    ];

    /// The wrappers that qualify a quantity: coverage and evidence quality
    /// (`Qualified`), a truthful refusal (`Derivation`), or exactly one freshness
    /// variant (`Freshness`).
    const QUALIFYING_WRAPPERS: [&str; 3] = ["Qualified", "Derivation", "Freshness"];

    /// Fields that hold a quantity without one of those wrappers, each with the
    /// reason it is nonetheless not an unqualified report number. `"*"` covers
    /// every field of the struct.
    const STRUCTURALLY_QUALIFIED: [(&str, &str, &str); 9] = [
        (
            "IngestSummary",
            "*",
            "operational counters describing what the ingestion run did, not \
             measurements it reports; they exist to say the report is incomplete",
        ),
        (
            "TaskIngestReport",
            "*",
            "operational counters describing what the tracker-event ingestion run \
             did, not measurements it reports; they exist to say the report is \
             incomplete",
        ),
        (
            "SpendGroup",
            "usage",
            "a UsageVector carries its own coverage and evidence quality, with the \
             provenance and derivation identifier as sibling fields on the group",
        ),
        (
            "TaskReport",
            "usage",
            "a UsageVector carries its own coverage and evidence quality; the report's \
             own provenance graph carries the field's node, keyed by task id",
        ),
        (
            "TaskSessionUsage",
            "usage",
            "a UsageVector carries its own coverage and evidence quality; it is one \
             session's share of the parent TaskReport's already-qualified usage field",
        ),
        (
            "TaskOverheadBucket",
            "usage",
            "a UsageVector carries its own coverage and evidence quality; the parent \
             TaskOverheadReport's own provenance graph carries the field's node",
        ),
        (
            "TaskOverheadReport",
            "task_usage",
            "a UsageVector carries its own coverage and evidence quality; the report's \
             own provenance graph carries the field's node",
        ),
        (
            "ClearDiagnosticsReport",
            "*",
            "operational counters describing what the clearing run removed, not \
             measurements it reports; there is no coverage question in a count of \
             files this command itself deleted",
        ),
        (
            "ExportReport",
            "unresolved_events",
            "an operational counter describing what the assembly could not attribute, \
             not a measurement it reports; it exists to say the export is incomplete",
        ),
    ];

    /// One named field of one struct, as read out of the module's own source.
    #[derive(Debug, PartialEq, Eq)]
    struct SourceField {
        owner: String,
        name: String,
        type_text: String,
    }

    /// The named public fields of every `pub struct` in `source`.
    ///
    /// Reading the source is what makes this test enumerate the models rather than
    /// sample them: a field added to any model is seen here without anyone
    /// remembering to list it.
    fn public_fields(source: &str) -> Vec<SourceField> {
        let mut fields = Vec::new();
        let mut owner: Option<String> = None;
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("pub struct ")
                && let Some(name) = rest.strip_suffix(" {")
            {
                owner = Some(name.to_string());
                continue;
            }
            if line == "}" {
                owner = None;
                continue;
            }
            let Some(current) = owner.as_ref() else {
                continue;
            };
            let trimmed = line.trim();
            let Some(declaration) = trimmed.strip_prefix("pub ") else {
                continue;
            };
            let Some((name, type_text)) = declaration.split_once(": ") else {
                continue;
            };
            fields.push(SourceField {
                owner: current.clone(),
                name: name.to_string(),
                type_text: type_text.trim_end_matches(',').to_string(),
            });
        }
        fields
    }

    /// Whether `type_text` names a quantity with no qualifying wrapper around it.
    fn is_bare_quantity(type_text: &str) -> bool {
        let identifiers: Vec<&str> = type_text
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|piece| !piece.is_empty())
            .collect();
        let wrapped = identifiers
            .iter()
            .any(|identifier| QUALIFYING_WRAPPERS.contains(identifier));
        !wrapped
            && identifiers
                .iter()
                .any(|identifier| BARE_QUANTITIES.contains(identifier))
    }

    /// Every field of every model in `source` that holds an unqualified quantity,
    /// as `Struct.field: Type`.
    fn unqualified_quantity_fields(source: &str) -> Vec<String> {
        public_fields(source)
            .into_iter()
            .filter(|field| is_bare_quantity(&field.type_text))
            .filter(|field| {
                !STRUCTURALLY_QUALIFIED.iter().any(|(owner, name, _)| {
                    *owner == field.owner && (*name == "*" || *name == field.name)
                })
            })
            .map(|field| format!("{}.{}: {}", field.owner, field.name, field.type_text))
            .collect()
    }

    /// Every quantity field on every report model is qualified, enumerated from
    /// the module's own source rather than from a list somebody maintains.
    ///
    /// This is the test the export row count regression got past: `rows: u64` was
    /// added to `ExportReport` with a provenance node and no qualification, and
    /// nothing in this module objected.
    #[test]
    fn every_quantity_field_on_every_model_is_qualified() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/report/models.rs"))
                .expect("this module's own source must be readable");

        let unqualified = unqualified_quantity_fields(&source);
        assert!(
            unqualified.is_empty(),
            "unqualified quantity fields in report models: {unqualified:?}"
        );
    }

    /// A one-field report model, as source text, for the checker's own tests.
    fn fixture(field: &str) -> String {
        [
            "pub struct ExampleReport {",
            "    pub metadata: ReportMetadata,",
            &format!("    {field}"),
            "}",
        ]
        .join("\n")
    }

    /// The enumeration itself refuses a bare quantity and accepts the qualified
    /// form of the same field.
    ///
    /// The two sources below differ only in the wrapper around the quantity, which
    /// is the single dimension the rule is about: a checker that accepted both, or
    /// refused both, would still pass the test above against a clean tree.
    #[test]
    fn the_enumeration_refuses_a_bare_quantity_and_accepts_a_qualified_one() {
        // Assembled line by line rather than written as one literal block: a
        // literal would put `pub struct` at column zero in this file, and the
        // test above reads this file, so the fixture would be parsed as a real
        // report model and fail it.
        let bare = fixture("pub rows: u64,");
        let qualified = fixture("pub rows: Qualified<RowCount>,");
        assert_eq!(
            unqualified_quantity_fields(&bare),
            vec!["ExampleReport.rows: u64".to_string()]
        );
        assert!(unqualified_quantity_fields(&qualified).is_empty());
    }

    /// A quantity hidden inside a container is still a bare quantity, so wrapping
    /// one in a `Vec` is not a way around the rule.
    #[test]
    fn a_quantity_inside_a_container_is_still_bare() {
        let source = fixture("pub counts: Vec<TokenCount>,");
        assert_eq!(
            unqualified_quantity_fields(&source),
            vec!["ExampleReport.counts: Vec<TokenCount>".to_string()]
        );
    }

    /// The structurally qualified exceptions are exactly the nine documented here,
    /// each naming the reason it is not an unqualified number. A tenth one cannot
    /// be added without this test being edited, which is the point: the list is a
    /// decision, not a convenience.
    #[test]
    fn the_structurally_qualified_exceptions_are_documented() {
        assert_eq!(STRUCTURALLY_QUALIFIED.len(), 9);
        for (owner, _, reason) in STRUCTURALLY_QUALIFIED {
            assert!(
                !reason.is_empty(),
                "the exception for {owner} states no reason"
            );
        }
    }
}
