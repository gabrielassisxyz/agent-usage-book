//! Construction and atomic publication of the status projection (`aub-me5.5`).
//!
//! The projection is a disposable, one-way derived file: a latency cache of
//! the durable meter state `aub status` needs, written after every committed
//! transaction that can change status-visible meter state. Correctness never
//! depends on its existence, only latency does, which is why every failure to
//! publish defers rather than fails the caller: the database commit is the
//! event that matters, and a deferred publication is healed by the next one.
//!
//! The projection stores the inputs to the freshness state machine and never
//! its result. There is no stored freshness boolean anywhere in it, so a dead
//! sampler makes the status line visibly age instead of remaining confidently
//! fresh. It holds no calibration constant, no computed spend, no valuation,
//! no credential material, and no raw provider body; the schema test over a
//! produced file enforces exactly that field set.
//!
//! Publication ordering is the invariant that gives the file its meaning: the
//! database commit always precedes it. A crash may leave the projection older
//! than SQLite and must never leave it claiming evidence newer than the
//! database. Because every projection-relevant transaction advances the ledger
//! generation inside the same transaction as its state change, the generation
//! the file records, read in the same snapshot as the state, is a statement
//! about content: a projection ahead of the database is a corruption report,
//! not a race.
//!
//! May not depend on:
//! - provider adapters
//! - terminal-formatting crates

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::domain::attempt::{AttemptId, AttemptOutcome};
use crate::domain::time::{MeasurementBasis, UtcTimestamp};
use crate::domain::window::{QuantizationSemantics, WindowScope};
use crate::error::Error;
use crate::store::ledger_generation::Generation;
use crate::store::meter_attempt::failure_class_sql;
use crate::store::meter_evidence::{measurement_basis_sql, quantization_sql};

pub mod build;
pub mod reader;

/// The ledger generation recorded in a projection file's text, or `None` when
/// the text is not a projection file this schema wrote. One reader for the
/// format this module owns, so test surfaces and doctor compare against the
/// file by parsing it rather than by grepping it.
pub fn recorded_generation(text: &str) -> Option<u64> {
    let document: Value = serde_json::from_str(text).ok()?;
    document.get("ledger_generation").and_then(Value::as_u64)
}

/// The schema version of the projection file format, written into every file
/// so a reader can refuse an older or newer one instead of misreading it.
/// Bumped only by a format change a reader must be told about.
pub const PROJECTION_SCHEMA_VERSION: u32 = 1;

/// The file name of the projection inside the state directory, next to the
/// ledger it describes. Status locates it from the state directory alone,
/// with no SQLite open (PLAN.md section 16.2).
pub const PROJECTION_FILE_NAME: &str = "projection";

/// The projection file's path inside a state directory: the one derivation a
/// reader or a publisher shares, so the file is named in exactly one place.
pub fn projection_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(PROJECTION_FILE_NAME)
}

/// The outcome of one publication attempt.
///
/// [`Publication::Published`] carries the generation the file now records.
/// [`Publication::Deferred`] names why nothing was written; the projection on
/// disk is then older than the database, which is always the safe direction,
/// and the next publication heals it. Publication never fails the caller: the
/// commit it follows is already durable, and reporting the deferral is the
/// caller's to surface, not something to invent a failure exit for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publication {
    Published { generation: Generation },
    Deferred { reason: String },
}

impl Publication {
    /// The generation the file now records, when publication succeeded.
    pub fn published_generation(&self) -> Option<Generation> {
        match self {
            Publication::Published { generation } => Some(*generation),
            Publication::Deferred { .. } => None,
        }
    }
}

/// Builds the projection from one read snapshot and publishes it atomically.
///
/// The generation and the state are read on `conn` inside one transaction, so
/// the file records exactly the generation of the state it contains. The
/// caller has already committed; a read here can only see durable state, which
/// is what keeps the file from ever claiming evidence newer than the database.
///
/// The write sequence is write a temporary file, fsync it, atomically rename
/// it over the target, and fsync the containing directory where the platform
/// supports it, so a reader never observes a torn file (PLAN.md sections
/// 16.1, 34.27).
pub fn publish(conn: &Connection, target_path: &Path) -> Publication {
    match publish_checked(conn, target_path) {
        Ok(generation) => Publication::Published { generation },
        Err(error) => Publication::Deferred {
            reason: error.to_string(),
        },
    }
}

fn publish_checked(conn: &Connection, target_path: &Path) -> Result<Generation, Error> {
    let snapshot = conn
        .unchecked_transaction()
        .map_err(|error| Error::Store(format!("cannot open the publication snapshot: {error}")))?;
    let generation = crate::store::ledger_generation::current(&snapshot)?;
    let states = crate::store::projection_source::account_meter_states(&snapshot)?;
    drop(snapshot);

    let projection = build::projection(generation, &states);
    let bytes = serialize(&projection);
    atomic_write(target_path, &bytes)?;
    Ok(generation)
}

/// The serialized projection: canonical JSON, field order fixed by the
/// serializer's own key ordering, no wall clock anywhere in the content. The
/// absence of a wall clock is what makes republishing from unchanged database
/// state reproduce the file byte for byte.
fn serialize(projection: &Projection) -> Vec<u8> {
    let document = projection.to_json();
    serde_json::to_vec(&document)
        .expect("projection JSON serialization cannot fail on in-memory values")
}

/// The projection's file format, one-to-one with what the design lists
/// (PLAN.md section 16.1) and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub ledger_generation: Generation,
    pub accounts: Vec<ProjectedAccount>,
}

/// One account's projected state: the logical identity, the last successful
/// observation with its windows, and the latest attempt with its terminal
/// outcome or the fact that it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedAccount {
    pub account_id: crate::store::account::AccountId,
    pub logical_name: String,
    pub provider: String,
    pub last_successful_observation: Option<SuccessfulObservation>,
    pub latest_attempt: Option<LatestAttempt>,
}

/// The last good observation and the windows it reported, exactly as the
/// provider expressed them: stored inputs, never computed spend or valuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessfulObservation {
    pub observation_id: crate::store::meter_evidence::ObservationRowId,
    /// The provider's own measurement timestamp, when it documented one. Part
    /// of the freshness machine's input, alongside `received_at` and the
    /// basis that says which of the two the staleness arithmetic uses.
    pub provider_observed_at: Option<UtcTimestamp>,
    pub received_at: UtcTimestamp,
    pub measurement_basis: MeasurementBasis,
    pub windows: Vec<ProjectedWindow>,
}

/// One provider-reported quota constraint, stored exactly as expressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedWindow {
    pub semantic_key: String,
    pub scope: WindowScope,
    pub quota_used_ppm: crate::domain::quota::QuotaUsed,
    pub reported_resolution_ppm: crate::domain::window::ReportedResolution,
    pub quantization: QuantizationSemantics,
    pub resets_at: UtcTimestamp,
    pub nominal_duration_nanos: crate::domain::window::NominalWindowDuration,
}

/// The newest attempt of an account: its identity, when it started, the
/// credential context it ran under, and its terminal outcome or the fact that
/// it has none. The credential context is an identifier, not credential
/// material; the design requires it and forbids the material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestAttempt {
    pub attempt_id: AttemptId,
    pub request_started_at: UtcTimestamp,
    pub credential_context_id: Option<String>,
    pub result: Option<TerminalOutcome>,
}

/// One terminal outcome: when it completed, and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutcome {
    pub completed_at: UtcTimestamp,
    pub outcome: AttemptOutcome,
}

impl Projection {
    /// The file document: the schema version plus the carried state. Field
    /// names are the schema test's contract; no key outside them is written.
    /// `pub(crate)` so the reader's round-trip test drives the same
    /// serializer the publisher uses.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "schema_version": PROJECTION_SCHEMA_VERSION,
            "ledger_generation": self.ledger_generation.value(),
            "accounts": self
                .accounts
                .iter()
                .map(ProjectedAccount::to_json)
                .collect::<Vec<Value>>(),
        })
    }
}

impl ProjectedAccount {
    fn to_json(&self) -> Value {
        json!({
            "account_id": self.account_id.value(),
            "logical_name": self.logical_name,
            "provider": self.provider,
            "last_successful_observation": self
                .last_successful_observation
                .as_ref()
                .map(SuccessfulObservation::to_json),
            "latest_attempt": self.latest_attempt.as_ref().map(LatestAttempt::to_json),
        })
    }
}

impl SuccessfulObservation {
    fn to_json(&self) -> Value {
        json!({
            "observation_id": self.observation_id.value(),
            "provider_observed_at_nanos": self.provider_observed_at.map(|t| t.unix_nanos()),
            "received_at_nanos": self.received_at.unix_nanos(),
            "measurement_basis": measurement_basis_sql::as_sql(self.measurement_basis),
            "windows": self
                .windows
                .iter()
                .map(ProjectedWindow::to_json)
                .collect::<Vec<Value>>(),
        })
    }
}

impl ProjectedWindow {
    fn to_json(&self) -> Value {
        json!({
            "semantic_key": self.semantic_key,
            "scope_kind": scope_kind_sql(&self.scope),
            "scoped_model": scoped_model(&self.scope),
            "quota_used_ppm": self.quota_used_ppm.as_ppm().get(),
            "reported_resolution_ppm": self.reported_resolution_ppm.as_ppm().get(),
            "quantization": quantization_sql::as_sql(self.quantization),
            "resets_at_nanos": self.resets_at.unix_nanos(),
            "nominal_duration_nanos": self.nominal_duration_nanos.as_nanos(),
        })
    }
}

impl LatestAttempt {
    fn to_json(&self) -> Value {
        json!({
            "attempt_id": self.attempt_id.value(),
            "request_started_at_nanos": self.request_started_at.unix_nanos(),
            "credential_context_id": self.credential_context_id,
            "result": self.result.as_ref().map(TerminalOutcome::to_json),
        })
    }
}

impl TerminalOutcome {
    fn to_json(&self) -> Value {
        let (outcome, failure_class) = match &self.outcome {
            AttemptOutcome::Success => ("success", None),
            AttemptOutcome::AuthRequired => ("auth_required", None),
            AttemptOutcome::Unreachable(class) => {
                ("unreachable", Some(failure_class_sql::as_sql(class)))
            }
        };
        json!({
            "completed_at_nanos": self.completed_at.unix_nanos(),
            "outcome": outcome,
            "failure_class": failure_class,
        })
    }
}

/// The scope spelling, from the same single definition the store writes.
/// `scoped_model` travels alongside it rather than inside it, so the file
/// stays a flat, hand-parseable shape.
fn scope_kind_sql(scope: &WindowScope) -> &'static str {
    match scope {
        WindowScope::AccountWide => "account_wide",
        WindowScope::ModelSpecific(_) => "model_specific",
    }
}

fn scoped_model(scope: &WindowScope) -> Option<String> {
    match scope {
        WindowScope::AccountWide => None,
        WindowScope::ModelSpecific(model) => Some(model.as_str().to_owned()),
    }
}

/// Writes `bytes` to `target` so a reader never observes a torn file, and so
/// the file exists at mode 0600 from the moment it first appears.
///
/// The temporary file is private to this process (`projection.tmp-<pid>`), so
/// two concurrent publishers never share one and never rename each other's
/// partial bytes; each rename publishes a fully written, fsynced file. A
/// crashed process may leave its temporary behind, where it is inert: nothing
/// reads `*.tmp-*`, and the next publication replaces the real file. The
/// rename carries the temporary's mode, so an existing file at a wider mode is
/// repaired as a side effect of being replaced rather than tolerated.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = target.parent().unwrap_or_else(|| Path::new(""));
    let file_name = target
        .file_name()
        .ok_or_else(|| Error::Store(format!("projection target {target:?} has no file name")))?
        .to_string_lossy()
        .into_owned();
    let temporary = parent.join(format!("{file_name}.tmp-{}", std::process::id()));

    let mut file = open_projection_temporary(&temporary)?;
    file.write_all(bytes)
        .map_err(|error| Error::Store(format!("cannot write the projection temporary: {error}")))?;
    file.sync_all()
        .map_err(|error| Error::Store(format!("cannot fsync the projection temporary: {error}")))?;
    drop(file);

    fs::rename(&temporary, target).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        Error::Store(format!("cannot publish the projection file: {error}"))
    })?;
    sync_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
fn open_projection_temporary(path: &Path) -> Result<fs::File, Error> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::Store(format!("cannot create {path:?} at mode 0600: {error}")))?;
    // `.mode(0o600)` binds only a file this call creates; a temporary left by
    // a dead process with a recycled pid keeps its old mode, so the repair
    // policy the store applies to its own files applies here too.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| Error::Store(format!("cannot set {path:?} to mode 0600: {error}")))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_projection_temporary(path: &Path) -> Result<fs::File, Error> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| Error::Store(format!("cannot create {path:?}: {error}")))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), Error> {
    let dir = fs::File::open(path)
        .map_err(|error| Error::Store(format!("cannot open {path:?} to sync it: {error}")))?;
    dir.sync_all()
        .map_err(|error| Error::Store(format!("cannot fsync the directory {path:?}: {error}")))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), Error> {
    // Where the platform cannot fsync a directory, the file's own fsync and
    // the atomic rename are the durability the sequence can state.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::failure::FailureClass;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::window::{ModelId, NominalWindowDuration, ReportedResolution};
    use crate::store::account::AccountId;
    use crate::store::meter_evidence::ObservationRowId;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-projection-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fraction(value: i32) -> QuotaFractionPpm {
        QuotaFractionPpm::new(value).unwrap()
    }

    fn window(key: &str, used_ppm: i32, model: Option<&str>) -> ProjectedWindow {
        ProjectedWindow {
            semantic_key: key.to_string(),
            scope: match model {
                None => WindowScope::AccountWide,
                Some(name) => WindowScope::ModelSpecific(ModelId::new(name.to_string())),
            },
            quota_used_ppm: QuotaUsed::new(fraction(used_ppm)),
            reported_resolution_ppm: ReportedResolution::new(fraction(10_000)).unwrap(),
            quantization: QuantizationSemantics::RoundedToNearest,
            resets_at: UtcTimestamp::from_unix_nanos(9_000),
            nominal_duration_nanos: NominalWindowDuration::from_nanos(18_000_000_000_000),
        }
    }

    fn account(
        id: i64,
        last_success: Option<SuccessfulObservation>,
        latest_attempt: Option<LatestAttempt>,
    ) -> ProjectedAccount {
        ProjectedAccount {
            account_id: AccountId::new(id),
            logical_name: "work".to_string(),
            provider: "anthropic".to_string(),
            last_successful_observation: last_success,
            latest_attempt,
        }
    }

    fn full_projection() -> Projection {
        let success = SuccessfulObservation {
            observation_id: ObservationRowId::new(7),
            provider_observed_at: Some(UtcTimestamp::from_unix_nanos(3_400)),
            received_at: UtcTimestamp::from_unix_nanos(3_500),
            measurement_basis: MeasurementBasis::ProviderObserved,
            windows: vec![
                window("five_hour", 250_000, None),
                window("seven_day", 400_000, Some("claude-model-x")),
            ],
        };
        let attempt = LatestAttempt {
            attempt_id: AttemptId::new(9),
            request_started_at: UtcTimestamp::from_unix_nanos(3_000),
            credential_context_id: Some("credential-context-v1".to_string()),
            result: Some(TerminalOutcome {
                completed_at: UtcTimestamp::from_unix_nanos(4_000),
                outcome: AttemptOutcome::Unreachable(FailureClass::RateLimited {
                    retry_after: None,
                }),
            }),
        };
        Projection {
            ledger_generation: Generation::new(12),
            accounts: vec![
                account(1, Some(success), Some(attempt)),
                account(2, None, None),
            ],
        }
    }

    fn produced_bytes() -> (ScratchDir, PathBuf, Vec<u8>) {
        let scratch = ScratchDir::new();
        let target = scratch.path().join(PROJECTION_FILE_NAME);
        let document = full_projection().to_json();
        let bytes = serde_json::to_vec(&document).unwrap();
        atomic_write(&target, &bytes).unwrap();
        let written = fs::read(&target).unwrap();
        (scratch, target, written)
    }

    // --- schema: exactly the listed fields, over a produced file -------------

    /// The field sets the design lists (PLAN.md section 16.1), asserted over a
    /// produced file. This is a whitelist, not a spot check: every key at
    /// every level is named, so a field that joins the file without joining
    /// this list fails here before it can become a stored result the design
    /// forbids.
    #[test]
    fn the_published_file_carries_exactly_the_listed_fields() {
        let (_scratch, _target, written) = produced_bytes();
        let document: Value = serde_json::from_slice(&written).unwrap();

        let top = document.as_object().unwrap();
        let top_keys: Vec<&str> = top.keys().map(String::as_str).collect();
        assert_eq!(
            top_keys,
            vec!["accounts", "ledger_generation", "schema_version"],
            "top-level fields must be exactly the design's list"
        );

        let accounts = document["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        let full = accounts[0].as_object().unwrap();
        let account_keys: Vec<&str> = full.keys().map(String::as_str).collect();
        assert_eq!(
            account_keys,
            vec![
                "account_id",
                "last_successful_observation",
                "latest_attempt",
                "logical_name",
                "provider"
            ],
            "per-account fields must be exactly the design's list"
        );

        let success = full["last_successful_observation"].as_object().unwrap();
        let success_keys: Vec<&str> = success.keys().map(String::as_str).collect();
        assert_eq!(
            success_keys,
            vec![
                "measurement_basis",
                "observation_id",
                "provider_observed_at_nanos",
                "received_at_nanos",
                "windows"
            ],
            "the last success carries the freshness machine's timestamp inputs and its windows"
        );

        let windows = success["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 2);
        let window_keys: Vec<&str> = windows[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            window_keys,
            vec![
                "nominal_duration_nanos",
                "quantization",
                "quota_used_ppm",
                "reported_resolution_ppm",
                "resets_at_nanos",
                "scope_kind",
                "scoped_model",
                "semantic_key"
            ],
            "windows carry the provider's reported values, nothing derived"
        );

        let attempt = full["latest_attempt"].as_object().unwrap();
        let attempt_keys: Vec<&str> = attempt.keys().map(String::as_str).collect();
        assert_eq!(
            attempt_keys,
            vec![
                "attempt_id",
                "credential_context_id",
                "request_started_at_nanos",
                "result"
            ],
            "the latest attempt carries its identity, its start and its credential context"
        );

        let result = attempt["result"].as_object().unwrap();
        let result_keys: Vec<&str> = result.keys().map(String::as_str).collect();
        assert_eq!(
            result_keys,
            vec!["completed_at_nanos", "failure_class", "outcome"],
            "the terminal outcome carries its time and its class"
        );

        let empty = accounts[1].as_object().unwrap();
        assert!(
            empty["last_successful_observation"].is_null(),
            "an account with no success carries the fact, not a placeholder value"
        );
        assert!(
            empty["latest_attempt"].is_null(),
            "an account with no attempt carries the fact, not a placeholder value"
        );
    }

    /// The projection's schema version and its source ledger generation are
    /// recorded in the file, asserted over a produced file.
    #[test]
    fn the_published_file_records_its_schema_version_and_source_generation() {
        let (_scratch, _target, written) = produced_bytes();
        let document: Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(document["schema_version"], PROJECTION_SCHEMA_VERSION);
        assert_eq!(document["ledger_generation"], 12);
    }

    /// None of the six the design forbids (PLAN.md section 16.1) can appear:
    /// no stored freshness boolean, no calibration constant, no computed
    /// spend, no valuation, no credential material, no raw provider body. The
    /// whitelist above is the primary guard; this names the forbidden
    /// vocabulary over the produced bytes so a near-miss field is named in the
    /// failure rather than silently absent.
    #[test]
    fn the_published_file_contains_none_of_the_six_forbidden_things() {
        let (_scratch, _target, written) = produced_bytes();
        let text = String::from_utf8(written).unwrap();

        assert!(
            !text.contains("fresh"),
            "no freshness state may be stored: freshness is recomputed at render time"
        );
        assert!(
            !text.contains("calibration"),
            "no calibration constant travels in the projection"
        );
        assert!(
            !text.contains("spend"),
            "no computed historical spend travels in the projection"
        );
        assert!(
            !text.contains("valuation") && !text.contains("price"),
            "no token valuation travels in the projection"
        );
        assert!(
            !text.contains("\"credential_material\"") && !text.contains("api_key"),
            "credential identifiers may travel, credential material may not"
        );
        assert!(
            !text.contains("evidence_capsule") && !text.contains("body"),
            "no raw provider body or evidence capsule travels in the projection"
        );
    }

    /// A planted negative for the schema test itself: an account that grew a
    /// forbidden `fresh` field is caught by the whitelist, which is what makes
    /// the schema test a test rather than a description.
    #[test]
    fn the_schema_test_rejects_a_file_that_grows_a_forbidden_field() {
        let mut document = full_projection().to_json();
        document["accounts"][0]["fresh"] = json!(true);
        let keys: Vec<&str> = document["accounts"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_ne!(
            keys,
            vec![
                "account_id",
                "last_successful_observation",
                "latest_attempt",
                "logical_name",
                "provider"
            ],
            "the whitelist above must fail an account that carries a freshness boolean"
        );
    }

    // --- file mode 0600 -------------------------------------------------------

    #[test]
    fn the_published_file_is_created_at_mode_0600() {
        let (_scratch, target, _) = produced_bytes();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the projection file must exist at mode 0600");
    }

    /// The negative that a naive in-place writer fails: an existing file left
    /// at a wider mode is repaired by being replaced, because the rename
    /// carries the temporary's 0600 mode.
    #[test]
    fn republishing_repairs_an_existing_file_left_at_a_wider_mode() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join(PROJECTION_FILE_NAME);
        fs::write(&target, b"stale and wide").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        let bytes = serde_json::to_vec(&full_projection().to_json()).unwrap();
        atomic_write(&target, &bytes).unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "publication must repair a file found wider than 0600"
        );
    }

    // --- deterministic reconstruction -----------------------------------------

    /// The `Done when` criterion: deleting the projection and re-running
    /// publication reproduces it byte for byte from the same database state.
    /// Both publications build from a real seeded database through the real
    /// read snapshot, so a wall clock, a map ordering or any other
    /// nondeterminism in the construction fails here.
    #[test]
    fn deleting_the_projection_and_republishing_reproduces_it_byte_for_byte() {
        let mut seeded = crate::store::projection_source::test_support::fixture("republish");
        let attempt = seeded.start_attempt();
        seeded.commit_success_bundle(attempt);

        let reader = crate::store::connection::open(
            &seeded._scratch.path().join("source.db"),
            crate::store::connection::AccessMode::ReadOnly,
            &crate::store::projection_source::test_support::fixture_policy(),
        )
        .unwrap();
        let target = seeded._scratch.path().join(PROJECTION_FILE_NAME);

        let first = publish_checked(&reader, &target).unwrap();
        let first_bytes = fs::read(&target).unwrap();
        fs::remove_file(&target).unwrap();
        let second = publish_checked(&reader, &target).unwrap();
        let second_bytes = fs::read(&target).unwrap();

        assert_eq!(
            first, second,
            "the recorded generation must be the same rebuild"
        );
        assert_eq!(
            first_bytes, second_bytes,
            "republishing from the same database state must reproduce the file byte for byte"
        );
    }

    /// The publication sequence the design states (PLAN.md section 16.1) is a
    /// property of the production source, not of an observable run: a removed
    /// fsync or a missing atomic rename changes durability and tear-safety
    /// that no behavior test over a passing run can catch. This scans the
    /// production half of this file for the sequence, the same convention the
    /// ledger generation module uses for its own substitutes ban.
    #[test]
    fn the_publication_source_carries_the_stated_write_sequence() {
        let source = include_str!("projection.rs");
        let production_source = source
            .split_once("#[cfg(test)]")
            .expect("this module must have a #[cfg(test)] boundary")
            .0;
        for expected in ["write_all", "sync_all", "fs::rename", "sync_directory"] {
            assert!(
                production_source.contains(expected),
                "the publication sequence must contain {expected}"
            );
        }
    }

    #[test]
    fn two_publications_from_unchanged_state_produce_identical_files() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join(PROJECTION_FILE_NAME);
        let bytes = serde_json::to_vec(&full_projection().to_json()).unwrap();

        atomic_write(&target, &bytes).unwrap();
        let first = fs::read(&target).unwrap();
        atomic_write(&target, &bytes).unwrap();
        let second = fs::read(&target).unwrap();

        assert_eq!(first, second);
    }

    // --- atomic replacement ---------------------------------------------------

    #[test]
    fn the_atomic_write_replaces_an_existing_file_completely() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join(PROJECTION_FILE_NAME);
        let long = vec![b'x'; 8_192];
        fs::write(&target, &long).unwrap();

        let bytes = serde_json::to_vec(&full_projection().to_json()).unwrap();
        atomic_write(&target, &bytes).unwrap();
        let now = fs::read(&target).unwrap();

        assert_eq!(now, bytes, "no stale tail of the previous file may survive");
    }

    /// The temporary file a crashed writer leaves behind is inert: nothing
    /// reads it, and it does not disturb the published file.
    #[test]
    fn a_leftover_temporary_file_is_never_read_as_the_projection() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join(PROJECTION_FILE_NAME);
        let temporary = scratch
            .path()
            .join(format!("{}.tmp-999999", PROJECTION_FILE_NAME));
        fs::write(&temporary, b"half-written garbage from a dead process").unwrap();

        let bytes = serde_json::to_vec(&full_projection().to_json()).unwrap();
        atomic_write(&target, &bytes).unwrap();
        let published = fs::read(&target).unwrap();

        assert_eq!(published, bytes);
        assert!(
            temporary.exists(),
            "the leftover temporary is not this writer's business"
        );
        assert!(
            !fs::read_to_string(&target)
                .unwrap()
                .contains("half-written"),
            "the published file must never contain a torn temporary's bytes"
        );
    }

    /// Publication onto an unwritable target defers with the reason instead of
    /// failing: the commit it follows is already durable, and a deferred
    /// publication is healed by the next one.
    #[test]
    fn publication_defers_with_a_reason_when_the_target_cannot_be_written() {
        let scratch = ScratchDir::new();
        let blocked = scratch
            .path()
            .join("missing-dir")
            .join(PROJECTION_FILE_NAME);
        let outcome = atomic_write(&blocked, b"{}").unwrap_err();
        assert!(
            outcome.to_string().contains("projection"),
            "the deferral must name the publication, got: {outcome}"
        );
    }

    // --- projected value serialization ----------------------------------------

    #[test]
    fn an_unreachable_outcome_serializes_its_failure_class_and_an_auth_one_does_not() {
        let reachable = TerminalOutcome {
            completed_at: UtcTimestamp::from_unix_nanos(4_000),
            outcome: AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        };
        let text = reachable.to_json().to_string();
        assert!(text.contains("\"outcome\":\"unreachable\""));
        assert!(text.contains("\"failure_class\":\"transport_timeout\""));

        let auth = TerminalOutcome {
            completed_at: UtcTimestamp::from_unix_nanos(4_000),
            outcome: AttemptOutcome::AuthRequired,
        };
        let text = auth.to_json().to_string();
        assert!(text.contains("\"outcome\":\"auth_required\""));
        assert!(text.contains("\"failure_class\":null"));
    }

    #[test]
    fn a_model_scoped_window_serializes_both_scope_parts() {
        let scoped = window("weekly", 400_000, Some("claude-model-x"));
        let text = scoped.to_json().to_string();
        assert!(text.contains("\"scope_kind\":\"model_specific\""));
        assert!(text.contains("\"scoped_model\":\"claude-model-x\""));

        let wide = window("five_hour", 250_000, None);
        let text = wide.to_json().to_string();
        assert!(text.contains("\"scope_kind\":\"account_wide\""));
        assert!(text.contains("\"scoped_model\":null"));
    }
}
