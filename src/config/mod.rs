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
//! `aub config` (`crate::cli`) prints every resolved key with the source that won,
//! using exactly the four labels above: `flag`, `environment`, `file`, `default`.
//!
//! Scope, stated rather than left implicit: the four scalar sections (`state`,
//! `sampling`, `freshness`, `coverage`) plus `backup.review_after`, `drill.max_age`
//! and `adapter_semantics.max_comparison_age` go through the full four-level order
//! and are individually provenance-tracked, since those are the keys
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
use crate::attribution::quality::AttributionQualityFloor;
use crate::domain::time::MonotonicDuration;
use crate::error::Error;

pub use aliases::AliasTable;
pub use duration::parse_duration;

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
    pub fn label(self) -> &'static str {
        match self {
            ConfigSource::Flag => "flag",
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

/// A configured account. `credential_kind`/`credential_detail` are a loose pass-through
/// of the file's `credential` table (`kind`, plus its `ref` or `path`): the typed,
/// validated credential model belongs to `aub-eun.1`, which consumes this section.
#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub provider: String,
    pub credential_kind: String,
    pub credential_detail: String,
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
    pub reconciliation: ReconciliationConfig,
    pub accounts: Vec<AccountConfig>,
    pub ingest: IngestConfig,
    pub transcripts: Vec<TranscriptConfig>,
    pub tracker: Option<TrackerConfig>,
    pub valuation: ValuationConfig,
    pub backup: BackupConfig,
    pub drill: DrillConfig,
    pub adapter_semantics: AdapterSemanticsConfig,
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
    "reconciliation",
    "accounts",
    "transcripts",
    "tracker",
    "valuation",
    "backup",
    "drill",
    "adapter_semantics",
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
const RECONCILIATION_KEYS: &[&str] = &["residual_window", "residual_min_eligible"];
const ACCOUNT_KEYS: &[&str] = &["name", "provider", "credential"];
const CREDENTIAL_PROFILE_KEYS: &[&str] = &["kind", "ref"];
const CREDENTIAL_FILE_KEYS: &[&str] = &["kind", "path"];
const TRANSCRIPT_KEYS: &[&str] = &["name", "root", "pattern", "format", "usage_evidence"];
const TRACKER_KEYS: &[&str] = &["kind", "path"];
const VALUATION_KEYS: &[&str] = &["default_rate_book"];
const BACKUP_KEYS: &[&str] = &["review_after", "destination"];
const DRILL_KEYS: &[&str] = &["max_age", "result"];
const ADAPTER_SEMANTICS_KEYS: &[&str] = &["max_comparison_age"];

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
                    // An unrecognized or absent `kind` is left to aub-eun.1's
                    // credential resolution to reject; this bead only owns the
                    // shape of the two kinds it already knows about.
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
                    AccountConfig {
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
                            .and_then(|c| c.get("ref").or_else(|| c.get("path")))
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }
                })
                .collect()
        })
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
            reconciliation,
            accounts,
            transcripts,
            tracker,
            valuation,
            backup,
            drill,
            adapter_semantics,
            projects,
            repositories,
        },
        provenance,
    ))
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
}
