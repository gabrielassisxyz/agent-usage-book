//! The immutable cost model tables and their repository (`aub-ai3.1`, PLAN.md 12.13,
//! 12.14, 22).
//!
//! A witness row that gets edited in place cannot answer what `aub` would have said
//! last month, and that question has to stay answerable, so neither `cost_model` nor
//! `cost_model_term` exposes an update or delete path here, and the migration that
//! created them refuses `UPDATE` and `DELETE` at the database itself. Activation and
//! supersession are rows in `cost_model_lifecycle`, never a column on the model: the
//! model active at a past instant is the `cost_model_id` of the latest lifecycle row
//! at or before that instant, and a superseded model stays queryable because nothing
//! is ever deleted.
//!
//! The model carries two independent times, the ones this system distinguishes
//! everywhere: the validity interval says when the model describes the physical world,
//! and `published_at` says when `aub` learned it. Conflating them makes historical
//! reports irreproducible (PLAN.md 12.14).
//!
//! An incomplete model is representable on purpose: a term that cannot be established
//! is an unavailable fact, not a zero, and fail-closed conversion is the consumer's
//! job (`aub-ai3.2`). What this module guarantees is that whatever terms exist are
//! stored exactly once per (model, token kind), that the model's own identity never
//! changes after it is written, and that every activation records which model it
//! replaced.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use std::collections::BTreeSet;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::domain::credits::CreditsPerToken;
use crate::domain::ids::BillingSemanticsId;
use crate::domain::provenance::{
    CostModelId, EvidenceId, ProvenanceManifest, canonical_inputs_hash,
};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::TokenKind;
use crate::domain::window::ModelId;
use crate::error::Error;

/// A cost model row's SQLite rowid.
///
/// Internal identity only: the semantic [`CostModelId`] is what provenance manifests
/// and callers name a model by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CostModelDbId(i64);

impl CostModelDbId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The provider a cost model's coefficients describe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderKey(String);

impl ProviderKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A plan scope a model's billing semantics apply to, when they are plan-dependent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanScope(String);

impl PlanScope {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The model's own version string, distinct from every semantic identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CostModelVersion(String);

impl CostModelVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The evidence experiment a term was derived from, where one exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvidenceExperimentId(String);

impl EvidenceExperimentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which models a cost model's coefficients apply to: every model of the provider, or
/// exactly one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostModelScope {
    /// Coefficients apply to every model of the provider (the model-class scope).
    ModelClass,
    /// Coefficients apply to exactly this model.
    Model(ModelId),
}

/// The stated uncertainty of one coefficient: an absolute interval over it.
///
/// `None` on a term means the term states no uncertainty; a term that states one
/// carries both bounds, with the lower bound never above the upper one (enforced
/// here and by the table's CHECK constraint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoefficientUncertainty {
    lower: CreditsPerToken,
    upper: CreditsPerToken,
}

impl CoefficientUncertainty {
    /// Constructs an interval, rejecting a lower bound above its upper one.
    pub fn new(lower: CreditsPerToken, upper: CreditsPerToken) -> Result<Self, Error> {
        if lower.micros_per_million_tokens() > upper.micros_per_million_tokens() {
            return Err(Error::Store(format!(
                "coefficient uncertainty lower bound exceeds upper bound"
            )));
        }
        Ok(Self { lower, upper })
    }

    pub fn lower(&self) -> CreditsPerToken {
        self.lower
    }

    pub fn upper(&self) -> CreditsPerToken {
        self.upper
    }
}

/// How a term's coefficient was derived.
///
/// Three qualities of evidence, kept distinguishable after the model has been in use
/// for a year (PLAN.md 22, `aub-ai3.3`): a coefficient from published billing
/// semantics, one measured in a controlled experiment, and one assumed because
/// nothing better exists are different statements, and a report must be able to say
/// which one a number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermDerivationMethod {
    /// Derived from the provider's published billing semantics.
    PublishedBillingSemantics,
    /// Measured in a controlled experiment.
    ControlledExperiment,
    /// Assumed because no better evidence exists; never a zero standing in for an
    /// unknown, just a declared quality of evidence.
    Assumed,
}

impl TermDerivationMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PublishedBillingSemantics => "published_billing_semantics",
            Self::ControlledExperiment => "controlled_experiment",
            Self::Assumed => "assumed",
        }
    }
}

impl FromStr for TermDerivationMethod {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "published_billing_semantics" => Ok(Self::PublishedBillingSemantics),
            "controlled_experiment" => Ok(Self::ControlledExperiment),
            "assumed" => Ok(Self::Assumed),
            other => Err(Error::Store(format!(
                "unknown term derivation method: '{other}'"
            ))),
        }
    }
}

/// The validity interval of a model: when it describes the physical world.
///
/// Closed on both ends, matching the plan's interval vocabulary; the constructor
/// rejects a `valid_until` before its `valid_from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidityInterval {
    valid_from: UtcTimestamp,
    valid_until: UtcTimestamp,
}

impl ValidityInterval {
    pub fn new(valid_from: UtcTimestamp, valid_until: UtcTimestamp) -> Result<Self, Error> {
        if valid_from > valid_until {
            return Err(Error::Store(format!(
                "validity interval starts after it ends: {}ns > {}ns",
                valid_from.unix_nanos(),
                valid_until.unix_nanos()
            )));
        }
        Ok(Self {
            valid_from,
            valid_until,
        })
    }

    pub fn valid_from(&self) -> UtcTimestamp {
        self.valid_from
    }

    pub fn valid_until(&self) -> UtcTimestamp {
        self.valid_until
    }
}

/// Model-level provenance: the content address of the evidence set the model was
/// built from.
///
/// Stored as the FNV-1a digest of the canonical sorted evidence identifiers plus
/// their count, the exact pair `ProvenanceManifest` derives its own content address
/// from. The full manifest stays in memory for the lifetime of a session; the row
/// keeps the reproducible checksum so an expansion can always be re-verified.
///
/// The digest is carried as a `u64` rather than as `Digest` because `Digest` has no
/// public raw-value constructor and this module's blast radius does not include
/// `domain/provenance.rs`; the hex-encoded column round-trips through `as_u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelProvenance {
    digest: u64,
    input_count: usize,
}

impl ModelProvenance {
    /// The content address of a manifest's input evidence set.
    pub fn from_manifest(manifest: &ProvenanceManifest) -> Self {
        Self {
            digest: manifest.inputs_hash().as_u64(),
            input_count: manifest.input_count(),
        }
    }

    /// Rebuilds a provenance record from the stored parts.
    pub fn from_parts(digest: u64, input_count: usize) -> Self {
        Self {
            digest,
            input_count,
        }
    }

    /// The FNV-1a digest of the evidence set, as a raw value.
    pub fn digest(&self) -> u64 {
        self.digest
    }

    pub fn input_count(&self) -> usize {
        self.input_count
    }

    /// True when `inputs` is exactly the set whose digest produced this record.
    pub fn verify_expansion(&self, inputs: &BTreeSet<EvidenceId>) -> bool {
        inputs.len() == self.input_count && canonical_inputs_hash(inputs).as_u64() == self.digest
    }
}

/// One term of a cost model: what one token kind costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostModelTerm {
    token_kind: TokenKind,
    coefficient: CreditsPerToken,
    uncertainty: Option<CoefficientUncertainty>,
    derivation_method: TermDerivationMethod,
    evidence_experiment: Option<EvidenceExperimentId>,
}

impl CostModelTerm {
    pub fn new(
        token_kind: TokenKind,
        coefficient: CreditsPerToken,
        uncertainty: Option<CoefficientUncertainty>,
        derivation_method: TermDerivationMethod,
        evidence_experiment: Option<EvidenceExperimentId>,
    ) -> Self {
        Self {
            token_kind,
            coefficient,
            uncertainty,
            derivation_method,
            evidence_experiment,
        }
    }

    pub fn token_kind(&self) -> TokenKind {
        self.token_kind
    }

    pub fn coefficient(&self) -> CreditsPerToken {
        self.coefficient
    }

    pub fn uncertainty(&self) -> Option<&CoefficientUncertainty> {
        self.uncertainty.as_ref()
    }

    pub fn derivation_method(&self) -> TermDerivationMethod {
        self.derivation_method
    }

    pub fn evidence_experiment(&self) -> Option<&EvidenceExperimentId> {
        self.evidence_experiment.as_ref()
    }
}

/// A complete typed cost model: immutable identity plus one term per known kind.
///
/// Fields are private and the constructor is `pub(crate)`, so a production
/// `CostModel` can only be built inside this crate (in practice, by this module from
/// stored rows, and by the calibration and cost-model modules that resolve models
/// through this repository). External code gets models through the repository, never
/// by assembling one from primitives.
///
/// An incomplete model (fewer kinds than [`TokenKind::ALL`]) is representable and
/// honest: the fail-closed conversion consumer (`aub-ai3.2`) decides what an absent
/// kind means, and an unestablishable coefficient is an unavailable fact, not a zero.
/// What the constructor refuses is a duplicate kind, because the (model, token kind)
/// pair is the table's uniqueness domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostModel {
    id: CostModelId,
    provider: ProviderKey,
    scope: CostModelScope,
    billing_semantics_id: BillingSemanticsId,
    plan_scope: Option<PlanScope>,
    version: CostModelVersion,
    validity: ValidityInterval,
    published_at: UtcTimestamp,
    provenance: ModelProvenance,
    terms: Vec<CostModelTerm>,
}

impl CostModel {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: CostModelId,
        provider: ProviderKey,
        scope: CostModelScope,
        billing_semantics_id: BillingSemanticsId,
        plan_scope: Option<PlanScope>,
        version: CostModelVersion,
        validity: ValidityInterval,
        published_at: UtcTimestamp,
        provenance: ModelProvenance,
        terms: Vec<CostModelTerm>,
    ) -> Result<Self, Error> {
        let mut seen = std::collections::BTreeSet::new();
        for term in &terms {
            if !seen.insert(token_kind_to_str(term.token_kind())) {
                return Err(Error::Store(format!(
                    "cost model '{}' carries two terms for '{}'",
                    id.as_str(),
                    token_kind_to_str(term.token_kind())
                )));
            }
        }
        Ok(Self {
            id,
            provider,
            scope,
            billing_semantics_id,
            plan_scope,
            version,
            validity,
            published_at,
            provenance,
            terms,
        })
    }

    /// The semantic identifier, the one provenance manifests name this model by.
    pub fn id(&self) -> &CostModelId {
        &self.id
    }

    pub fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    pub fn scope(&self) -> &CostModelScope {
        &self.scope
    }

    pub fn billing_semantics_id(&self) -> &BillingSemanticsId {
        &self.billing_semantics_id
    }

    pub fn plan_scope(&self) -> Option<&PlanScope> {
        self.plan_scope.as_ref()
    }

    pub fn version(&self) -> &CostModelVersion {
        &self.version
    }

    pub fn validity(&self) -> &ValidityInterval {
        &self.validity
    }

    pub fn published_at(&self) -> UtcTimestamp {
        self.published_at
    }

    pub fn provenance(&self) -> &ModelProvenance {
        &self.provenance
    }

    /// The term for one token kind, when the model carries one.
    pub fn term(&self, kind: TokenKind) -> Option<&CostModelTerm> {
        self.terms.iter().find(|term| term.token_kind() == kind)
    }

    /// Every term, in no guaranteed order. Uniqueness of the (model, token kind)
    /// pair is guaranteed by the constructor and by the table constraint.
    pub fn terms(&self) -> &[CostModelTerm] {
        &self.terms
    }
}

/// A lifecycle event row's SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LifecycleEventId(i64);

impl LifecycleEventId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

fn token_kind_to_str(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Input => "input",
        TokenKind::Output => "output",
        TokenKind::CacheRead => "cache_read",
        TokenKind::CacheWrite => "cache_write",
    }
}

fn token_kind_from_str(s: &str) -> Result<TokenKind, Error> {
    match s {
        "input" => Ok(TokenKind::Input),
        "output" => Ok(TokenKind::Output),
        "cache_read" => Ok(TokenKind::CacheRead),
        "cache_write" => Ok(TokenKind::CacheWrite),
        other => Err(Error::Store(format!("unknown token kind: '{other}'"))),
    }
}

fn digest_to_hex(digest: u64) -> String {
    format!("{digest:016x}")
}

fn digest_from_hex(text: &str) -> Result<u64, Error> {
    u64::from_str_radix(text, 16)
        .map_err(|e| Error::Store(format!("malformed provenance digest '{text}': {e}")))
}

/// The identity columns of a `cost_model` row, for `SELECT` statements.
const MODEL_IDENTITY_COLUMNS: &str = "cost_model_id, provider, scope_kind, model_id, \
     billing_semantics_id, plan_scope, version, valid_from, valid_until, published_at, \
     provenance_digest, provenance_input_count";

/// The term columns of a `cost_model_term` row, for `SELECT` statements.
const TERM_COLUMNS: &str = "token_kind, credits_per_token_micros, uncertainty_low_micros, \
     uncertainty_high_micros, derivation_method, evidence_experiment";

/// Reads one typed column, mapping the driver error into the store vocabulary so
/// row readers can stay `Result<_, Error>` end to end.
fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T, Error> {
    row.get::<_, T>(index)
        .map_err(|e| Error::Store(format!("cannot read column {index}: {e}")))
}

fn term_from_row(row: &Row<'_>) -> Result<CostModelTerm, Error> {
    let token_kind = token_kind_from_str(&get::<String>(row, 0)?)?;
    let coefficient = CreditsPerToken::from_micros_per_million_tokens(get::<i64>(row, 1)?);
    let low = get::<Option<i64>>(row, 2)?;
    let high = get::<Option<i64>>(row, 3)?;
    let uncertainty = match (low, high) {
        (Some(low), Some(high)) => Some(CoefficientUncertainty::new(
            CreditsPerToken::from_micros_per_million_tokens(low),
            CreditsPerToken::from_micros_per_million_tokens(high),
        )?),
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(Error::Store(
                "cost_model_term stores uncertainty bounds only as a pair".into(),
            ));
        }
    };
    let derivation_method = get::<String>(row, 4)?.parse::<TermDerivationMethod>()?;
    let evidence_experiment = get::<Option<String>>(row, 5)?.map(EvidenceExperimentId::new);
    Ok(CostModelTerm::new(
        token_kind,
        coefficient,
        uncertainty,
        derivation_method,
        evidence_experiment,
    ))
}

fn scope_from_row(kind: &str, model_id: Option<String>) -> Result<CostModelScope, Error> {
    match kind {
        "model_class" => Ok(CostModelScope::ModelClass),
        "model" => Ok(CostModelScope::Model(ModelId::new(model_id.ok_or_else(
            || Error::Store("a model-scoped cost_model has no model_id".into()),
        )?))),
        other => Err(Error::Store(format!("unknown scope kind '{other}'"))),
    }
}

fn model_from_row(row: &Row<'_>) -> Result<CostModel, Error> {
    let id = CostModelId::new(get::<String>(row, 0)?);
    let provider = ProviderKey::new(get::<String>(row, 1)?);
    let scope_kind = get::<String>(row, 2)?;
    let scope = scope_from_row(&scope_kind, get::<Option<String>>(row, 3)?)?;
    let billing_semantics_id = BillingSemanticsId::new(get::<String>(row, 4)?);
    let plan_scope = get::<Option<String>>(row, 5)?.map(PlanScope::new);
    let version = CostModelVersion::new(get::<String>(row, 6)?);
    let validity = ValidityInterval::new(
        UtcTimestamp::from_unix_nanos(get::<i64>(row, 7)?),
        UtcTimestamp::from_unix_nanos(get::<i64>(row, 8)?),
    )?;
    let published_at = UtcTimestamp::from_unix_nanos(get::<i64>(row, 9)?);
    let provenance = ModelProvenance::from_parts(
        digest_from_hex(&get::<String>(row, 10)?)?,
        get::<i64>(row, 11)?
            .try_into()
            .map_err(|_| Error::Store("provenance input count stored negative".into()))?,
    );
    CostModel::new(
        id,
        provider,
        scope,
        billing_semantics_id,
        plan_scope,
        version,
        validity,
        published_at,
        provenance,
        Vec::new(),
    )
}

/// Maps a store error into the driver error vocabulary at a row boundary, the
/// convention every repository in this module family uses for a domain parse that
/// fails while decoding a stored row.
fn store_error_to_sql(e: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

/// Inserts a model row and its term rows atomically. The model is not yet active:
/// activation is a separate, explicit lifecycle event. Refuses to write anything when
/// a term duplicates a (cost model, token kind) pair or when any CHECK or constraint
/// on the tables is violated.
pub fn insert_model(conn: &mut Connection, model: &CostModel) -> Result<CostModelDbId, Error> {
    let tx = conn.transaction().map_err(|e| {
        Error::Store(format!(
            "cannot open the cost model insert transaction: {e}"
        ))
    })?;
    let db_id = insert_model_rows(&tx, model)?;
    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the cost model insert: {e}")))?;
    Ok(db_id)
}

fn insert_model_rows(
    tx: &rusqlite::Transaction<'_>,
    model: &CostModel,
) -> Result<CostModelDbId, Error> {
    let db_id: i64 = tx
        .query_row(
            "INSERT INTO cost_model (
                cost_model_id, provider, scope_kind, model_id, billing_semantics_id,
                plan_scope, version, valid_from, valid_until, published_at,
                provenance_digest, provenance_input_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            RETURNING id",
            params![
                model.id().as_str(),
                model.provider().as_str(),
                match model.scope() {
                    CostModelScope::ModelClass => "model_class",
                    CostModelScope::Model(_) => "model",
                },
                match model.scope() {
                    CostModelScope::ModelClass => None,
                    CostModelScope::Model(model_id) => Some(model_id.as_str()),
                },
                model.billing_semantics_id().as_str(),
                model.plan_scope().map(|s| s.as_str()),
                model.version().as_str(),
                model.validity().valid_from().unix_nanos(),
                model.validity().valid_until().unix_nanos(),
                model.published_at().unix_nanos(),
                digest_to_hex(model.provenance().digest()),
                i64::try_from(model.provenance().input_count())
                    .map_err(|_| Error::Store("provenance input count out of i64 range".into()))?,
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| Error::Store(format!("cannot insert the cost_model row: {e}")))?;

    for term in model.terms() {
        insert_term_row(tx, db_id, term)?;
    }
    Ok(CostModelDbId::new(db_id))
}

fn insert_term_row(
    tx: &rusqlite::Transaction<'_>,
    cost_model_db_id: i64,
    term: &CostModelTerm,
) -> Result<(), Error> {
    let (low, high) = match term.uncertainty() {
        Some(uncertainty) => (
            Some(uncertainty.lower().micros_per_million_tokens()),
            Some(uncertainty.upper().micros_per_million_tokens()),
        ),
        None => (None, None),
    };
    tx.execute(
        "INSERT INTO cost_model_term (
            cost_model_id, token_kind, credits_per_token_micros,
            uncertainty_low_micros, uncertainty_high_micros,
            derivation_method, evidence_experiment
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            cost_model_db_id,
            token_kind_to_str(term.token_kind()),
            term.coefficient().micros_per_million_tokens(),
            low,
            high,
            term.derivation_method().as_str(),
            term.evidence_experiment().map(|id| id.as_str()),
        ],
    )
    .map_err(|e| Error::Store(format!("cannot insert the cost_model_term row: {e}")))?;
    Ok(())
}

/// Loads a model by its semantic identifier, with every stored term.
pub fn load_by_semantic_id(
    conn: &Connection,
    id: &CostModelId,
) -> Result<Option<CostModel>, Error> {
    load_model(conn, "WHERE cost_model_id = ?1", params![id.as_str()])
}

/// The model active at a past instant: the `cost_model_id` of the latest lifecycle
/// row at or before that instant, with its terms.
///
/// Returns `None` before the first activation. Ties at one instant are broken by
/// event row id, the later row winning; the repository's activation rule makes ties
/// unreachable in practice (a successor must supersede the model active just before
/// its event instant, so two events at the same instant can only both name the same
/// predecessor, and the later-inserted row is the one that speaks).
pub fn load_active_at(conn: &Connection, at: UtcTimestamp) -> Result<Option<CostModel>, Error> {
    load_model(
        conn,
        "WHERE id = (
            SELECT cost_model_id FROM cost_model_lifecycle
            WHERE event_at <= ?1
            ORDER BY event_at DESC, id DESC
            LIMIT 1
        )",
        params![at.unix_nanos()],
    )
}

fn load_model(
    conn: &Connection,
    where_clause: &str,
    params: impl rusqlite::Params,
) -> Result<Option<CostModel>, Error> {
    // The rowid is selected last so `model_from_row` keeps reading the identity
    // columns at their fixed offsets.
    let sql = format!("SELECT {MODEL_IDENTITY_COLUMNS}, id FROM cost_model {where_clause}");
    let loaded = conn
        .query_row(&sql, params, |row| {
            let model = model_from_row(row).map_err(store_error_to_sql)?;
            let db_id = CostModelDbId::new(row.get::<_, i64>(12)?);
            Ok((db_id, model))
        })
        .optional()
        .map_err(|e| Error::Store(format!("cannot load the cost_model row: {e}")))?;
    let Some((db_id, mut model)) = loaded else {
        return Ok(None);
    };
    let term_sql = format!(
        "SELECT {TERM_COLUMNS} FROM cost_model_term WHERE cost_model_id = ?1 ORDER BY token_kind"
    );
    let mut stmt = conn
        .prepare(&term_sql)
        .map_err(|e| Error::Store(format!("cannot prepare the cost_model_term query: {e}")))?;
    let terms = stmt
        .query_map([db_id.value()], |row| {
            term_from_row(row).map_err(store_error_to_sql)
        })
        .map_err(|e| Error::Store(format!("cannot query cost_model_term rows: {e}")))?;
    let mut stored = Vec::new();
    for term in terms {
        let term =
            term.map_err(|e| Error::Store(format!("cannot read a cost_model_term row: {e}")))?;
        stored.push(term);
    }
    model.terms = stored;
    Ok(Some(model))
}

/// Records `model` becoming active at `event_at`.
///
/// One self-consistent chain is enforced here, not merely described: the first
/// activation on an empty history has no predecessor, and every later activation
/// must name, as its superseded model, the model that was active just before
/// `event_at`. A call that violates either rule is refused before anything is
/// written, so the active-at query can never fork. The model rows and the lifecycle
/// row are written in the same short transaction.
pub fn activate(
    conn: &mut Connection,
    model: &CostModel,
    event_at: UtcTimestamp,
    supersedes: Option<&CostModelId>,
) -> Result<LifecycleEventId, Error> {
    let tx = conn
        .transaction()
        .map_err(|e| Error::Store(format!("cannot open the activation transaction: {e}")))?;

    let active_before = load_active_at(&tx, instant_before(event_at))?;
    match (active_before.as_ref(), supersedes) {
        (None, None) => {}
        (None, Some(_)) => {
            return Err(Error::Store(format!(
                "first activation of '{}' names a predecessor that is not active",
                model.id().as_str()
            )));
        }
        (Some(active), None) => {
            return Err(Error::Store(format!(
                "activation of '{}' at {}ns must supersede the active model '{}'",
                model.id().as_str(),
                event_at.unix_nanos(),
                active.id().as_str()
            )));
        }
        (Some(active), Some(supersedes)) => {
            if active.id() != supersedes {
                return Err(Error::Store(format!(
                    "activation of '{}' supersedes '{}' but '{}' is active at that instant",
                    model.id().as_str(),
                    supersedes.as_str(),
                    active.id().as_str()
                )));
            }
            if model.id() == supersedes {
                return Err(Error::Store(format!(
                    "activation of '{}' would supersede itself",
                    model.id().as_str()
                )));
            }
        }
    }

    let db_id = insert_model_rows(&tx, model)?;

    let supersedes_db_id: Option<i64> = match supersedes {
        None => None,
        Some(id) => Some(
            tx.query_row(
                "SELECT id FROM cost_model WHERE cost_model_id = ?1",
                [id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| {
                Error::Store(format!(
                    "cannot resolve the superseded model '{}': {e}",
                    id.as_str()
                ))
            })?,
        ),
    };

    let event_kind = if supersedes.is_some() {
        "supersession"
    } else {
        "activation"
    };
    let event_id: i64 = tx
        .query_row(
            "INSERT INTO cost_model_lifecycle (
                cost_model_id, event_kind, event_at, supersedes_model_id
            ) VALUES (?1, ?2, ?3, ?4)
            RETURNING id",
            params![
                db_id.value(),
                event_kind,
                event_at.unix_nanos(),
                supersedes_db_id
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| Error::Store(format!("cannot insert the lifecycle event: {e}")))?;

    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the activation: {e}")))?;
    Ok(LifecycleEventId::new(event_id))
}

/// One nanosecond before `at`, the instant `load_active_at` uses to read the state a
/// new event at `at` is replacing.
fn instant_before(at: UtcTimestamp) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(at.unix_nanos().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provenance::{QuerySemantics, WitnessId};
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-cost-model-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_conn() -> (ScratchDir, Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("cost_model.db");
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    fn ts(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    /// A model covering all four known token kinds, with one term carrying an
    /// uncertainty interval and one carrying an evidence experiment.
    fn complete_model(id: &str, validity: ValidityInterval) -> CostModel {
        let manifest = ProvenanceManifest::new(
            [EvidenceId::new("exp-2026-a")],
            [WitnessId::CostModel(CostModelId::new(id))],
            QuerySemantics::new("model", "activation"),
        );
        CostModel::new(
            CostModelId::new(id),
            ProviderKey::new("test-provider"),
            CostModelScope::ModelClass,
            BillingSemanticsId::new("test-billing-v1"),
            Some(PlanScope::new("plus")),
            CostModelVersion::new("1.0"),
            validity,
            ts(5_000),
            ModelProvenance::from_manifest(&manifest),
            vec![
                CostModelTerm::new(
                    TokenKind::Input,
                    CreditsPerToken::from_micros_per_million_tokens(8),
                    None,
                    TermDerivationMethod::PublishedBillingSemantics,
                    None,
                ),
                CostModelTerm::new(
                    TokenKind::Output,
                    CreditsPerToken::from_micros_per_million_tokens(40),
                    Some(
                        CoefficientUncertainty::new(
                            CreditsPerToken::from_micros_per_million_tokens(30),
                            CreditsPerToken::from_micros_per_million_tokens(50),
                        )
                        .unwrap(),
                    ),
                    TermDerivationMethod::ControlledExperiment,
                    Some(EvidenceExperimentId::new("exp-2026-a")),
                ),
                CostModelTerm::new(
                    TokenKind::CacheRead,
                    CreditsPerToken::from_micros_per_million_tokens(1),
                    None,
                    TermDerivationMethod::Assumed,
                    None,
                ),
                CostModelTerm::new(
                    TokenKind::CacheWrite,
                    CreditsPerToken::from_micros_per_million_tokens(5),
                    None,
                    TermDerivationMethod::PublishedBillingSemantics,
                    None,
                ),
            ],
        )
        .unwrap()
    }

    fn validity(from_nanos: i64, until_nanos: i64) -> ValidityInterval {
        ValidityInterval::new(ts(from_nanos), ts(until_nanos)).unwrap()
    }

    /// Round trip: an inserted model comes back equal, terms included.
    #[test]
    fn inserted_model_round_trips_with_terms() {
        let (_scratch, mut conn) = fixture_conn();
        let model = complete_model("cm-roundtrip", validity(1_000, 9_000));
        let db_id = insert_model(&mut conn, &model).unwrap();
        assert!(db_id.value() >= 1);

        let loaded = load_by_semantic_id(&conn, model.id()).unwrap().unwrap();
        // Terms come back in the table's own token_kind order, so the comparison
        // sorts both sides rather than depending on construction order.
        let mut expected = model.terms().to_vec();
        let mut actual = loaded.terms().to_vec();
        expected.sort_by_key(|term| token_kind_to_str(term.token_kind()));
        actual.sort_by_key(|term| token_kind_to_str(term.token_kind()));
        assert_eq!(actual, expected);
        assert_eq!(loaded.terms().len(), TokenKind::ALL.len());
        assert_eq!(
            loaded
                .term(TokenKind::Output)
                .unwrap()
                .uncertainty()
                .unwrap()
                .lower(),
            CreditsPerToken::from_micros_per_million_tokens(30)
        );
        assert_eq!(
            loaded
                .term(TokenKind::Output)
                .unwrap()
                .evidence_experiment()
                .unwrap()
                .as_str(),
            "exp-2026-a"
        );

        // The loaded provenance verifies against exactly the evidence set it was
        // built from.
        let mut expansion = BTreeSet::new();
        expansion.insert(EvidenceId::new("exp-2026-a"));
        assert!(loaded.provenance().verify_expansion(&expansion));
        let mut wrong = BTreeSet::new();
        wrong.insert(EvidenceId::new("some-other-evidence"));
        assert!(!loaded.provenance().verify_expansion(&wrong));
    }

    /// The model active at a past instant is the latest activation at or before it,
    /// across three activations, and a superseded model stays queryable.
    #[test]
    fn point_in_time_activation_query_across_three_activations() {
        let (_scratch, mut conn) = fixture_conn();
        let a = complete_model("cm-a", validity(1_000, 9_000));
        let b = complete_model("cm-b", validity(1_000, 9_000));
        let c = complete_model("cm-c", validity(1_000, 9_000));

        activate(&mut conn, &a, ts(1_000), None).unwrap();
        activate(&mut conn, &b, ts(2_000), Some(a.id())).unwrap();
        activate(&mut conn, &c, ts(3_000), Some(b.id())).unwrap();

        assert!(load_active_at(&conn, ts(999)).unwrap().is_none());
        assert_eq!(
            load_active_at(&conn, ts(1_000)).unwrap().unwrap().id(),
            a.id()
        );
        assert_eq!(
            load_active_at(&conn, ts(1_999)).unwrap().unwrap().id(),
            a.id()
        );
        assert_eq!(
            load_active_at(&conn, ts(2_000)).unwrap().unwrap().id(),
            b.id()
        );
        assert_eq!(
            load_active_at(&conn, ts(2_999)).unwrap().unwrap().id(),
            b.id()
        );
        assert_eq!(
            load_active_at(&conn, ts(3_000)).unwrap().unwrap().id(),
            c.id()
        );
        assert_eq!(
            load_active_at(&conn, ts(99_000)).unwrap().unwrap().id(),
            c.id()
        );

        // What makes a past report reproducible is that the whole resolved model
        // differs by knowledge instant: a report rendered with a knowledge time in
        // B's span resolves B, and one rendered today resolves C, and either stays
        // stable however often it is re-resolved.
        let at_b = load_active_at(&conn, ts(2_500)).unwrap().unwrap();
        let today = load_active_at(&conn, ts(99_000)).unwrap().unwrap();
        assert_ne!(
            at_b.id(),
            today.id(),
            "two knowledge instants must resolve different models"
        );
        assert_eq!(
            at_b.version(),
            b.version(),
            "the historical resolution must be stable: same instant, same model"
        );
        assert_eq!(
            load_active_at(&conn, ts(2_500)).unwrap().unwrap(),
            at_b,
            "re-resolving the same instant returns the identical model"
        );

        // Superseded models remain queryable and complete.
        for id in ["cm-a", "cm-b", "cm-c"] {
            let loaded = load_by_semantic_id(&conn, &CostModelId::new(id))
                .unwrap()
                .unwrap();
            assert_eq!(loaded.terms().len(), TokenKind::ALL.len());
        }
    }

    /// An activation that does not supersede the active model, or that names a
    /// predecessor that is not active, is refused before anything is written.
    #[test]
    fn activation_chain_invariants_are_enforced() {
        let (_scratch, mut conn) = fixture_conn();
        let a = complete_model("cm-chain-a", validity(1_000, 9_000));
        let b = complete_model("cm-chain-b", validity(1_000, 9_000));
        let c = complete_model("cm-chain-c", validity(1_000, 9_000));

        // First activation must have no predecessor.
        assert!(activate(&mut conn, &a, ts(1_000), Some(b.id())).is_err());

        activate(&mut conn, &a, ts(1_000), None).unwrap();

        // A successor must supersede the active model.
        assert!(activate(&mut conn, &b, ts(2_000), None).is_err());
        // ... and must not supersede a model that is not active.
        assert!(activate(&mut conn, &b, ts(2_000), Some(c.id())).is_err());
        // ... and must not supersede itself (an already-activated model re-activated).
        assert!(activate(&mut conn, &a, ts(2_000), Some(a.id())).is_err());

        // The refused writes left no trace: no lifecycle row past the first one.
        activate(&mut conn, &b, ts(2_000), Some(a.id())).unwrap();
        assert_eq!(
            load_active_at(&conn, ts(2_000)).unwrap().unwrap().id(),
            b.id()
        );
    }

    /// A direct UPDATE or DELETE against any of the three tables fails at the
    /// database: immutability is enforced by triggers, not by repository politeness.
    ///
    /// SQLite triggers fire per matched row, so each table is populated before the
    /// attempt; an UPDATE that matches zero rows would succeed silently even with the
    /// trigger in place, which is why the lifecycle assertions run after an
    /// activation.
    #[test]
    fn direct_update_and_delete_are_refused_by_the_database() {
        let (_scratch, mut conn) = fixture_conn();
        let model = complete_model("cm-immutable", validity(1_000, 9_000));
        // Activation inserts the model and term rows itself; a second insert would
        // trip the semantic-id uniqueness constraint.
        activate(&mut conn, &model, ts(1_000), None).unwrap();

        let update_err = conn
            .execute("UPDATE cost_model SET provider = 'tampered'", [])
            .unwrap_err();
        assert!(
            update_err.to_string().contains("immutable"),
            "unexpected error: {update_err}"
        );

        let delete_err = conn.execute("DELETE FROM cost_model", []).unwrap_err();
        assert!(delete_err.to_string().contains("immutable"));

        let term_update_err = conn
            .execute(
                "UPDATE cost_model_term SET credits_per_token_micros = 0",
                [],
            )
            .unwrap_err();
        assert!(term_update_err.to_string().contains("immutable"));

        let lifecycle_update_err = conn
            .execute("UPDATE cost_model_lifecycle SET event_at = 1", [])
            .unwrap_err();
        assert!(lifecycle_update_err.to_string().contains("append-only"));

        let lifecycle_delete_err = conn
            .execute("DELETE FROM cost_model_lifecycle", [])
            .unwrap_err();
        assert!(lifecycle_delete_err.to_string().contains("append-only"));

        // The model row is still there, untouched.
        assert!(load_by_semantic_id(&conn, model.id()).unwrap().is_some());
    }

    /// The UNIQUE constraint on (cost model, token kind) is the final authority: a
    /// second term for the same kind fails at the database, not in Rust.
    #[test]
    fn duplicate_term_for_one_kind_fails_at_the_database() {
        let (_scratch, mut conn) = fixture_conn();
        let model = complete_model("cm-unique", validity(1_000, 9_000));
        insert_model(&mut conn, &model).unwrap();

        let err = conn
            .execute(
                "INSERT INTO cost_model_term (
                    cost_model_id, token_kind, credits_per_token_micros,
                    uncertainty_low_micros, uncertainty_high_micros,
                    derivation_method, evidence_experiment
                ) VALUES (
                    (SELECT id FROM cost_model WHERE cost_model_id = 'cm-unique'),
                    'input', 999, NULL, NULL, 'assumed', NULL
                )",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE"),
            "expected a UNIQUE violation, got: {err}"
        );
    }

    /// A term row cannot reference a cost model that does not exist: foreign keys
    /// are enforced on this connection.
    #[test]
    fn term_row_cannot_reference_a_missing_model() {
        let (_scratch, mut conn) = fixture_conn();
        let err = conn
            .execute(
                "INSERT INTO cost_model_term (
                    cost_model_id, token_kind, credits_per_token_micros,
                    uncertainty_low_micros, uncertainty_high_micros,
                    derivation_method, evidence_experiment
                ) VALUES (9999, 'input', 8, NULL, NULL, 'assumed', NULL)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "expected a foreign key violation, got: {err}"
        );
    }

    /// Activation and supersession live in rows, never in a column on the model:
    /// the cost_model column set carries no such column, and the lifecycle table
    /// holds one event row per transition.
    #[test]
    fn lifecycle_is_rows_not_columns() {
        let (_scratch, mut conn) = fixture_conn();
        let a = complete_model("cm-rows-a", validity(1_000, 9_000));
        let b = complete_model("cm-rows-b", validity(1_000, 9_000));
        activate(&mut conn, &a, ts(1_000), None).unwrap();
        activate(&mut conn, &b, ts(2_000), Some(a.id())).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(cost_model)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            !columns
                .iter()
                .any(|c| c.contains("activ") || c.contains("supersed")),
            "cost_model carries a lifecycle column: {columns:?}"
        );

        let events: Vec<(String, i64)> = conn
            .prepare("SELECT event_kind, cost_model_id FROM cost_model_lifecycle ORDER BY event_at")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "activation");
        assert_eq!(events[1].0, "supersession");
    }

    /// Both interval constructors reject an inverted range rather than normalising
    /// it, and the derivation-method vocabulary round-trips.
    #[test]
    fn inverted_intervals_are_rejected_and_vocabulary_round_trips() {
        assert!(ValidityInterval::new(ts(2_000), ts(1_000)).is_err());
        assert!(ValidityInterval::new(ts(1_000), ts(1_000)).is_ok());
        assert!(
            CoefficientUncertainty::new(
                CreditsPerToken::from_micros_per_million_tokens(50),
                CreditsPerToken::from_micros_per_million_tokens(30),
            )
            .is_err()
        );
        for method in [
            TermDerivationMethod::PublishedBillingSemantics,
            TermDerivationMethod::ControlledExperiment,
            TermDerivationMethod::Assumed,
        ] {
            let parsed: TermDerivationMethod = method.as_str().parse().unwrap();
            assert_eq!(parsed, method);
        }
        assert!("nonsense".parse::<TermDerivationMethod>().is_err());
    }
}
