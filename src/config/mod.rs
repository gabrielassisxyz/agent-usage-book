//! All paths, accounts, credentials, sampling policy, and aliases (the configuration interfaces).
//!
//! May not depend on:
//! - SQLite, HTTP, or terminal-formatting crates
//! - transcript locations
//! - any adapter, workflow, or presentation layer
//!
//! Configuration is the only authority for local identity and paths, which is what
//! makes the no-compiled-identity invariant achievable at all: no source file names a
//! machine, an account, a username or a home directory. Resolution order is documented
//! and deterministic, checked in both directions everywhere it is tested (a higher
//! level wins when present, and the level below still wins when it is not, per the
//! lesson the domain epic paid six reworks to learn):
//!
//! ```text
//! command-line override (--set key=value)
//!   -> explicitly supported environment override (AUB_<SECTION>_<KEY>)
//!     -> config file
//!       -> non-identifying platform default
//! ```
//!
//! `aub config` (`crate::cli`) prints every resolved key with its value and
//! the source that won, using exactly the four labels below: `override`,
//! `environment`, `file`, `default`.
//!
//! Scope, stated rather than left implicit: the four scalar sections (`state`,
//! `sampling`, `freshness`, `coverage`) plus the doctor review horizons
//! (`backup.review_after`, `drill.max_age`, `adapter_semantics.max_comparison_age`, and
//! `doctor.meter_anomaly_horizon`) go through the full four-level order and are
//! individually provenance-tracked, since those are the keys
//! whose default this project actually defends (`aub-zxf`'s decision). `accounts`,
//! `transcripts`, `tracker` and `valuation.default_rate_book` are populated from the
//! file (or left absent) without flag/environment overrides: overriding a
//! heterogeneous list, or a credential shape that varies by its own `kind` field,
//! through one `--set` string is not a well-formed operation, and the adapters that
//! actually consume those sections (`aub-eun.1`'s credential resolution,
//! `aub-lqe.1`'s transcript discovery) are later beads.

mod duration;

pub mod aliases;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::advice::historical_distribution::{
    HistoricalDistributionConfig, Percentile, QuantileMethod,
};
use crate::advice::verdict::{AmpleMarginMultiple, CanRunHeadroomBound, CanRunVerdictConfig};
use crate::attribution::quality::AttributionQualityFloor;
use crate::domain::time::MonotonicDuration;
use crate::error::Error;

pub use aliases::AliasTable;
pub use duration::{format_config_duration, parse_duration};

/// Where a resolved value came from, in the order that decides a tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Flag,
    Environment,
    File,
    Default,
}

impl ConfigSource {
    /// The one of the four stable labels `aub config` prints for this source.
    /// A `--set key=value` command-line override prints as `override` (aub-ukh5):
    /// the row answers what the operator forced, not which flag spelling did it.
    pub fn label(self) -> &'static str {
        match self {
            ConfigSource::Flag => "override",
            ConfigSource::Environment => "environment",
            ConfigSource::File => "file",
            ConfigSource::Default => "default",
        }
    }
}

/// Reads named environment variables. A trait so tests resolve configuration under a
/// synthetic environment without mutating the real process environment, which would
/// make tests order-dependent under any test runner that parallelizes within a
/// process.
pub trait EnvSource {
    fn get(&self, name: &str) -> Option<String>;
}

/// The real process environment, used by the CLI entry point.
pub struct RealEnv;

impl EnvSource for RealEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed, injectable environment, used by tests to resolve configuration under a
/// synthetic `$HOME`/username without touching the real process environment.
#[derive(Debug, Clone, Default)]
pub struct FakeEnv(BTreeMap<String, String>);

impl FakeEnv {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn set(mut self, name: &str, value: impl Into<String>) -> Self {
        self.0.insert(name.to_string(), value.into());
        self
    }
}

impl EnvSource for FakeEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// Command-line `--set key=value` overrides, the highest-precedence source.
#[derive(Debug, Clone, Default)]
pub struct Overrides(BTreeMap<String, String>);

impl Overrides {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

/// The source that won for every resolved key, in the dotted-key order `aub config`
/// prints them in.
#[derive(Debug, Clone, Default)]
pub struct Provenance(BTreeMap<String, ConfigSource>);

impl Provenance {
    fn set(&mut self, key: &str, source: ConfigSource) {
        self.0.insert(key.to_string(), source);
    }

    /// Every resolved key and the source that won for it, in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, ConfigSource)> {
        self.0.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn get(&self, key: &str) -> Option<ConfigSource> {
        self.0.get(key).copied()
    }
}

/// A coverage floor: a fraction in `[0.0, 1.0]`. Private storage, validated
/// construction, matching this project's rule for every ordinary quantity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverageFloor(f64);

impl CoverageFloor {
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=1.0).contains(&value).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// The floor in parts per million, rounded half-up. The named conversion
    /// from the configured fraction to the unit the rest of this project
    /// expresses fractions in, so the JSON contract carries the floor in the
    /// same unit as the coverages it judges.
    pub fn as_ppm(self) -> u32 {
        (self.0 * 1_000_000.0).round().clamp(0.0, 1_000_000.0) as u32
    }
}

impl std::fmt::Display for CoverageFloor {
    /// Renders the floor as the bare fraction `aub config` prints (aub-ukh5):
    /// `0.98` as written in TOML, never a percentage or a parts-per-million
    /// count, so the row reads in the unit the operator configured.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct StateConfig {
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SamplingConfig {
    pub scheduler_tick: MonotonicDuration,
    pub default_interval: MonotonicDuration,
    pub reset_edge_lead: MonotonicDuration,
    pub request_timeout: MonotonicDuration,
    /// How long `aub sample` waits for the ledger's write slot before refusing
    /// a tick. Its own key rather than `request_timeout`: that one bounds a
    /// provider request, and an operator raising it for a slow provider must
    /// not thereby lengthen a lock wait, nor push it past the store's bound.
    pub busy_timeout: MonotonicDuration,
    pub command_budget: MonotonicDuration,
    /// The most provider requests one sampling batch may keep in flight.
    /// Bounded so a machine with many configured accounts cannot open an
    /// unbounded number of simultaneous connections; the default is small
    /// because only a few accounts exist today.
    pub max_concurrent_requests: usize,
}

/// The transcript ingest batch policy (PLAN.md section 11.2: "Transcript ingest
/// commits in bounded batches so it cannot monopolize the single SQLite writer
/// slot"). One ingest pass lands its canonical usage events in transactions of
/// at most `max_batch_events` events or `max_batch_files` files, whichever
/// comes first, releasing the writer slot between batches, so a concurrent
/// meter write never waits behind one unbounded pass.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// The maximum number of canonical usage events one ingest batch lands in
    /// one transaction. A batch commits atomically or not at all; the bound
    /// caps how long any one batch can hold the writer slot. The value must be
    /// at least 1: a zero bound would mean no batch could ever land a row.
    pub max_batch_events: u64,
    /// The maximum number of source files one ingest batch may span (`aub-va6s`).
    /// A file's events are never split across two batches, so this is the
    /// commit boundary that actually bounds how long the corpus goes without a
    /// commit when files carry few events each: `max_batch_events` alone
    /// would let a batch of many small files grow unbounded in file count and
    /// wall time before it ever landed. The value must be at least 1.
    pub max_batch_files: u64,
    /// The longest one ingest transaction may hold the SQLite writer slot,
    /// independently of `max_batch_events` and `max_batch_files` (`aub-mh1c`).
    /// Those two bound a batch by what it counts, not by what landing it
    /// actually costs; a batch that hits this bound instead commits whatever
    /// it already landed and the remainder continues as a further
    /// transaction, so a sampler waiting on the writer lock is served within
    /// this bound however slow the per-event cost turns out to be.
    pub max_batch_seconds: MonotonicDuration,
}

#[derive(Debug, Clone)]
pub struct FreshnessConfig {
    pub meter: MonotonicDuration,
}

#[derive(Debug, Clone)]
pub struct CoverageConfig {
    pub attempt_floor: CoverageFloor,
    pub measurement_floor: CoverageFloor,
}

/// The attribution-quality policy: the advisory floor for the attributed
/// fraction and the recent window the metric is also computed over.
#[derive(Debug, Clone)]
pub struct AttributionConfig {
    /// The advisory floor `doctor` flags a breach of. `None` until an operator
    /// configures one (the value itself is decided by `aub-cab.7`): the metric
    /// is still reported, just not judged.
    pub quality_floor: Option<AttributionQualityFloor>,
    /// The recent window the metric is computed over in addition to all
    /// history, so a slow decline in attribution coverage is visible against a
    /// lifetime average.
    pub recent_window: MonotonicDuration,
}

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub review_after: MonotonicDuration,
    /// Where `doctor` looks for the last verified backup. `aub backup` takes its
    /// destination as an explicit argument and remembers nothing durably, so
    /// without this the backup-age check would have nowhere to look. `None`
    /// means backup age is not applicable rather than an assumed default path.
    pub destination: Option<PathBuf>,
}

/// The periodic restore drill's own review policy, the same shape as
/// [`BackupConfig`] and for the same reason: `aub drill` takes its scratch
/// destination and source as explicit arguments and remembers nothing
/// durably on its own, so `doctor` needs a configured place to read the last
/// recorded run from.
#[derive(Debug, Clone)]
pub struct DrillConfig {
    pub max_age: MonotonicDuration,
    /// Where `aub drill` appends one durable JSON record per run, and where
    /// `doctor` reads the age of the last successful one. `None` means drill
    /// age is not applicable rather than an assumed default path.
    pub result: Option<PathBuf>,
}

/// The review policy for the adapter-semantics comparison log (`aub-eun.12`,
/// docs/adapter-semantics-validation.md), the same shape as [`BackupConfig`]
/// and [`DrillConfig`] and for the same reason: `doctor` needs a configured
/// threshold to turn the age of the newest recorded comparison into a
/// pass/fail verdict. Unlike backup and drill there is no destination path
/// here: the comparison log lives in the ledger itself
/// (`store::adapter_semantics_validation::latest_comparison_read_at`), so
/// there is nowhere else it could be configured to.
#[derive(Debug, Clone)]
pub struct AdapterSemanticsConfig {
    pub max_comparison_age: MonotonicDuration,
}

/// The thresholds `aub doctor` uses to distinguish current health from retained
/// evidence. The 15-minute anomaly horizon spans several normal sampling cycles,
/// so an ongoing detector fault remains visible while a corrected false-positive
/// detector can return to a meaningful passing state.
#[derive(Debug, Clone)]
pub struct DoctorConfig {
    pub meter_anomaly_horizon: MonotonicDuration,
}

/// The exclusivity policy for a configured account (`aub-c0b.7`).
///
/// Exhaustive with no wildcard arm: an unrecognized configured value fails
/// configuration resolution with [`Error::Usage`] naming `accounts[].exclusivity_policy`,
/// matching this repository's rule for configured values (`QuantileMethod`, `CanRunHeadroomBound`).
///
/// Accepted values:
/// - `"permit_passive"`: passive calibration fitting is permitted on this account.
/// - `"forbid_passive"`: passive calibration fitting is forbidden on this account.
///
/// An absent `exclusivity_policy` key in `[[accounts]]` defaults to [`Self::ForbidPassive`].
/// This conservative (fail-closed) default ensures an account without an explicit policy
/// is not assumed to have exclusive traffic, preventing multi-consumer or unverified
/// sessions from silently contaminating passive calibration candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountExclusivityPolicy {
    /// Passive calibration fitting is permitted on this account.
    PermitPassive,
    /// Passive calibration fitting is forbidden on this account (the conservative default).
    #[default]
    ForbidPassive,
}

impl AccountExclusivityPolicy {
    /// The stable name this policy resolves from and renders under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PermitPassive => "permit_passive",
            Self::ForbidPassive => "forbid_passive",
        }
    }

    /// Parses the stable name, returning `None` for any unrecognized value.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "permit_passive" => Some(Self::PermitPassive),
            "forbid_passive" => Some(Self::ForbidPassive),
            _ => None,
        }
    }

    /// True when this policy permits passive calibration fitting.
    pub const fn permits_passive_fitting(self) -> bool {
        match self {
            Self::PermitPassive => true,
            Self::ForbidPassive => false,
        }
    }
}

impl std::fmt::Display for AccountExclusivityPolicy {
    /// Renders the stable name `aub config` prints (aub-ukh5).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A configured account. `credential_kind`/`credential_detail` are a loose pass-through
/// of the file's `credential` table (`kind`, plus its `ref`, `path` or `name`): the typed,
/// validated credential model belongs to `aub-eun.1`, which consumes this section.
#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub provider: String,
    pub credential_kind: String,
    pub credential_detail: String,
    pub exclusivity_policy: AccountExclusivityPolicy,
    /// The Codex home directory a `provider = "codex"` account's meter reads
    /// its evidence from (`aub-cg6k`). Optional: a Codex account can be
    /// transcript-only, with no meter home at all, and no other provider has
    /// a local source, so a `codex_home` on any other provider is rejected.
    pub codex_home: Option<PathBuf>,
}

impl AccountConfig {
    /// True when this account's exclusivity policy permits passive calibration fitting (`aub-c0b.7`).
    pub fn permits_passive_fitting(&self) -> bool {
        self.exclusivity_policy.permits_passive_fitting()
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptConfig {
    pub name: String,
    pub root: PathBuf,
    pub pattern: String,
    /// Which parser reads this source: `claude-code`, `codex`, `opencode` or `pi`. The name is
    /// the operator's label and says nothing about the record shape, so the format
    /// is declared rather than guessed from a path.
    pub format: Option<String>,
    pub usage_evidence: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrackerConfig {
    pub kind: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ValuationConfig {
    pub default_rate_book: Option<String>,
}

/// The rolling-residual self-audit policy `doctor` reads (PLAN.md section 35,
/// aub-dpn.3). `doctor` reconciles observed meter movement against locally
/// explained movement over recent eligible intervals; these two keys bound the
/// window it looks back over and the fewest eligible intervals it will state a
/// verdict from.
#[derive(Debug, Clone)]
pub struct ReconciliationConfig {
    /// How far back `doctor` looks for eligible reconciliation intervals when it
    /// reports rolling residual health.
    pub residual_window: MonotonicDuration,
    /// The fewest eligible intervals the window must hold before `doctor` states
    /// a residual verdict. Below it the count is still reported and the verdict
    /// is suppressed, never averaged out of too few points.
    pub residual_min_eligible: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub state: StateConfig,
    pub sampling: SamplingConfig,
    pub freshness: FreshnessConfig,
    pub coverage: CoverageConfig,
    pub attribution: AttributionConfig,
    /// The historical task distribution's default quantiles, minimum sample
    /// count and attribution-quality floor (`aub-1o3`, `aub-cab.7`), owned
    /// and documented by `crate::advice::historical_distribution`.
    pub task_distribution: HistoricalDistributionConfig,
    /// The can-run verdict thresholds (`aub-jsq`, decided 2026-08-25), owned
    /// and documented by `crate::advice::verdict`.
    pub can_run: CanRunVerdictConfig,
    pub reconciliation: ReconciliationConfig,
    pub accounts: Vec<AccountConfig>,
    pub ingest: IngestConfig,
    pub transcripts: Vec<TranscriptConfig>,
    pub tracker: Option<TrackerConfig>,
    pub valuation: ValuationConfig,
    pub backup: BackupConfig,
    pub drill: DrillConfig,
    pub adapter_semantics: AdapterSemanticsConfig,
    pub doctor: DoctorConfig,
    /// Working-directory to logical project identity (`aub-lqe.12`).
    pub projects: AliasTable,
    /// Working-directory to logical repository identity (`aub-lqe.12`).
    pub repositories: AliasTable,
}

/// The section names and, one level down, the key names this project recognizes. An
/// unknown key anywhere in this shape is an error naming the key, never a silently
/// ignored line.
const KNOWN_SECTIONS: &[&str] = &[
    "schema",
    "state",
    "sampling",
    "ingest",
    "freshness",
    "coverage",
    "attribution",
    "task_distribution",
    "can_run",
    "reconciliation",
    "accounts",
    "transcripts",
    "tracker",
    "valuation",
    "backup",
    "drill",
    "adapter_semantics",
    "doctor",
    "projects",
    "repositories",
];
const STATE_KEYS: &[&str] = &["dir"];
const SAMPLING_KEYS: &[&str] = &[
    "scheduler_tick",
    "default_interval",
    "reset_edge_lead",
    "request_timeout",
    "busy_timeout",
    "command_budget",
    "max_concurrent_requests",
];
const INGEST_KEYS: &[&str] = &["max_batch_events", "max_batch_files", "max_batch_seconds"];
const FRESHNESS_KEYS: &[&str] = &["meter"];
const COVERAGE_KEYS: &[&str] = &["attempt_floor", "measurement_floor"];
const ATTRIBUTION_KEYS: &[&str] = &["quality_floor", "recent_window"];
const TASK_DISTRIBUTION_KEYS: &[&str] = &[
    "central_low",
    "central_high",
    "upper",
    "min_samples",
    "quantile_method",
    "attribution_floor",
];
const CAN_RUN_KEYS: &[&str] = &["labels", "ample_margin_multiple", "headroom_bound"];
const RECONCILIATION_KEYS: &[&str] = &["residual_window", "residual_min_eligible"];
const ACCOUNT_KEYS: &[&str] = &[
    "name",
    "provider",
    "credential",
    "exclusivity_policy",
    "codex_home",
];
const CREDENTIAL_PROFILE_KEYS: &[&str] = &["kind", "ref"];
const CREDENTIAL_FILE_KEYS: &[&str] = &["kind", "path"];
const CREDENTIAL_ENV_KEYS: &[&str] = &["kind", "name"];
const TRANSCRIPT_KEYS: &[&str] = &["name", "root", "pattern", "format", "usage_evidence"];
const TRACKER_KEYS: &[&str] = &["kind", "path"];
const VALUATION_KEYS: &[&str] = &["default_rate_book"];
const BACKUP_KEYS: &[&str] = &["review_after", "destination"];
const DRILL_KEYS: &[&str] = &["max_age", "result"];
const ADAPTER_SEMANTICS_KEYS: &[&str] = &["max_comparison_age"];
const DOCTOR_KEYS: &[&str] = &["meter_anomaly_horizon"];

fn unknown_key_error(key: &str, file_display: &str) -> Error {
    Error::Usage(format!(
        "unknown configuration key {key:?} in {file_display}; remove it or fix the spelling"
    ))
}

fn missing_key_error(key: &str, file_display: &str) -> Error {
    Error::Usage(format!(
        "missing required configuration key {key:?}; set it in {file_display}"
    ))
}

/// Renders a config file path for error messages with the home directory
/// collapsed to `~`, so a default error never prints an absolute home path
/// (aub-xus.8). A path outside the home directory is left as it is.
fn display_path(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !rest.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn check_keys(
    table: &toml::Table,
    allowed: &[&str],
    path: &str,
    file_display: &str,
) -> Result<(), Error> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(unknown_key_error(&format!("{path}.{key}"), file_display));
        }
    }
    Ok(())
}

/// Rejects every key not on this project's known list, walked over the whole parsed
/// file rather than only the sections this bead resolves scalar-by-scalar, so a typo
/// anywhere in the file is caught here rather than silently ignored.
fn validate_known_keys(table: &toml::Table, file_display: &str) -> Result<(), Error> {
    check_keys(table, KNOWN_SECTIONS, "", file_display)?;

    if let Some(t) = table.get("state").and_then(toml::Value::as_table) {
        check_keys(t, STATE_KEYS, "state", file_display)?;
    }
    if let Some(t) = table.get("sampling").and_then(toml::Value::as_table) {
        check_keys(t, SAMPLING_KEYS, "sampling", file_display)?;
    }
    if let Some(t) = table.get("ingest").and_then(toml::Value::as_table) {
        check_keys(t, INGEST_KEYS, "ingest", file_display)?;
    }
    if let Some(t) = table.get("freshness").and_then(toml::Value::as_table) {
        check_keys(t, FRESHNESS_KEYS, "freshness", file_display)?;
    }
    if let Some(t) = table.get("coverage").and_then(toml::Value::as_table) {
        check_keys(t, COVERAGE_KEYS, "coverage", file_display)?;
    }
    if let Some(t) = table.get("attribution").and_then(toml::Value::as_table) {
        check_keys(t, ATTRIBUTION_KEYS, "attribution", file_display)?;
    }
    if let Some(t) = table
        .get("task_distribution")
        .and_then(toml::Value::as_table)
    {
        check_keys(t, TASK_DISTRIBUTION_KEYS, "task_distribution", file_display)?;
    }
    if let Some(t) = table.get("can_run").and_then(toml::Value::as_table) {
        check_keys(t, CAN_RUN_KEYS, "can_run", file_display)?;
    }
    if let Some(t) = table.get("reconciliation").and_then(toml::Value::as_table) {
        check_keys(t, RECONCILIATION_KEYS, "reconciliation", file_display)?;
    }
    if let Some(t) = table.get("tracker").and_then(toml::Value::as_table) {
        check_keys(t, TRACKER_KEYS, "tracker", file_display)?;
    }
    if let Some(t) = table.get("valuation").and_then(toml::Value::as_table) {
        check_keys(t, VALUATION_KEYS, "valuation", file_display)?;
    }
    if let Some(t) = table.get("backup").and_then(toml::Value::as_table) {
        check_keys(t, BACKUP_KEYS, "backup", file_display)?;
    }
    if let Some(t) = table.get("drill").and_then(toml::Value::as_table) {
        check_keys(t, DRILL_KEYS, "drill", file_display)?;
    }
    if let Some(t) = table
        .get("adapter_semantics")
        .and_then(toml::Value::as_table)
    {
        check_keys(t, ADAPTER_SEMANTICS_KEYS, "adapter_semantics", file_display)?;
    }
    if let Some(t) = table.get("doctor").and_then(toml::Value::as_table) {
        check_keys(t, DOCTOR_KEYS, "doctor", file_display)?;
    }
    if let Some(accounts) = table.get("accounts").and_then(toml::Value::as_array) {
        for account in accounts {
            let Some(account) = account.as_table() else {
                continue;
            };
            check_keys(account, ACCOUNT_KEYS, "accounts[]", file_display)?;
            if let Some(cred) = account.get("credential").and_then(toml::Value::as_table) {
                match cred.get("kind").and_then(toml::Value::as_str) {
                    Some("profile") => check_keys(
                        cred,
                        CREDENTIAL_PROFILE_KEYS,
                        "accounts[].credential",
                        file_display,
                    )?,
                    Some("file") => check_keys(
                        cred,
                        CREDENTIAL_FILE_KEYS,
                        "accounts[].credential",
                        file_display,
                    )?,
                    Some("env") => check_keys(
                        cred,
                        CREDENTIAL_ENV_KEYS,
                        "accounts[].credential",
                        file_display,
                    )?,
                    // An unrecognized or absent `kind` is left to aub-eun.1's
                    // credential resolution to reject; this bead only owns the
                    // shape of the three kinds it already knows about.
                    _ => {}
                }
            }
        }
    }
    if let Some(transcripts) = table.get("transcripts").and_then(toml::Value::as_array) {
        for transcript in transcripts {
            if let Some(transcript) = transcript.as_table() {
                check_keys(transcript, TRANSCRIPT_KEYS, "transcripts[]", file_display)?;
            }
        }
    }
    for section in ["projects", "repositories"] {
        if let Some(aliases) = table.get(section).and_then(toml::Value::as_table) {
            for (path, value) in aliases {
                if value.as_str().is_none() {
                    return Err(Error::Usage(format!(
                        "{section}.{path}: alias value must be a string"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// One entry in the file's parsed dotted-path lookup, rendered as a string regardless
/// of whether the TOML author wrote it quoted (a duration like `"5m"`) or bare (a
/// coverage floor like `0.98`): `resolve_string` and everything built on it work on
/// text, and a coverage floor written as a bare TOML float has no `as_str()` at all,
/// which is what the first version of this function missed - it silently fell through
/// to the platform default for every floor actually set in the file, in both
/// directions (a floor that should have failed range validation resolved to the
/// default instead, and a valid in-range floor never got read from the file either).
fn file_raw(file: Option<&toml::Table>, section: &str, key: &str) -> Option<String> {
    let value = file?.get(section)?.as_table()?.get(key)?;
    match value {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Float(n) => Some(n.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Datetime(_) | toml::Value::Array(_) | toml::Value::Table(_) => None,
    }
}

fn env_var_name(key: &str) -> String {
    format!("AUB_{}", key.to_uppercase().replace('.', "_"))
}

fn resolve_string(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<String, Error> {
    if let Some(v) = overrides.get(key) {
        provenance.set(key, ConfigSource::Flag);
        return Ok(v.to_string());
    }
    let env_var = env_var_name(key);
    if let Some(v) = env.get(&env_var) {
        provenance.set(key, ConfigSource::Environment);
        return Ok(v);
    }
    if let Some(v) = file_value {
        provenance.set(key, ConfigSource::File);
        return Ok(v);
    }
    if let Some(v) = default {
        provenance.set(key, ConfigSource::Default);
        return Ok(v.to_string());
    }
    Err(missing_key_error(key, file_display))
}

fn resolve_duration(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<MonotonicDuration, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    parse_duration(&raw).map_err(|e| Error::Usage(format!("{key}: {e}")))
}

/// Resolves a positive integer count: the four-level string order, then a
/// parse that refuses zero and every non-number with the key named, so a
/// mistyped value is a usage error rather than a silently zero batch bound.
fn resolve_positive_count(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<u64, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    let parsed = raw
        .parse::<u64>()
        .map_err(|_| Error::Usage(format!("{key}: {raw:?} is not a positive integer")))?;
    if parsed == 0 {
        return Err(Error::Usage(format!(
            "{key}: must be at least 1, got {raw:?}"
        )));
    }
    Ok(parsed)
}

fn resolve_floor(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<CoverageFloor, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    let value: f64 = raw
        .parse()
        .map_err(|_| Error::Usage(format!("{key}: {raw:?} is not a number")))?;
    CoverageFloor::new(value)
        .ok_or_else(|| Error::Usage(format!("{key}: {value} is not in the range [0.0, 1.0]")))
}

/// Resolves a percentile in `[0, 100]` through the four-level order.
fn resolve_percentile(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<Percentile, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    let value: u8 = raw
        .parse()
        .map_err(|_| Error::Usage(format!("{key}: {raw:?} is not a whole number 0-100")))?;
    Percentile::new(value)
        .ok_or_else(|| Error::Usage(format!("{key}: {value} is not in the range [0, 100]")))
}

/// Resolves a [`QuantileMethod`] by its stable name through the four-level
/// order.
fn resolve_quantile_method(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<QuantileMethod, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    QuantileMethod::parse(&raw).ok_or_else(|| {
        Error::Usage(format!(
            "{key}: {raw:?} is not a recognized quantile method"
        ))
    })
}

/// An attribution-quality floor with no platform default: absent everywhere
/// means `None`, and an operator opts in by setting it. Follows the same
/// override, environment, file precedence as the other scalars, without the
/// fourth (default) level.
fn resolve_optional_floor(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    provenance: &mut Provenance,
) -> Result<Option<AttributionQualityFloor>, Error> {
    let raw = if let Some(v) = overrides.get(key) {
        provenance.set(key, ConfigSource::Flag);
        Some(v.to_string())
    } else if let Some(v) = env.get(&env_var_name(key)) {
        provenance.set(key, ConfigSource::Environment);
        Some(v)
    } else if let Some(v) = file_value {
        provenance.set(key, ConfigSource::File);
        Some(v)
    } else {
        None
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: f64 = raw
        .parse()
        .map_err(|_| Error::Usage(format!("{key}: {raw:?} is not a number")))?;
    AttributionQualityFloor::new(value)
        .map(Some)
        .ok_or_else(|| Error::Usage(format!("{key}: {value} is not in the range [0.0, 1.0]")))
}

/// Resolves a boolean through the four-level order, accepting only the TOML
/// spellings `true` and `false`, so a mistyped value is a usage error naming
/// the key rather than a silently false flag.
fn resolve_bool(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<bool, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    match raw.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(Error::Usage(format!(
            "{key}: {raw:?} is not a boolean (expected \"true\" or \"false\")"
        ))),
    }
}

/// A count a configuration file expresses as a bare positive integer. A value
/// of zero would sample nothing while reporting a completed batch, so it is
/// refused at resolution time rather than discovered at sampling time.
fn resolve_count(
    key: &str,
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_value: Option<String>,
    default: Option<&str>,
    file_display: &str,
    provenance: &mut Provenance,
) -> Result<usize, Error> {
    let raw = resolve_string(
        key,
        overrides,
        env,
        file_value,
        default,
        file_display,
        provenance,
    )?;
    let value: usize = raw
        .parse()
        .map_err(|_| Error::Usage(format!("{key}: {raw:?} is not a whole number")))?;
    if value == 0 {
        return Err(Error::Usage(format!(
            "{key}: a bound of zero would sample nothing; set it to at least 1"
        )));
    }
    Ok(value)
}

/// Non-identifying platform defaults: derived from `$HOME` at resolution time, never
/// from a compiled-in path. `home` is itself resolution's caller-supplied, so a test
/// can prove no default leaks the *real* process's home directory by resolving under a
/// synthetic one instead of the actual `$HOME`.
fn default_state_dir(home: &str) -> String {
    format!("{home}/.local/state/aub")
}

/// Resolves the full configuration from, in precedence order, `overrides`, `env`, the
/// TOML file at `file_contents` (already read by the caller, so this function stays
/// free of filesystem access and is trivially testable), and this project's own
/// defaults. `file_display` names the file in a missing-key error even though this
/// function never opens it itself.
pub fn resolve(
    overrides: &Overrides,
    env: &dyn EnvSource,
    file_contents: Option<&str>,
    file_display: &str,
) -> Result<(Config, Provenance), Error> {
    let home = env
        .get("HOME")
        .unwrap_or_else(|| "/nonexistent".to_string());
    // Error messages name the config file home-relative, so a default error
    // never prints an absolute home-directory path (aub-xus.8).
    let file_display = display_path(file_display, &home);
    let file: Option<toml::Table> = match file_contents {
        Some(contents) => Some(contents.parse().map_err(|e| {
            Error::Usage(format!("{file_display}: invalid TOML: {e}; fix the file"))
        })?),
        None => None,
    };
    if let Some(table) = &file {
        validate_known_keys(table, &file_display)?;
    }

    let mut provenance = Provenance::default();
    let default_dir = default_state_dir(&home);

    let state = StateConfig {
        dir: PathBuf::from(resolve_string(
            "state.dir",
            overrides,
            env,
            file_raw(file.as_ref(), "state", "dir"),
            Some(&default_dir),
            &file_display,
            &mut provenance,
        )?),
    };

    let ingest = IngestConfig {
        max_batch_events: resolve_positive_count(
            "ingest.max_batch_events",
            overrides,
            env,
            file_raw(file.as_ref(), "ingest", "max_batch_events"),
            Some("5000"),
            &file_display,
            &mut provenance,
        )?,
        max_batch_files: resolve_positive_count(
            "ingest.max_batch_files",
            overrides,
            env,
            file_raw(file.as_ref(), "ingest", "max_batch_files"),
            Some("200"),
            &file_display,
            &mut provenance,
        )?,
        max_batch_seconds: resolve_duration(
            "ingest.max_batch_seconds",
            overrides,
            env,
            file_raw(file.as_ref(), "ingest", "max_batch_seconds"),
            Some("2s"),
            &file_display,
            &mut provenance,
        )?,
    };

    let sampling = SamplingConfig {
        scheduler_tick: resolve_duration(
            "sampling.scheduler_tick",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "scheduler_tick"),
            Some("1m"),
            &file_display,
            &mut provenance,
        )?,
        default_interval: resolve_duration(
            "sampling.default_interval",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "default_interval"),
            Some("5m"),
            &file_display,
            &mut provenance,
        )?,
        reset_edge_lead: resolve_duration(
            "sampling.reset_edge_lead",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "reset_edge_lead"),
            Some("120s"),
            &file_display,
            &mut provenance,
        )?,
        request_timeout: resolve_duration(
            "sampling.request_timeout",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "request_timeout"),
            Some("5s"),
            &file_display,
            &mut provenance,
        )?,
        // Default sized against a batched ingest's commit cadence (about 5000 events,
        // seconds at most per batch) and under the store's 30s bound; a lock held
        // longer than this is a stuck writer, not a batch, and refusing is right.
        busy_timeout: resolve_duration(
            "sampling.busy_timeout",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "busy_timeout"),
            Some("10s"),
            &file_display,
            &mut provenance,
        )?,
        command_budget: resolve_duration(
            "sampling.command_budget",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "command_budget"),
            Some("8s"),
            &file_display,
            &mut provenance,
        )?,
        max_concurrent_requests: resolve_count(
            "sampling.max_concurrent_requests",
            overrides,
            env,
            file_raw(file.as_ref(), "sampling", "max_concurrent_requests"),
            Some("2"),
            &file_display,
            &mut provenance,
        )?,
    };

    let freshness = FreshnessConfig {
        meter: resolve_duration(
            "freshness.meter",
            overrides,
            env,
            file_raw(file.as_ref(), "freshness", "meter"),
            Some("12m"),
            &file_display,
            &mut provenance,
        )?,
    };

    let coverage = CoverageConfig {
        attempt_floor: resolve_floor(
            "coverage.attempt_floor",
            overrides,
            env,
            file_raw(file.as_ref(), "coverage", "attempt_floor"),
            Some("0.98"),
            &file_display,
            &mut provenance,
        )?,
        measurement_floor: resolve_floor(
            "coverage.measurement_floor",
            overrides,
            env,
            file_raw(file.as_ref(), "coverage", "measurement_floor"),
            Some("0.95"),
            &file_display,
            &mut provenance,
        )?,
    };

    let attribution = AttributionConfig {
        quality_floor: resolve_optional_floor(
            "attribution.quality_floor",
            overrides,
            env,
            file_raw(file.as_ref(), "attribution", "quality_floor"),
            &mut provenance,
        )?,
        recent_window: resolve_duration(
            "attribution.recent_window",
            overrides,
            env,
            file_raw(file.as_ref(), "attribution", "recent_window"),
            Some("30d"),
            &file_display,
            &mut provenance,
        )?,
    };

    // Defaults documented on `aub-1o3` (2026-09-04, option A) and
    // `aub-cab.7` (2026-09-04, option B): central range p25-p75, upper
    // reference p90, minimum 12 samples, nearest-rank, attribution floor
    // 0.80.
    let task_distribution_central_low = resolve_percentile(
        "task_distribution.central_low",
        overrides,
        env,
        file_raw(file.as_ref(), "task_distribution", "central_low"),
        Some("25"),
        &file_display,
        &mut provenance,
    )?;
    let task_distribution_central_high = resolve_percentile(
        "task_distribution.central_high",
        overrides,
        env,
        file_raw(file.as_ref(), "task_distribution", "central_high"),
        Some("75"),
        &file_display,
        &mut provenance,
    )?;
    let task_distribution_upper = resolve_percentile(
        "task_distribution.upper",
        overrides,
        env,
        file_raw(file.as_ref(), "task_distribution", "upper"),
        Some("90"),
        &file_display,
        &mut provenance,
    )?;
    if task_distribution_central_low >= task_distribution_central_high
        || task_distribution_central_high > task_distribution_upper
    {
        return Err(Error::Usage(format!(
            "task_distribution: central_low ({}) must be less than central_high ({}), which must be at most upper ({})",
            task_distribution_central_low.value(),
            task_distribution_central_high.value(),
            task_distribution_upper.value()
        )));
    }
    let task_distribution_min_samples = resolve_count(
        "task_distribution.min_samples",
        overrides,
        env,
        file_raw(file.as_ref(), "task_distribution", "min_samples"),
        Some("12"),
        &file_display,
        &mut provenance,
    )?;
    let task_distribution_quantile_method = resolve_quantile_method(
        "task_distribution.quantile_method",
        overrides,
        env,
        file_raw(file.as_ref(), "task_distribution", "quantile_method"),
        Some("nearest-rank"),
        &file_display,
        &mut provenance,
    )?;
    let task_distribution_attribution_floor_fraction = resolve_floor(
        "task_distribution.attribution_floor",
        overrides,
        env,
        file_raw(file.as_ref(), "task_distribution", "attribution_floor"),
        Some("0.80"),
        &file_display,
        &mut provenance,
    )?;
    let task_distribution = HistoricalDistributionConfig {
        central_low: task_distribution_central_low,
        central_high: task_distribution_central_high,
        upper: task_distribution_upper,
        min_samples: task_distribution_min_samples,
        quantile_method: task_distribution_quantile_method,
        attribution_floor: AttributionQualityFloor::new(
            task_distribution_attribution_floor_fraction.get(),
        )
        .expect("CoverageFloor's [0,1] range matches AttributionQualityFloor::new's domain"),
    };

    // Defaults decided on `aub-jsq` (2026-08-25, option A): labels enabled in
    // both output modes, AMPLE at twice the upper reference against the low end
    // of the headroom. The multiple is coupled to `aub-1o3`'s upper reference
    // (p90, decided 2026-09-04, so the threshold does not move); the trail on
    // `aub-cab.3` records both halves of that coupling.
    let can_run_labels = resolve_bool(
        "can_run.labels",
        overrides,
        env,
        file_raw(file.as_ref(), "can_run", "labels"),
        Some("true"),
        &file_display,
        &mut provenance,
    )?;
    let can_run_multiple_raw = resolve_string(
        "can_run.ample_margin_multiple",
        overrides,
        env,
        file_raw(file.as_ref(), "can_run", "ample_margin_multiple"),
        Some("2.0"),
        &file_display,
        &mut provenance,
    )?;
    let can_run_multiple_value: f64 = can_run_multiple_raw.parse().map_err(|_| {
        Error::Usage(format!(
            "can_run.ample_margin_multiple: {can_run_multiple_raw:?} is not a number"
        ))
    })?;
    let can_run_multiple = AmpleMarginMultiple::new(can_run_multiple_value).ok_or_else(|| {
        Error::Usage(format!(
            "can_run.ample_margin_multiple: {can_run_multiple_value} is not a positive finite number"
        ))
    })?;
    let can_run_bound_raw = resolve_string(
        "can_run.headroom_bound",
        overrides,
        env,
        file_raw(file.as_ref(), "can_run", "headroom_bound"),
        Some("low"),
        &file_display,
        &mut provenance,
    )?;
    let can_run_bound = CanRunHeadroomBound::parse(&can_run_bound_raw).ok_or_else(|| {
        Error::Usage(format!(
            "can_run.headroom_bound: {can_run_bound_raw:?} is not a recognized headroom bound"
        ))
    })?;
    let can_run = CanRunVerdictConfig {
        labels_enabled: can_run_labels,
        ample_margin_multiple: can_run_multiple,
        headroom_bound: can_run_bound,
    };

    let reconciliation = ReconciliationConfig {
        residual_window: resolve_duration(
            "reconciliation.residual_window",
            overrides,
            env,
            file_raw(file.as_ref(), "reconciliation", "residual_window"),
            Some("30d"),
            &file_display,
            &mut provenance,
        )?,
        residual_min_eligible: resolve_positive_count(
            "reconciliation.residual_min_eligible",
            overrides,
            env,
            file_raw(file.as_ref(), "reconciliation", "residual_min_eligible"),
            Some("5"),
            &file_display,
            &mut provenance,
        )? as usize,
    };

    let backup_destination = file
        .as_ref()
        .and_then(|t| t.get("backup"))
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("destination"))
        .and_then(toml::Value::as_str)
        .map(PathBuf::from);
    if backup_destination.is_some() {
        provenance.set("backup.destination", ConfigSource::File);
    }
    let backup = BackupConfig {
        review_after: resolve_duration(
            "backup.review_after",
            overrides,
            env,
            file_raw(file.as_ref(), "backup", "review_after"),
            Some("48h"),
            &file_display,
            &mut provenance,
        )?,
        destination: backup_destination,
    };

    let drill_result = file
        .as_ref()
        .and_then(|t| t.get("drill"))
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("result"))
        .and_then(toml::Value::as_str)
        .map(PathBuf::from);
    if drill_result.is_some() {
        provenance.set("drill.result", ConfigSource::File);
    }
    let drill = DrillConfig {
        max_age: resolve_duration(
            "drill.max_age",
            overrides,
            env,
            file_raw(file.as_ref(), "drill", "max_age"),
            Some("30d"),
            &file_display,
            &mut provenance,
        )?,
        result: drill_result,
    };

    let adapter_semantics = AdapterSemanticsConfig {
        max_comparison_age: resolve_duration(
            "adapter_semantics.max_comparison_age",
            overrides,
            env,
            file_raw(file.as_ref(), "adapter_semantics", "max_comparison_age"),
            Some("30d"),
            &file_display,
            &mut provenance,
        )?,
    };

    let doctor = DoctorConfig {
        meter_anomaly_horizon: resolve_duration(
            "doctor.meter_anomaly_horizon",
            overrides,
            env,
            file_raw(file.as_ref(), "doctor", "meter_anomaly_horizon"),
            Some("15m"),
            &file_display,
            &mut provenance,
        )?,
    };

    let valuation = ValuationConfig {
        default_rate_book: file
            .as_ref()
            .and_then(|t| t.get("valuation"))
            .and_then(toml::Value::as_table)
            .and_then(|t| t.get("default_rate_book"))
            .and_then(toml::Value::as_str)
            .map(str::to_string),
    };
    if valuation.default_rate_book.is_some() {
        provenance.set("valuation.default_rate_book", ConfigSource::File);
    }

    let tracker = match file
        .as_ref()
        .and_then(|t| t.get("tracker"))
        .and_then(toml::Value::as_table)
    {
        Some(t) => {
            let kind = t
                .get("kind")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| missing_key_error("tracker.kind", &file_display))?;
            let path = t.get("path").and_then(toml::Value::as_str).unwrap_or("");
            provenance.set("tracker.kind", ConfigSource::File);
            // Tracked alongside the kind (aub-ukh5): `aub config` prints every
            // key the resolver knows, and the tracker path is one of them.
            provenance.set("tracker.path", ConfigSource::File);
            Some(TrackerConfig {
                kind: kind.to_string(),
                path: PathBuf::from(path),
            })
        }
        None => None,
    };

    let accounts: Vec<AccountConfig> = file
        .as_ref()
        .and_then(|t| t.get("accounts"))
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(toml::Value::as_table)
                .map(|entry| {
                    let credential = entry.get("credential").and_then(toml::Value::as_table);
                    let exclusivity_policy = match entry.get("exclusivity_policy") {
                        Some(val) => {
                            let raw = val.as_str().ok_or_else(|| {
                                Error::Usage(
                                    "accounts[].exclusivity_policy must be a string".to_string(),
                                )
                            })?;
                            AccountExclusivityPolicy::parse(raw).ok_or_else(|| {
                                Error::Usage(format!(
                                    "accounts[].exclusivity_policy: {raw:?} is not a recognized exclusivity policy"
                                ))
                            })?
                        }
                        None => AccountExclusivityPolicy::ForbidPassive,
                    };
                    let codex_home = match entry.get("codex_home") {
                        Some(val) => {
                            let raw = val.as_str().ok_or_else(|| {
                                Error::Usage("accounts[].codex_home must be a string".to_string())
                            })?;
                            if raw.trim().is_empty() {
                                return Err(Error::Usage(
                                    "accounts[].codex_home must not be empty".to_string(),
                                ));
                            }
                            // The key names one provider's source, so it
                            // belongs only to a provider = "codex" account; a
                            // Codex account without one is transcript-only,
                            // which stays legal.
                            if entry.get("provider").and_then(toml::Value::as_str) != Some("codex")
                            {
                                return Err(Error::Usage(format!(
                                    "accounts[].codex_home belongs to a provider = \"codex\" account; account '{}' has provider '{}'",
                                    entry
                                        .get("name")
                                        .and_then(toml::Value::as_str)
                                        .unwrap_or_default(),
                                    entry
                                        .get("provider")
                                        .and_then(toml::Value::as_str)
                                        .unwrap_or_default(),
                                )));
                            }
                            Some(PathBuf::from(raw))
                        }
                        None => None,
                    };
                    Ok(AccountConfig {
                        name: entry
                            .get("name")
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        provider: entry
                            .get("provider")
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        credential_kind: credential
                            .and_then(|c| c.get("kind"))
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        credential_detail: credential
                            .and_then(|c| {
                                c.get("ref")
                                    .or_else(|| c.get("path"))
                                    .or_else(|| c.get("name"))
                            })
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        exclusivity_policy,
                        codex_home,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()
        })
        .transpose()?
        .unwrap_or_default();
    if !accounts.is_empty() {
        provenance.set("accounts", ConfigSource::File);
    }

    let transcripts: Vec<TranscriptConfig> = file
        .as_ref()
        .and_then(|t| t.get("transcripts"))
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(toml::Value::as_table)
                .map(|entry| TranscriptConfig {
                    name: entry
                        .get("name")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    root: PathBuf::from(
                        entry
                            .get("root")
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default(),
                    ),
                    pattern: entry
                        .get("pattern")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    format: entry
                        .get("format")
                        .and_then(toml::Value::as_str)
                        .map(str::to_string),
                    usage_evidence: entry
                        .get("usage_evidence")
                        .and_then(toml::Value::as_str)
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    if !transcripts.is_empty() {
        provenance.set("transcripts", ConfigSource::File);
    }

    let projects = alias_table_from_file(file.as_ref(), "projects")?;
    if projects.entries().next().is_some() {
        provenance.set("projects", ConfigSource::File);
    }
    let repositories = alias_table_from_file(file.as_ref(), "repositories")?;
    if repositories.entries().next().is_some() {
        provenance.set("repositories", ConfigSource::File);
    }

    Ok((
        Config {
            state,
            sampling,
            ingest,
            freshness,
            coverage,
            attribution,
            task_distribution,
            can_run,
            reconciliation,
            accounts,
            transcripts,
            tracker,
            valuation,
            backup,
            drill,
            adapter_semantics,
            doctor,
            projects,
            repositories,
        },
        provenance,
    ))
}

/// One printed `aub config` row (aub-ukh5): the dotted key, the resolved value
/// already rendered to text, and the source that won for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProvenanceRow {
    pub key: String,
    pub value: String,
    pub source: ConfigSource,
}

/// The value column's total width cap (aub-ukh5): the column is the longest
/// rendered value plus the 2-space gap, capped here. Longer values are
/// truncated with `…` by [`fit_provenance_value`].
const PROVENANCE_VALUE_COLUMN_CAP: usize = 48;

impl Config {
    /// Every row `aub config` prints (aub-ukh5): one per scalar the resolver
    /// knows, with its rendered value and winning source, plus the array
    /// sections expanded one row per element and field
    /// (`accounts[0].name`, `transcripts[1].root`, ...). The heterogeneous
    /// section buckets (`accounts`, `transcripts`, `projects`,
    /// `repositories`) never appear as rows themselves: their entries do.
    ///
    /// Rows follow `provenance.entries()` order, which is today's printed
    /// order, so rows stay sorted as today inside every section. An absent
    /// optional (`attribution.quality_floor`, `backup.destination`,
    /// `drill.result`, `valuation.default_rate_book`, an unset transcript
    /// `format`) has no row: there is no value to print.
    pub fn provenance_rows(&self, provenance: &Provenance) -> Vec<ConfigProvenanceRow> {
        let mut rows = Vec::new();
        for (key, source) in provenance.entries() {
            match key {
                "accounts" => push_account_provenance_rows(&mut rows, &self.accounts, source),
                "transcripts" => {
                    push_transcript_provenance_rows(&mut rows, &self.transcripts, source);
                }
                "projects" => {
                    push_alias_provenance_rows(&mut rows, "projects", &self.projects, source)
                }
                "repositories" => {
                    push_alias_provenance_rows(
                        &mut rows,
                        "repositories",
                        &self.repositories,
                        source,
                    );
                }
                _ => {
                    // A provenance key this match does not name is a key
                    // `resolve` learned without this rendering following it;
                    // the golden test pins every current key, so a new one
                    // fails there until its row is added here.
                    if let Some(value) = self.scalar_provenance_value(key) {
                        rows.push(ConfigProvenanceRow {
                            key: key.to_string(),
                            value,
                            source,
                        });
                    }
                }
            }
        }
        rows
    }

    /// The full `aub config` text (aub-ukh5): three aligned columns (key,
    /// value, source) with the key column sized to the longest key plus the
    /// 2-space gap, one blank line between sections, no headers. Every
    /// non-blank line starts its source at the same offset.
    pub fn render_provenance(&self, provenance: &Provenance) -> String {
        render_provenance_rows(&self.provenance_rows(provenance))
    }

    /// The rendered value for one scalar provenance key, or `None` for an
    /// unset optional the output skips. Values render through each typed
    /// quantity's `Display` (durations via [`format_config_duration`], the
    /// domain quantity that deliberately carries no `Display` of its own);
    /// paths render verbatim.
    fn scalar_provenance_value(&self, key: &str) -> Option<String> {
        let value = match key {
            "state.dir" => self.state.dir.display().to_string(),
            "sampling.scheduler_tick" => format_config_duration(self.sampling.scheduler_tick),
            "sampling.default_interval" => format_config_duration(self.sampling.default_interval),
            "sampling.reset_edge_lead" => format_config_duration(self.sampling.reset_edge_lead),
            "sampling.request_timeout" => format_config_duration(self.sampling.request_timeout),
            "sampling.busy_timeout" => format_config_duration(self.sampling.busy_timeout),
            "sampling.command_budget" => format_config_duration(self.sampling.command_budget),
            "sampling.max_concurrent_requests" => self.sampling.max_concurrent_requests.to_string(),
            "ingest.max_batch_events" => self.ingest.max_batch_events.to_string(),
            "ingest.max_batch_files" => self.ingest.max_batch_files.to_string(),
            "ingest.max_batch_seconds" => format_config_duration(self.ingest.max_batch_seconds),
            "freshness.meter" => format_config_duration(self.freshness.meter),
            "coverage.attempt_floor" => self.coverage.attempt_floor.to_string(),
            "coverage.measurement_floor" => self.coverage.measurement_floor.to_string(),
            "attribution.recent_window" => format_config_duration(self.attribution.recent_window),
            "attribution.quality_floor" => self.attribution.quality_floor?.to_string(),
            "task_distribution.central_low" => self.task_distribution.central_low.to_string(),
            "task_distribution.central_high" => self.task_distribution.central_high.to_string(),
            "task_distribution.upper" => self.task_distribution.upper.to_string(),
            "task_distribution.min_samples" => self.task_distribution.min_samples.to_string(),
            "task_distribution.quantile_method" => {
                self.task_distribution.quantile_method.to_string()
            }
            "task_distribution.attribution_floor" => {
                self.task_distribution.attribution_floor.to_string()
            }
            "can_run.labels" => self.can_run.labels_enabled.to_string(),
            "can_run.ample_margin_multiple" => self.can_run.ample_margin_multiple.to_string(),
            "can_run.headroom_bound" => self.can_run.headroom_bound.to_string(),
            "reconciliation.residual_window" => {
                format_config_duration(self.reconciliation.residual_window)
            }
            "reconciliation.residual_min_eligible" => {
                self.reconciliation.residual_min_eligible.to_string()
            }
            "backup.review_after" => format_config_duration(self.backup.review_after),
            "backup.destination" => self.backup.destination.as_ref()?.display().to_string(),
            "drill.max_age" => format_config_duration(self.drill.max_age),
            "drill.result" => self.drill.result.as_ref()?.display().to_string(),
            "adapter_semantics.max_comparison_age" => {
                format_config_duration(self.adapter_semantics.max_comparison_age)
            }
            "doctor.meter_anomaly_horizon" => {
                format_config_duration(self.doctor.meter_anomaly_horizon)
            }
            "tracker.kind" => self.tracker.as_ref()?.kind.clone(),
            "tracker.path" => self.tracker.as_ref()?.path.display().to_string(),
            "valuation.default_rate_book" => self.valuation.default_rate_book.clone()?,
            _ => return None,
        };
        Some(value)
    }
}

/// One account's expanded rows (aub-ukh5): name, provider, credential and
/// exclusivity policy, in key order. The credential renders as `file:<path>`,
/// `env:<NAME>` or `none` from its kind and reference only: the material is
/// never read here, so no byte of any credential file can reach the output
/// through this path.
fn push_account_provenance_rows(
    rows: &mut Vec<ConfigProvenanceRow>,
    accounts: &[AccountConfig],
    source: ConfigSource,
) {
    for (index, account) in accounts.iter().enumerate() {
        let base = format!("accounts[{index}]");
        let mut entry = vec![
            (
                format!("{base}.credential"),
                render_account_credential(&account.credential_kind, &account.credential_detail),
            ),
            (
                format!("{base}.exclusivity_policy"),
                account.exclusivity_policy.to_string(),
            ),
            (format!("{base}.name"), account.name.clone()),
            (format!("{base}.provider"), account.provider.clone()),
        ];
        // The optional meter home prints only when set, like every other
        // optional field in this output: an unset key is never invented.
        if let Some(home) = &account.codex_home {
            entry.push((format!("{base}.codex_home"), home.display().to_string()));
        }
        entry.sort();
        for (key, value) in entry {
            rows.push(ConfigProvenanceRow { key, value, source });
        }
    }
}

/// Renders one account credential reference without touching the material
/// (aub-ukh5): the kind and the path, variable name or profile reference it
/// names, never the secret itself.
fn render_account_credential(kind: &str, detail: &str) -> String {
    if kind == "none" || (kind.is_empty() && detail.is_empty()) {
        "none".to_string()
    } else if kind.is_empty() {
        detail.to_string()
    } else if detail.is_empty() {
        format!("{kind}:")
    } else {
        format!("{kind}:{detail}")
    }
}

/// One transcript source's expanded rows (aub-ukh5): name, root, pattern and
/// whichever optional fields are set, in key order.
fn push_transcript_provenance_rows(
    rows: &mut Vec<ConfigProvenanceRow>,
    transcripts: &[TranscriptConfig],
    source: ConfigSource,
) {
    for (index, transcript) in transcripts.iter().enumerate() {
        let base = format!("transcripts[{index}]");
        let mut entry = vec![
            (format!("{base}.name"), transcript.name.clone()),
            (
                format!("{base}.root"),
                transcript.root.display().to_string(),
            ),
            (format!("{base}.pattern"), transcript.pattern.clone()),
        ];
        if let Some(format) = &transcript.format {
            entry.push((format!("{base}.format"), format.clone()));
        }
        if let Some(evidence) = &transcript.usage_evidence {
            entry.push((format!("{base}.usage_evidence"), evidence.clone()));
        }
        entry.sort();
        for (key, value) in entry {
            rows.push(ConfigProvenanceRow { key, value, source });
        }
    }
}

/// One alias table's expanded rows (aub-ukh5): `section.<path>` to the
/// logical name it maps to, in path order.
fn push_alias_provenance_rows(
    rows: &mut Vec<ConfigProvenanceRow>,
    section: &str,
    table: &AliasTable,
    source: ConfigSource,
) {
    for (path, name) in table.entries() {
        rows.push(ConfigProvenanceRow {
            key: format!("{section}.{path}"),
            value: name.to_string(),
            source,
        });
    }
}

/// The key column width (aub-ukh5): the longest key plus the 2-space gap, so
/// the value column starts at one offset on every row.
fn provenance_key_width(rows: &[ConfigProvenanceRow]) -> usize {
    rows.iter()
        .map(|row| row.key.chars().count())
        .max()
        .unwrap_or(0)
        + 2
}

/// The value column width (aub-ukh5): the longest rendered value plus the
/// 2-space gap, capped at [`PROVENANCE_VALUE_COLUMN_CAP`].
fn provenance_value_width(rows: &[ConfigProvenanceRow]) -> usize {
    let longest = rows
        .iter()
        .map(|row| row.value.chars().count())
        .max()
        .unwrap_or(0);
    (longest + 2).min(PROVENANCE_VALUE_COLUMN_CAP)
}

/// Fits one rendered value into a value column of `column_width`: values past
/// the column's content budget (its width minus the 2-space gap) are
/// truncated with `…`. Widths count characters, never bytes, so the source
/// column that follows still starts at one offset.
fn fit_provenance_value(value: &str, column_width: usize) -> String {
    let budget = column_width.saturating_sub(2);
    if value.chars().count() <= budget {
        value.to_string()
    } else {
        let kept: String = value.chars().take(budget.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Pads text with spaces to exactly `width` characters, counting characters
/// rather than bytes so a multibyte tail (the `…` truncation marker) cannot
/// shift the column that follows.
fn pad_provenance_column(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// Renders already-built rows (aub-ukh5): sections are the first key segment
/// (up to the first `.` or `[`), rows keep their incoming order inside a
/// section, sections are separated by one blank line, and there are no
/// headers. Every non-blank line starts its source at the same offset.
fn render_provenance_rows(rows: &[ConfigProvenanceRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let key_width = provenance_key_width(rows);
    let value_width = provenance_value_width(rows);
    let mut out = String::new();
    let mut current_section: Option<&str> = None;
    for row in rows {
        let section = row.key.split(['.', '[']).next().unwrap_or(row.key.as_str());
        if current_section.is_some_and(|current| current != section) {
            out.push('\n');
        }
        current_section = Some(section);
        out.push_str(&pad_provenance_column(&row.key, key_width));
        out.push_str(&pad_provenance_column(
            &fit_provenance_value(&row.value, value_width),
            value_width,
        ));
        out.push_str(row.source.label());
        out.push('\n');
    }
    out
}

/// Reads one alias section (`projects` or `repositories`) from the file into a
/// validated [`AliasTable`]. File-only, like the other heterogeneous sections:
/// overriding a path-to-name mapping through one `--set` string is not a
/// well-formed operation.
fn alias_table_from_file(file: Option<&toml::Table>, section: &str) -> Result<AliasTable, Error> {
    let Some(table) = file
        .and_then(|t| t.get(section))
        .and_then(toml::Value::as_table)
    else {
        return Ok(AliasTable::default());
    };
    let mut entries = BTreeMap::new();
    for (path, value) in table {
        let name = value.as_str().ok_or_else(|| {
            Error::Usage(format!("{section}.{path}: alias value must be a string"))
        })?;
        entries.insert(path.clone(), name.to_string());
    }
    AliasTable::new(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_with(
        overrides: Overrides,
        env: FakeEnv,
        file_contents: Option<&str>,
    ) -> Result<(Config, Provenance), Error> {
        resolve(&overrides, &env, file_contents, "/test/aub.toml")
    }

    fn plain_env() -> FakeEnv {
        FakeEnv::new().set("HOME", "/home/synthetic-user")
    }

    // --- resolution order: each level checked in BOTH directions ------------------

    #[test]
    fn flag_wins_over_everything_below_it() {
        let overrides = Overrides::new().set("sampling.default_interval", "9m");
        let env = plain_env().set("AUB_SAMPLING_DEFAULT_INTERVAL", "7m");
        let file = "[sampling]\ndefault_interval = \"3m\"\n";
        let (config, provenance) = resolve_with(overrides, env, Some(file)).unwrap();
        assert_eq!(
            config.sampling.default_interval.as_nanos(),
            9 * 60 * 1_000_000_000
        );
        assert_eq!(
            provenance.get("sampling.default_interval"),
            Some(ConfigSource::Flag)
        );
    }

    #[test]
    fn environment_wins_when_no_flag_is_set() {
        let env = plain_env().set("AUB_SAMPLING_DEFAULT_INTERVAL", "7m");
        let file = "[sampling]\ndefault_interval = \"3m\"\n";
        let (config, provenance) = resolve_with(Overrides::new(), env, Some(file)).unwrap();
        assert_eq!(
            config.sampling.default_interval.as_nanos(),
            7 * 60 * 1_000_000_000
        );
        assert_eq!(
            provenance.get("sampling.default_interval"),
            Some(ConfigSource::Environment)
        );
    }

    #[test]
    fn file_wins_when_no_flag_or_environment_is_set() {
        let file = "[sampling]\ndefault_interval = \"3m\"\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.sampling.default_interval.as_nanos(),
            3 * 60 * 1_000_000_000
        );
        assert_eq!(
            provenance.get("sampling.default_interval"),
            Some(ConfigSource::File)
        );
    }

    #[test]
    fn default_wins_when_nothing_else_is_set() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(
            config.sampling.default_interval.as_nanos(),
            5 * 60 * 1_000_000_000
        );
        assert_eq!(
            provenance.get("sampling.default_interval"),
            Some(ConfigSource::Default)
        );
    }

    // --- unknown key: checked in both directions -----------------------------------

    #[test]
    fn an_unknown_key_is_a_named_error() {
        let file = "[sampling]\nnonexistent_key = \"3m\"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("sampling.nonexistent_key"), "{message}");
    }

    #[test]
    fn a_known_key_in_the_same_section_is_not_rejected() {
        let file = "[sampling]\ndefault_interval = \"3m\"\n";
        assert!(resolve_with(Overrides::new(), plain_env(), Some(file)).is_ok());
    }

    // --- missing required key: checked in both directions --------------------------

    #[test]
    fn a_tracker_section_with_no_kind_is_a_missing_key_error_naming_the_file() {
        let file = "[tracker]\npath = \"~/work/.tracker\"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        let message = err.to_string();
        assert!(message.contains("tracker.kind"), "{message}");
        assert!(message.contains("/test/aub.toml"), "{message}");
    }

    #[test]
    fn a_tracker_section_with_a_kind_resolves_successfully() {
        let file = "[tracker]\nkind = \"local\"\npath = \"~/work/.tracker\"\n";
        let (config, _) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.tracker.unwrap().kind, "local");
    }

    #[test]
    fn no_tracker_section_at_all_is_not_a_missing_key_error() {
        let (config, _) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert!(config.tracker.is_none());
    }

    // --- the concurrency bound ------------------------------------------------------

    /// The documented small default: bounded concurrency is configuration,
    /// not a constant, and its default is two.
    #[test]
    fn the_concurrency_bound_defaults_to_two() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(config.sampling.max_concurrent_requests, 2);
        assert_eq!(
            provenance.get("sampling.max_concurrent_requests"),
            Some(ConfigSource::Default)
        );
    }

    #[test]
    fn the_concurrency_bound_resolves_from_the_file_and_the_environment() {
        let file = "[sampling]\nmax_concurrent_requests = 4\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.sampling.max_concurrent_requests, 4);
        assert_eq!(
            provenance.get("sampling.max_concurrent_requests"),
            Some(ConfigSource::File)
        );

        let env = plain_env().set("AUB_SAMPLING_MAX_CONCURRENT_REQUESTS", "6");
        let (config, provenance) = resolve_with(Overrides::new(), env, Some(file)).unwrap();
        assert_eq!(config.sampling.max_concurrent_requests, 6);
        assert_eq!(
            provenance.get("sampling.max_concurrent_requests"),
            Some(ConfigSource::Environment)
        );
    }

    /// Planted negative: a bound of zero would sample nothing while reporting
    /// a completed batch, so it is refused at resolution time.
    #[test]
    fn a_zero_concurrency_bound_is_a_named_usage_error() {
        let file = "[sampling]\nmax_concurrent_requests = 0\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        assert!(
            err.to_string().contains("sampling.max_concurrent_requests"),
            "the refusal must name the key: {err}"
        );
    }

    #[test]
    fn a_non_numeric_concurrency_bound_is_a_named_usage_error() {
        let file = "[sampling]\nmax_concurrent_requests = \"many\"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        assert!(err.to_string().contains("not a whole number"), "{err}");
    }

    // --- no compiled identity -------------------------------------------------------

    #[test]
    fn state_dir_default_is_derived_from_the_injected_home_not_a_compiled_path() {
        let env = FakeEnv::new().set("HOME", "/tmp/synthetic-home-alpha");
        let (config, provenance) = resolve_with(Overrides::new(), env, None).unwrap();
        assert_eq!(
            config.state.dir,
            PathBuf::from("/tmp/synthetic-home-alpha/.local/state/aub")
        );
        assert_eq!(provenance.get("state.dir"), Some(ConfigSource::Default));
    }

    /// Property: over every scalar default, resolved under several different
    /// synthetic environments, none contains the real process's actual $HOME or
    /// username - proving the code path is genuinely driven by the injected
    /// environment rather than falling back to a real, compiled-in, or
    /// process-inherited value under any of them.
    #[test]
    fn defaults_never_contain_the_real_process_home_or_username() {
        let real_home = std::env::var("HOME").unwrap_or_default();
        let real_user = std::env::var("USER").unwrap_or_default();

        let synthetic_environments = [
            ("/tmp/synthetic-home-alpha", "alpha-user"),
            ("/tmp/synthetic-home-beta", "beta-person"),
            ("/nonexistent/totally-fake-home", "ghost"),
        ];

        for (fake_home, fake_user) in synthetic_environments {
            let env = FakeEnv::new().set("HOME", fake_home).set("USER", fake_user);
            let (config, _) = resolve_with(Overrides::new(), env, None).unwrap();
            let rendered = format!("{config:?}");

            if !real_home.is_empty() {
                assert!(
                    !rendered.contains(&real_home),
                    "resolved defaults under a synthetic HOME contained the real HOME: {rendered}"
                );
            }
            if !real_user.is_empty() {
                assert!(
                    !rendered.contains(&real_user),
                    "resolved defaults under a synthetic USER contained the real USER: {rendered}"
                );
            }
            assert!(rendered.contains(fake_home), "{rendered}");
        }
    }

    // --- everything else the model covers -------------------------------------------

    #[test]
    fn accounts_and_transcripts_are_populated_from_the_file() {
        let file = r#"
[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "profile", ref = "work-primary" }

[[transcripts]]
name = "cli-a"
root = "~/.local/share/cli-a"
pattern = "**/*.jsonl"
"#;
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "work-primary");
        assert_eq!(config.accounts[0].credential_kind, "profile");
        assert_eq!(config.accounts[0].credential_detail, "work-primary");
        assert_eq!(config.transcripts.len(), 1);
        assert_eq!(config.transcripts[0].pattern, "**/*.jsonl");
        assert_eq!(provenance.get("accounts"), Some(ConfigSource::File));
    }

    /// The ingest batch bound defaults, is file-overridable and is provenance-
    /// tracked like every other scalar section; a zero or non-numeric value is a
    /// usage error naming the key, never a silently degenerate batch bound.
    #[test]
    fn ingest_batch_bound_resolves_and_refuses_zero_or_garbage() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(config.ingest.max_batch_events, 5000);
        assert_eq!(
            provenance.get("ingest.max_batch_events"),
            Some(ConfigSource::Default)
        );

        let file = "\n[ingest]\nmax_batch_events = 250\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.ingest.max_batch_events, 250);
        assert_eq!(
            provenance.get("ingest.max_batch_events"),
            Some(ConfigSource::File)
        );

        let zero = "\n[ingest]\nmax_batch_events = 0\n";
        let error = resolve_with(Overrides::new(), plain_env(), Some(zero)).unwrap_err();
        assert!(
            error.to_string().contains("ingest.max_batch_events"),
            "{error}"
        );

        let garbage = "\n[ingest]\nmax_batch_events = \"lots\"\n";
        let error = resolve_with(Overrides::new(), plain_env(), Some(garbage)).unwrap_err();
        assert!(
            error.to_string().contains("ingest.max_batch_events"),
            "{error}"
        );
    }

    /// The file-count batch bound (`aub-va6s`): same four-level resolution and
    /// the same refusal of zero or garbage as `max_batch_events`, checked
    /// independently because the two bound different axes of one batch.
    #[test]
    fn ingest_batch_file_bound_resolves_and_refuses_zero_or_garbage() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(config.ingest.max_batch_files, 200);
        assert_eq!(
            provenance.get("ingest.max_batch_files"),
            Some(ConfigSource::Default)
        );

        let file = "\n[ingest]\nmax_batch_files = 3\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.ingest.max_batch_files, 3);
        assert_eq!(
            provenance.get("ingest.max_batch_files"),
            Some(ConfigSource::File)
        );

        let zero = "\n[ingest]\nmax_batch_files = 0\n";
        let error = resolve_with(Overrides::new(), plain_env(), Some(zero)).unwrap_err();
        assert!(
            error.to_string().contains("ingest.max_batch_files"),
            "{error}"
        );

        let garbage = "\n[ingest]\nmax_batch_files = \"lots\"\n";
        let error = resolve_with(Overrides::new(), plain_env(), Some(garbage)).unwrap_err();
        assert!(
            error.to_string().contains("ingest.max_batch_files"),
            "{error}"
        );
    }

    /// `ingest.max_batch_seconds` (`aub-mh1c`) resolves to a 2-second default,
    /// takes a file override, and refuses garbage naming the key: the same
    /// contract `max_batch_events` and `max_batch_files` already carry. The
    /// planted negative is a resolver that silently ignores the file value
    /// and always reports the default, which the file-override assertion
    /// below would still catch even though the default-only assertion would
    /// not.
    #[test]
    fn ingest_batch_seconds_bound_resolves_default_and_from_file() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(
            config.ingest.max_batch_seconds,
            MonotonicDuration::from_seconds(2)
        );
        assert_eq!(
            provenance.get("ingest.max_batch_seconds"),
            Some(ConfigSource::Default)
        );

        let file = "\n[ingest]\nmax_batch_seconds = \"5s\"\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.ingest.max_batch_seconds,
            MonotonicDuration::from_seconds(5)
        );
        assert_eq!(
            provenance.get("ingest.max_batch_seconds"),
            Some(ConfigSource::File)
        );

        let garbage = "\n[ingest]\nmax_batch_seconds = \"lots\"\n";
        let error = resolve_with(Overrides::new(), plain_env(), Some(garbage)).unwrap_err();
        assert!(
            error.to_string().contains("ingest.max_batch_seconds"),
            "{error}"
        );
    }

    #[test]
    fn a_credential_kind_with_an_unexpected_field_is_accepted_by_this_bead() {
        // Deliberate scope boundary, exercised rather than merely stated: the full
        // credential shape belongs to aub-eun.1. A "profile" credential missing its
        // own `ref` (or carrying an extra field under a kind this bead does not
        // model) is not rejected here.
        let file = r#"
[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "unknown-future-kind", anything = "goes" }
"#;
        assert!(resolve_with(Overrides::new(), plain_env(), Some(file)).is_ok());
    }

    #[test]
    fn an_env_credential_table_resolves_to_the_variable_name() {
        let file = r#"
[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "env", name = "AUB_TEST_TOKEN" }
"#;
        let (config, _) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.accounts[0].credential_kind, "env");
        assert_eq!(config.accounts[0].credential_detail, "AUB_TEST_TOKEN");
    }

    #[test]
    fn an_env_credential_table_with_an_unknown_key_is_rejected() {
        // The planted negative: an `env` table carrying a `path` key must be
        // rejected the same way a `file` table carrying `name` would be. A
        // resolver that accepted any key set would silently read the wrong
        // variable when an operator renamed a key instead of moving it.
        let file = r#"
[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "env", name = "AUB_TEST_TOKEN", path = "elsewhere" }
"#;
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(
            err.to_string().contains("accounts[].credential.path"),
            "{err}"
        );
    }

    #[test]
    fn an_env_credential_with_an_empty_name_parses_but_fails_at_resolution() {
        // The shape is valid TOML and a valid credential table, so the config
        // layer accepts it and credential resolution is what names the account
        // and the key `name` as the missing piece.
        let file = r#"
[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "env", name = "" }
"#;
        let (config, _) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        let err = crate::auth::CredentialSource::from_account(&config.accounts[0]).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("work-primary"), "{message}");
        assert!(message.contains("'name'"), "{message}");
    }

    #[test]
    fn an_invalid_toml_file_is_a_usage_error() {
        let err =
            resolve_with(Overrides::new(), plain_env(), Some("not valid toml =")).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
    }

    #[test]
    fn a_coverage_floor_out_of_range_is_a_usage_error() {
        let file = "[coverage]\nattempt_floor = 1.5\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
    }

    #[test]
    fn a_coverage_floor_in_range_resolves_successfully() {
        let file = "[coverage]\nattempt_floor = 0.9\n";
        let (config, _) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.coverage.attempt_floor.get(), 0.9);
    }

    #[test]
    fn the_attribution_quality_floor_is_absent_by_default_and_set_from_the_file() {
        let (default_config, provenance) =
            resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert!(
            default_config.attribution.quality_floor.is_none(),
            "no floor until an operator configures one"
        );
        assert!(provenance.get("attribution.quality_floor").is_none());
        // The window still has a default and is provenance-tracked.
        assert_eq!(
            provenance.get("attribution.recent_window"),
            Some(ConfigSource::Default)
        );

        let file = "[attribution]\nquality_floor = 0.8\nrecent_window = \"14d\"\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.attribution.quality_floor.map(|f| f.ppm()),
            Some(800_000)
        );
        assert_eq!(
            provenance.get("attribution.quality_floor"),
            Some(ConfigSource::File)
        );
        assert_eq!(
            config.attribution.recent_window,
            MonotonicDuration::from_seconds(14 * 86_400)
        );
    }

    #[test]
    fn an_attribution_quality_floor_out_of_range_is_a_usage_error() {
        let file = "[attribution]\nquality_floor = 1.4\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
    }

    #[test]
    fn an_unknown_key_under_attribution_is_a_usage_error() {
        let file = "[attribution]\nnope = 1\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("attribution.nope"), "{err}");
    }

    #[test]
    fn the_task_distribution_policy_has_the_decided_defaults() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(config.task_distribution.central_low.value(), 25);
        assert_eq!(config.task_distribution.central_high.value(), 75);
        assert_eq!(config.task_distribution.upper.value(), 90);
        assert_eq!(config.task_distribution.min_samples, 12);
        assert_eq!(
            config.task_distribution.quantile_method,
            QuantileMethod::NearestRank
        );
        assert_eq!(config.task_distribution.attribution_floor.ppm(), 800_000);
        assert_eq!(
            provenance.get("task_distribution.min_samples"),
            Some(ConfigSource::Default)
        );
    }

    #[test]
    fn the_task_distribution_policy_is_set_from_the_file() {
        let file = "[task_distribution]\ncentral_low = 20\ncentral_high = 80\nupper = 95\nmin_samples = 20\nquantile_method = \"nearest-rank\"\nattribution_floor = 0.9\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(config.task_distribution.central_low.value(), 20);
        assert_eq!(config.task_distribution.central_high.value(), 80);
        assert_eq!(config.task_distribution.upper.value(), 95);
        assert_eq!(config.task_distribution.min_samples, 20);
        assert_eq!(config.task_distribution.attribution_floor.ppm(), 900_000);
        assert_eq!(
            provenance.get("task_distribution.central_low"),
            Some(ConfigSource::File)
        );
    }

    #[test]
    fn a_task_distribution_percentile_out_of_range_is_a_usage_error() {
        let file = "[task_distribution]\ncentral_low = 200\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
    }

    #[test]
    fn task_distribution_percentiles_out_of_order_is_a_usage_error() {
        let file = "[task_distribution]\ncentral_low = 80\ncentral_high = 25\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("central_low"), "{err}");
    }

    #[test]
    fn an_unrecognized_task_distribution_quantile_method_is_a_usage_error() {
        let file = "[task_distribution]\nquantile_method = \"linear-interpolation\"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("quantile_method"), "{err}");
    }

    #[test]
    fn an_unknown_key_under_task_distribution_is_a_usage_error() {
        let file = "[task_distribution]\nnope = 1\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("task_distribution.nope"), "{err}");
    }

    #[test]
    fn the_reconciliation_residual_policy_has_defaults_and_is_set_from_the_file() {
        let (default_config, provenance) =
            resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(
            default_config.reconciliation.residual_window,
            MonotonicDuration::from_seconds(30 * 86_400)
        );
        assert_eq!(default_config.reconciliation.residual_min_eligible, 5);
        assert_eq!(
            provenance.get("reconciliation.residual_window"),
            Some(ConfigSource::Default)
        );
        assert_eq!(
            provenance.get("reconciliation.residual_min_eligible"),
            Some(ConfigSource::Default)
        );

        let file = "[reconciliation]\nresidual_window = \"14d\"\nresidual_min_eligible = 8\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.reconciliation.residual_window,
            MonotonicDuration::from_seconds(14 * 86_400)
        );
        assert_eq!(config.reconciliation.residual_min_eligible, 8);
        assert_eq!(
            provenance.get("reconciliation.residual_min_eligible"),
            Some(ConfigSource::File)
        );
    }

    #[test]
    fn a_zero_residual_minimum_is_a_usage_error() {
        let file = "[reconciliation]\nresidual_min_eligible = 0\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
    }

    #[test]
    fn an_unknown_key_under_reconciliation_is_a_usage_error() {
        let file = "[reconciliation]\nnope = 1\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("reconciliation.nope"), "{err}");
    }

    #[test]
    fn the_doctor_anomaly_horizon_defaults_and_resolves_from_the_file() {
        let (default_config, provenance) =
            resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert_eq!(
            default_config.doctor.meter_anomaly_horizon,
            MonotonicDuration::from_seconds(15 * 60)
        );
        assert_eq!(
            provenance.get("doctor.meter_anomaly_horizon"),
            Some(ConfigSource::Default)
        );

        let file = "[doctor]\nmeter_anomaly_horizon = \"45m\"\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.doctor.meter_anomaly_horizon,
            MonotonicDuration::from_seconds(45 * 60)
        );
        assert_eq!(
            provenance.get("doctor.meter_anomaly_horizon"),
            Some(ConfigSource::File)
        );
    }

    #[test]
    fn an_unknown_key_under_doctor_is_a_usage_error() {
        let file = "[doctor]\nnope = 1\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("doctor.nope"), "{err}");
    }

    #[test]
    fn the_can_run_policy_has_the_jsq_decided_defaults() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        assert!(config.can_run.labels_enabled);
        assert_eq!(config.can_run.ample_margin_multiple.get(), 2.0);
        assert_eq!(config.can_run.headroom_bound, CanRunHeadroomBound::Low);
        assert_eq!(
            provenance.get("can_run.labels"),
            Some(ConfigSource::Default)
        );
        assert_eq!(
            provenance.get("can_run.ample_margin_multiple"),
            Some(ConfigSource::Default)
        );
        assert_eq!(
            provenance.get("can_run.headroom_bound"),
            Some(ConfigSource::Default)
        );
    }

    #[test]
    fn the_can_run_policy_is_set_from_the_file() {
        let file =
            "[can_run]\nlabels = false\nample_margin_multiple = 3.5\nheadroom_bound = \"low\"\n";
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert!(!config.can_run.labels_enabled);
        assert_eq!(config.can_run.ample_margin_multiple.get(), 3.5);
        assert_eq!(config.can_run.headroom_bound, CanRunHeadroomBound::Low);
        assert_eq!(provenance.get("can_run.labels"), Some(ConfigSource::File));
        assert_eq!(
            provenance.get("can_run.ample_margin_multiple"),
            Some(ConfigSource::File)
        );
    }

    #[test]
    fn a_non_boolean_can_run_labels_is_a_usage_error() {
        let file = "[can_run]\nlabels = \"yes\"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        assert!(err.to_string().contains("can_run.labels"), "{err}");
    }

    #[test]
    fn a_non_positive_can_run_multiple_is_a_usage_error() {
        for raw in ["0", "-1.5", "nan", "inf"] {
            let file = format!("[can_run]\nample_margin_multiple = {raw}\n");
            let err = resolve_with(Overrides::new(), plain_env(), Some(&file)).unwrap_err();
            assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
            assert!(
                err.to_string().contains("can_run.ample_margin_multiple"),
                "{err}"
            );
        }
    }

    #[test]
    fn an_unrecognized_can_run_headroom_bound_is_a_usage_error() {
        let file = "[can_run]\nheadroom_bound = \"high\"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        assert!(err.to_string().contains("can_run.headroom_bound"), "{err}");
    }

    #[test]
    fn an_unknown_key_under_can_run_is_a_usage_error() {
        let file = "[can_run]\nnope = 1\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert!(err.to_string().contains("can_run.nope"), "{err}");
    }

    #[test]
    fn account_exclusivity_policy_accepted_spellings() {
        let file = r#"
[[accounts]]
name = "work-permit"
provider = "anthropic"
exclusivity_policy = "permit_passive"

[[accounts]]
name = "work-forbid"
provider = "anthropic"
exclusivity_policy = "forbid_passive"
"#;
        let (config, _) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.accounts[0].exclusivity_policy,
            AccountExclusivityPolicy::PermitPassive
        );
        assert!(config.accounts[0].permits_passive_fitting());
        assert_eq!(
            config.accounts[1].exclusivity_policy,
            AccountExclusivityPolicy::ForbidPassive
        );
        assert!(!config.accounts[1].permits_passive_fitting());
    }

    #[test]
    fn account_exclusivity_policy_unknown_value_is_usage_error() {
        let file = r#"
[[accounts]]
name = "work"
provider = "anthropic"
exclusivity_policy = "shared"
"#;
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        assert!(
            err.to_string().contains("accounts[].exclusivity_policy"),
            "{err}"
        );
    }

    #[test]
    fn account_exclusivity_policy_absent_key_defaults_to_forbid_passive() {
        let file = r#"
[[accounts]]
name = "work"
provider = "anthropic"
"#;
        let (config, _) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.accounts[0].exclusivity_policy,
            AccountExclusivityPolicy::ForbidPassive
        );
        assert!(!config.accounts[0].permits_passive_fitting());
    }

    #[test]
    fn account_exclusivity_policy_parse_and_as_str() {
        assert_eq!(
            AccountExclusivityPolicy::PermitPassive.as_str(),
            "permit_passive"
        );
        assert_eq!(
            AccountExclusivityPolicy::ForbidPassive.as_str(),
            "forbid_passive"
        );
        assert_eq!(
            AccountExclusivityPolicy::parse("permit_passive"),
            Some(AccountExclusivityPolicy::PermitPassive)
        );
        assert_eq!(
            AccountExclusivityPolicy::parse("forbid_passive"),
            Some(AccountExclusivityPolicy::ForbidPassive)
        );
        assert_eq!(AccountExclusivityPolicy::parse("shared"), None);
        assert_eq!(AccountExclusivityPolicy::parse("dedicated"), None);
        assert_eq!(AccountExclusivityPolicy::parse(""), None);
    }

    #[test]
    fn account_exclusivity_alias_is_rejected_as_unknown_key() {
        let file = r#"
[[accounts]]
name = "work"
provider = "anthropic"
exclusivity = "permit_passive"
"#;
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        assert!(err.to_string().contains("exclusivity"), "{err}");
    }

    /// The codex meter home key resolves as a path on a codex account and
    /// reaches the provenance rows only when set (aub-cg6k).
    #[test]
    fn codex_home_resolves_and_prints_only_when_set() {
        let file = r#"
[[accounts]]
name = "codex-primary"
provider = "codex"
codex_home = "/home/user/.codex"
"#;
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap();
        assert_eq!(
            config.accounts[0]
                .codex_home
                .as_ref()
                .map(|home| home.display().to_string()),
            Some("/home/user/.codex".to_string())
        );
        let rows = config.provenance_rows(&provenance);
        let home = rows
            .iter()
            .find(|row| row.key == "accounts[0].codex_home")
            .expect("the set home prints as its own row");
        assert_eq!(home.value, "/home/user/.codex");

        // A codex account without one is transcript-only, legal, and prints
        // no row for a key nobody set.
        let transcript_only = "[[accounts]]\nname = \"work\"\nprovider = \"codex\"\n";
        let (config, provenance) =
            resolve_with(Overrides::new(), plain_env(), Some(transcript_only)).unwrap();
        assert!(config.accounts[0].codex_home.is_none());
        assert!(
            config
                .provenance_rows(&provenance)
                .iter()
                .all(|row| row.key != "accounts[0].codex_home")
        );
    }

    /// A `codex_home` belongs only to a provider = "codex" account: the key
    /// names one provider's source, so any other provider carrying it is a
    /// usage error naming the account (aub-cg6k).
    #[test]
    fn codex_home_on_a_non_codex_provider_is_rejected() {
        let file = r#"
[[accounts]]
name = "work"
provider = "anthropic"
codex_home = "/home/user/.codex"
"#;
        let err = resolve_with(Overrides::new(), plain_env(), Some(file)).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Usage);
        let message = err.to_string();
        assert!(message.contains("codex_home"), "{message}");
        assert!(message.contains("work"), "{message}");
    }

    /// A non-string or empty `codex_home` is a usage error naming the key,
    /// never a silently degenerate path.
    #[test]
    fn codex_home_refuses_non_string_and_empty_values() {
        let garbage = "[[accounts]]\nname = \"work\"\nprovider = \"codex\"\ncodex_home = 3\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(garbage)).unwrap_err();
        assert!(err.to_string().contains("codex_home"), "{err}");

        let empty = "[[accounts]]\nname = \"work\"\nprovider = \"codex\"\ncodex_home = \"  \"\n";
        let err = resolve_with(Overrides::new(), plain_env(), Some(empty)).unwrap_err();
        assert!(err.to_string().contains("codex_home"), "{err}");
    }

    // --- aub-ukh5: aligned key, value and source rows ---------------------------

    fn provenance_row(key: &str, value: &str) -> ConfigProvenanceRow {
        ConfigProvenanceRow {
            key: key.to_string(),
            value: value.to_string(),
            source: ConfigSource::Default,
        }
    }

    #[test]
    fn key_column_width_is_the_longest_key_plus_two() {
        let ten = "a".repeat(10);
        let rows = vec![provenance_row(&ten, "1s")];
        assert_eq!(provenance_key_width(&rows), 12);

        let thirty_two = "b".repeat(32);
        let rows = vec![provenance_row(&thirty_two, "1s")];
        assert_eq!(provenance_key_width(&rows), 34);

        let forty = "c".repeat(40);
        let rows = vec![provenance_row(&forty, "1s")];
        assert_eq!(provenance_key_width(&rows), 42);
    }

    #[test]
    fn a_short_key_does_not_shrink_the_column_below_the_longest() {
        // The planted negative: a width taken from any row but the longest
        // (here the first) would misalign the longest key's source.
        let rows = vec![
            provenance_row("state.dir", "/x"),
            provenance_row("reconciliation.residual_min_eligible", "5"),
        ];
        assert_eq!(
            provenance_key_width(&rows),
            "reconciliation.residual_min_eligible".len() + 2
        );
    }

    #[test]
    fn value_column_width_is_the_longest_value_plus_two_capped_at_48() {
        let rows = vec![provenance_row("state.dir", "short")];
        assert_eq!(provenance_value_width(&rows), "short".len() + 2);

        let long = "v".repeat(60);
        let rows = vec![provenance_row("state.dir", &long)];
        assert_eq!(provenance_value_width(&rows), 48);
    }

    #[test]
    fn values_past_the_cap_truncate_with_an_ellipsis() {
        let long = "v".repeat(60);
        let fitted = fit_provenance_value(&long, 48);
        assert_eq!(fitted.chars().count(), 46, "{fitted}");
        assert!(fitted.ends_with('…'), "{fitted}");
        assert_eq!(&fitted[..45], &long[..45], "{fitted}");

        let short = "12m";
        assert_eq!(fit_provenance_value(short, 48), "12m");
    }

    #[test]
    fn padding_counts_characters_never_bytes() {
        // A truncated value ends in `…` (three bytes, one character): the
        // column that follows must still start at the same offset.
        let padded = pad_provenance_column("12m", 8);
        assert_eq!(padded, "12m     ");
        let padded = pad_provenance_column("ab…", 8);
        assert_eq!(padded.chars().count(), 8, "{padded}");
        assert!(padded.ends_with("     "), "{padded}");
    }

    #[test]
    fn credential_renders_kind_and_reference_for_file_env_and_none() {
        assert_eq!(
            render_account_credential("file", "/creds/max.json"),
            "file:/creds/max.json"
        );
        assert_eq!(
            render_account_credential("env", "AUB_TOKEN"),
            "env:AUB_TOKEN"
        );
        assert_eq!(render_account_credential("none", ""), "none");
        assert_eq!(render_account_credential("", ""), "none");
    }

    #[test]
    fn credential_rendering_never_reads_the_material() {
        // A legacy kind the typed model no longer accepts still renders as
        // its kind and reference: the loose pass-through keeps resolving,
        // and only the reference (never file contents) reaches the row.
        assert_eq!(
            render_account_credential("profile", "work-primary"),
            "profile:work-primary"
        );
    }

    #[test]
    fn a_command_line_override_row_carries_the_override_source() {
        let overrides = Overrides::new().set("freshness.meter", "5m");
        let (config, provenance) = resolve_with(overrides, plain_env(), None).unwrap();
        let rows = config.provenance_rows(&provenance);
        let row = rows
            .iter()
            .find(|row| row.key == "freshness.meter")
            .expect("every resolved scalar has a row");
        assert_eq!(row.value, "5m");
        assert_eq!(row.source, ConfigSource::Flag);
        assert_eq!(row.source.label(), "override");
    }

    #[test]
    fn every_non_blank_line_starts_its_source_at_one_offset() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        let text = config.render_provenance(&provenance);
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines.is_empty());
        let key_width = provenance_key_width(&config.provenance_rows(&provenance));
        let value_width = provenance_value_width(&config.provenance_rows(&provenance));
        let offset = key_width + value_width;
        for line in &lines {
            if line.trim().is_empty() {
                continue;
            }
            let source = &line[offset..];
            assert!(
                ["override", "environment", "file", "default"].contains(&source),
                "line does not start its source at offset {offset}: {line:?}"
            );
        }
        // The planted negative: the longest key must hold its source at the
        // same offset, which a fixed 32-character column would break.
        let long = lines
            .iter()
            .find(|line| line.starts_with("sampling.max_concurrent_requests"))
            .expect("the long key is printed");
        assert_eq!(&long[offset..], "default", "{long:?}");
    }

    #[test]
    fn sections_are_separated_by_one_blank_line_with_no_headers() {
        let (config, provenance) = resolve_with(Overrides::new(), plain_env(), None).unwrap();
        let text = config.render_provenance(&provenance);
        assert!(!text.starts_with('\n'), "no leading blank line");
        assert!(!text.contains("\n\n\n"), "never two blank lines: {text:?}");
        let sections = text.split("\n\n").count();
        assert!(sections > 5, "sections are separated: {text:?}");
        // One blank line separates sampling from the next section.
        let sampling_block = text
            .split("\n\n")
            .find(|block| block.contains("sampling.scheduler_tick"))
            .expect("a sampling section");
        assert!(
            sampling_block
                .lines()
                .all(|line| line.starts_with("sampling.")),
            "one section per block: {sampling_block:?}"
        );
    }

    /// The golden rendering (aub-ukh5): two accounts, two transcript sources
    /// and one `[backup]` key, fixing the exact text including the blank
    /// lines between sections. Paths stay short on purpose so no value hits
    /// the 48-character truncation cap here; truncation is pinned by its own
    /// unit test above instead.
    const GOLDEN_TOML: &str = r#"
[backup]
review_after = "36h"
destination = "/tmp/aub-golden/backups"

[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "file", path = "/tmp/aub-golden/creds-primary.json" }

[[accounts]]
name = "work-secondary"
provider = "provider-b"
credential = { kind = "env", name = "AUB_GOLDEN_TOKEN" }
exclusivity_policy = "permit_passive"

[[transcripts]]
name = "cli-a"
root = "/tmp/aub-golden/cli-a"
pattern = "**/*.jsonl"
format = "claude-code"

[[transcripts]]
name = "cli-b"
root = "/tmp/aub-golden/cli-b"
pattern = "**/*.md"
format = "codex"
usage_evidence = "measured"
"#;

    #[test]
    fn golden_config_rendering_with_two_accounts_and_two_transcript_sources() {
        let (config, provenance) =
            resolve_with(Overrides::new(), plain_env(), Some(GOLDEN_TOML)).unwrap();
        let text = config.render_provenance(&provenance);
        // Frozen against the real binary's output for this fixture and
        // verified line by line: key column 38, value column 43, one blank
        // line between sections, sources at one offset.
        const EXPECTED: &str = r#"accounts[0].credential                file:/tmp/aub-golden/creds-primary.json  file
accounts[0].exclusivity_policy        forbid_passive                           file
accounts[0].name                      work-primary                             file
accounts[0].provider                  provider-a                               file
accounts[1].credential                env:AUB_GOLDEN_TOKEN                     file
accounts[1].exclusivity_policy        permit_passive                           file
accounts[1].name                      work-secondary                           file
accounts[1].provider                  provider-b                               file

adapter_semantics.max_comparison_age  30d                                      default

attribution.recent_window             30d                                      default

backup.destination                    /tmp/aub-golden/backups                  file
backup.review_after                   36h                                      file

can_run.ample_margin_multiple         2                                        default
can_run.headroom_bound                low                                      default
can_run.labels                        true                                     default

coverage.attempt_floor                0.98                                     default
coverage.measurement_floor            0.95                                     default

doctor.meter_anomaly_horizon          15m                                      default

drill.max_age                         30d                                      default

freshness.meter                       12m                                      default

ingest.max_batch_events               5000                                     default
ingest.max_batch_files                200                                      default
ingest.max_batch_seconds              2s                                       default

reconciliation.residual_min_eligible  5                                        default
reconciliation.residual_window        30d                                      default

sampling.busy_timeout                 10s                                      default
sampling.command_budget               8s                                       default
sampling.default_interval             5m                                       default
sampling.max_concurrent_requests      2                                        default
sampling.request_timeout              5s                                       default
sampling.reset_edge_lead              2m                                       default
sampling.scheduler_tick               1m                                       default

state.dir                             /home/synthetic-user/.local/state/aub    default

task_distribution.attribution_floor   0.8                                      default
task_distribution.central_high        75                                       default
task_distribution.central_low         25                                       default
task_distribution.min_samples         12                                       default
task_distribution.quantile_method     nearest-rank                             default
task_distribution.upper               90                                       default

transcripts[0].format                 claude-code                              file
transcripts[0].name                   cli-a                                    file
transcripts[0].pattern                **/*.jsonl                               file
transcripts[0].root                   /tmp/aub-golden/cli-a                    file
transcripts[1].format                 codex                                    file
transcripts[1].name                   cli-b                                    file
transcripts[1].pattern                **/*.md                                  file
transcripts[1].root                   /tmp/aub-golden/cli-b                    file
transcripts[1].usage_evidence         measured                                 file
"#;
        assert_eq!(text, EXPECTED);
    }

    #[test]
    fn rendered_output_contains_no_byte_of_any_credential_file() {
        // A real credential file holding a marker string, referenced by the
        // fixture config: the row prints the path, and the marker (the file's
        // contents, which resolution never reads) must not appear.
        let marker = "aub-ukh5-marker-never-printed-9f3c";
        let path = std::env::temp_dir().join(format!("aub-ukh5-cred-{}.json", std::process::id()));
        std::fs::write(&path, format!("{{\"token\": \"{marker}\"}}")).unwrap();
        let file = format!(
            "[[accounts]]\nname = \"work\"\nprovider = \"provider-a\"\ncredential = {{ kind = \"file\", path = {:?} }}\n",
            path.to_string_lossy()
        );
        let (config, provenance) =
            resolve_with(Overrides::new(), plain_env(), Some(&file)).unwrap();
        let text = config.render_provenance(&provenance);
        std::fs::remove_file(&path).ok();
        // The temp path is long enough to hit the value column's truncation
        // cap, so this asserts the surviving prefix rather than the full
        // path; the full path form is pinned by the golden test's short
        // paths instead.
        let credential = config
            .provenance_rows(&provenance)
            .into_iter()
            .find(|row| row.key == "accounts[0].credential")
            .expect("one credential row per account");
        assert!(
            credential.value.starts_with("file:"),
            "the row names the credential kind: {text:?}"
        );
        assert!(
            !text.contains(marker),
            "no byte of the credential file reaches the output: {text:?}"
        );
    }
}
