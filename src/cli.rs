//! Argument parsing and orchestration.
//!
//! May not depend on:
//! - provider adapters directly
//!
//! Rendering is delegated to `presentation`: a command builds its typed report
//! model and hands it to the presentation entry point, never formatting a
//! quantity itself.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use crate::domain::time::{
    Clock, ClockSkewEnvelope, MonotonicDuration, RealClock, UtcDate, UtcTimestamp,
};
use crate::error::Error;
use crate::logging::{DiagnosticEvent, DiagnosticLogger, Level, LogicalName, Quantity, RunId};
pub use crate::presentation::ExplainMode;
use crate::presentation::json::{
    coverage_json, now_json_with_explain, spend_json_with_explain, status_json_with_explain,
};
use crate::presentation::render::{
    render_coverage_report, render_coverage_threshold_message, render_now_report_with_explain,
    render_spend_report_with_explain, render_status_report_with_explain,
};
use crate::report::ReportEnvelope;
use crate::report::coverage::{CoverageFloors, CoverageSelector, assemble as assemble_coverage};
use crate::report::export::assemble as assemble_export;
use crate::report::spend::{CreditReporting, SpendWindow, assemble_canonical as assemble_spend};
use crate::report::{
    LedgerGeneration, MeterAccount, NowReport, ReportMetadata, SpendGrouping, StatusReport,
};
use crate::store::export::ExportKey;

/// Declares [`Command`] and the derived list of its variants from one token list, so
/// the list cannot drift from the enum: a variant joins both at once. [`Command::ALL`]
/// stays a separate, hand-written literal, and the test module pins it against the
/// derived list, so a variant that joins the enum without joining `ALL` fails a test
/// that names it.
macro_rules! aub_command_enum {
    ($($variant:ident),+ $(,)?) => {
        /// Every command the CLI exposes.
        ///
        /// The exhaustive match in [`Command::flag_policy`] is what makes the shared
        /// flag policy a compile-time obligation: adding a command means adding a
        /// variant, and the match refuses to compile until that variant declares
        /// its policy.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Command {
            $($variant),+
        }

        impl Command {
            /// Every variant, derived from the enum's own declaration by
            /// [`macro_rules@aub_command_enum`]: declaration order is list order, so
            /// a variant is never added in only one place. [`Command::ALL`] is a
            /// separate literal the tests pin against this list.
            pub const DECLARED_VARIANTS: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

aub_command_enum! {
    Status,
    Spend,
    Config,
    Export,
    LoggingFixture,
    StateCheck,
    ExitClass,
    AttemptCrashHook,
    ProjectionCrashHook,
    CostModelFixture,
    RateCard,
    Backup,
    Ingest,
    Rebuild,
    Doctor,
    Coverage,
    Import,
    Sample,
    Now,
    ClearDiagnostics,
    Drill,
    Task,
    Compare,
}

/// Whether a command accepts a shared flag, and the reason it does not when it
/// rejects it. The reason is what help prints, so a rejection is never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagSupport {
    Accepted,
    Rejected { reason: &'static str },
}

/// A command's shared-flag policy: which global flags it accepts and which it
/// explicitly rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagPolicy {
    pub format: FlagSupport,
    pub explain: FlagSupport,
    pub account: FlagSupport,
    pub model: FlagSupport,
    pub no_color: FlagSupport,
    pub verbosity: FlagSupport,
}

impl Command {
    /// Every command, in one place: the enumeration test drives from this rather
    /// than keeping its own private list. `all_lists_every_declared_variant` pins
    /// this array against [`Command::DECLARED_VARIANTS`], which the enum's own
    /// declaration derives, so a variant that joins the enum without joining this
    /// array fails a test that names it.
    pub const ALL: [Self; 23] = [
        Self::Status,
        Self::Spend,
        Self::Config,
        Self::Export,
        Self::LoggingFixture,
        Self::StateCheck,
        Self::ExitClass,
        Self::AttemptCrashHook,
        Self::ProjectionCrashHook,
        Self::CostModelFixture,
        Self::RateCard,
        Self::Backup,
        Self::Ingest,
        Self::Rebuild,
        Self::Doctor,
        Self::Coverage,
        Self::Import,
        Self::Sample,
        Self::Now,
        Self::ClearDiagnostics,
        Self::Drill,
        Self::Task,
        Self::Compare,
    ];

    /// The shared-flag policy for this command: which global flags it accepts
    /// and which it explicitly rejects.
    ///
    /// The match is exhaustive with no wildcard arm, so adding a variant to
    /// [`Command`] breaks compilation here until that variant declares its
    /// policy.
    pub fn flag_policy(self) -> FlagPolicy {
        match self {
            Command::Status => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Accepted,
                account: FlagSupport::Accepted,
                model: FlagSupport::Accepted,
                no_color: FlagSupport::Rejected {
                    reason: "status prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Spend => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Accepted,
                account: FlagSupport::Rejected {
                    reason: "spend groups by account with --group-by account; it has no single-account filter",
                },
                model: FlagSupport::Rejected {
                    reason: "spend has no model dimension until a model selector is needed",
                },
                no_color: FlagSupport::Rejected {
                    reason: "spend prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Config => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "config prints provenance, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "config derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "config prints every account at once",
                },
                model: FlagSupport::Rejected {
                    reason: "config prints every key at once",
                },
                no_color: FlagSupport::Rejected {
                    reason: "config prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Export => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "export is always versioned JSONL",
                },
                explain: FlagSupport::Rejected {
                    reason: "export derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "export reads every stored session, not one account",
                },
                model: FlagSupport::Rejected {
                    reason: "export keys on session or run, not on a model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "export prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::LoggingFixture => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "logging-fixture emits diagnostic fixtures, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "logging-fixture derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "logging-fixture takes no account",
                },
                model: FlagSupport::Rejected {
                    reason: "logging-fixture takes no model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "logging-fixture prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::StateCheck => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "state-check prints a readiness line, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "state-check derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "state-check takes no account",
                },
                model: FlagSupport::Rejected {
                    reason: "state-check takes no model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "state-check prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::ExitClass => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "exit-class returns an exit code, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "exit-class derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "exit-class takes no account",
                },
                model: FlagSupport::Rejected {
                    reason: "exit-class takes no model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "exit-class prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::CostModelFixture => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "cost-model-fixture drives the store, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "cost-model-fixture derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "a cost model is scoped to a provider, not to an account",
                },
                model: FlagSupport::Rejected {
                    reason: "cost-model-fixture names its own model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "cost-model-fixture prints a plain activation line",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::AttemptCrashHook => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "attempt-crash-hook drives the store, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "attempt-crash-hook derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "attempt-crash-hook names its own fixture account",
                },
                model: FlagSupport::Rejected {
                    reason: "attempt-crash-hook drives the store, not a model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "attempt-crash-hook prints plain counts",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::ProjectionCrashHook => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "projection-crash-hook drives the store, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "projection-crash-hook derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "projection-crash-hook names its own fixture account",
                },
                model: FlagSupport::Rejected {
                    reason: "projection-crash-hook drives the store, not a model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "projection-crash-hook prints plain counts",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::RateCard => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "rate-card prints a price book, not a report",
                },
                explain: FlagSupport::Rejected {
                    reason: "rate-card derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "the rate book is reference data, not per-account state",
                },
                model: FlagSupport::Rejected {
                    reason: "the rate book is reference data, not per-model state",
                },
                no_color: FlagSupport::Rejected {
                    reason: "rate-card prints plain rows",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Backup => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "backup prints one operational result",
                },
                explain: FlagSupport::Rejected {
                    reason: "backup derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "backup covers the whole ledger",
                },
                model: FlagSupport::Rejected {
                    reason: "backup covers the whole ledger",
                },
                no_color: FlagSupport::Rejected {
                    reason: "backup prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Ingest => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "ingest prints one operational result",
                },
                explain: FlagSupport::Rejected {
                    reason: "ingest derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "ingest reads the configured transcript sources",
                },
                model: FlagSupport::Rejected {
                    reason: "ingest reads transcript sources, not models",
                },
                no_color: FlagSupport::Rejected {
                    reason: "ingest prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Rebuild => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "rebuild prints one operational result",
                },
                explain: FlagSupport::Rejected {
                    reason: "rebuild derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "rebuild sweeps whole materialization tables, not one account",
                },
                model: FlagSupport::Rejected {
                    reason: "rebuild sweeps whole materialization tables, not models",
                },
                no_color: FlagSupport::Rejected {
                    reason: "rebuild prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Doctor => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Rejected {
                    reason: "doctor derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "doctor is a system-wide diagnostic",
                },
                model: FlagSupport::Rejected {
                    reason: "doctor is a system-wide diagnostic",
                },
                no_color: FlagSupport::Rejected {
                    reason: "doctor prints plain text or json",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Coverage => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Accepted,
                account: FlagSupport::Accepted,
                no_color: FlagSupport::Rejected {
                    reason: "coverage prints plain text or json",
                },
                model: FlagSupport::Rejected {
                    reason: "coverage reports every window of the account, not one model",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Import => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "import prints one operational result",
                },
                explain: FlagSupport::Rejected {
                    reason: "import derives no report",
                },
                account: FlagSupport::Rejected {
                    reason: "import carries account identity in its source",
                },
                model: FlagSupport::Rejected {
                    reason: "import carries no model selector",
                },
                no_color: FlagSupport::Rejected {
                    reason: "import prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Sample => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Rejected {
                    reason: "sample derives no quantity",
                },
                account: FlagSupport::Accepted,
                model: FlagSupport::Rejected {
                    reason: "sample operates on configured accounts, not models",
                },
                no_color: FlagSupport::Rejected {
                    reason: "sample prints plain status or json",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Now => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Accepted,
                account: FlagSupport::Accepted,
                model: FlagSupport::Rejected {
                    reason: "now reports every window; only status takes a --model selector",
                },
                no_color: FlagSupport::Rejected {
                    reason: "now prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::ClearDiagnostics => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Rejected {
                    reason: "clear-diagnostics derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "clear-diagnostics scopes by provider, not account",
                },
                model: FlagSupport::Rejected {
                    reason: "clear-diagnostics takes no model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "clear-diagnostics prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Drill => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "drill prints one operational result",
                },
                explain: FlagSupport::Rejected {
                    reason: "drill derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "a drill exercises the whole state directory, not one account",
                },
                model: FlagSupport::Rejected {
                    reason: "a drill exercises the whole state directory, not one model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "drill prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Task => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Accepted,
                account: FlagSupport::Rejected {
                    reason: "task attribution has no account dimension",
                },
                model: FlagSupport::Rejected {
                    reason: "task attribution has no model dimension",
                },
                no_color: FlagSupport::Rejected {
                    reason: "task prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Compare => FlagPolicy {
                format: FlagSupport::Rejected {
                    reason: "compare prints one operational result",
                },
                explain: FlagSupport::Rejected {
                    reason: "compare derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "a comparison is scoped by observation and window, not by account",
                },
                model: FlagSupport::Rejected {
                    reason: "a comparison is scoped by window, not by model",
                },
                no_color: FlagSupport::Rejected {
                    reason: "compare prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
        }
    }

    /// The token that selects this command on the command line. Test hooks carry a
    /// double underscore so they cannot be typed by accident.
    pub fn name(self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::Spend => "spend",
            Command::Config => "config",
            Command::Export => "export",
            Command::LoggingFixture => "__logging-fixture",
            Command::StateCheck => "__state-check",
            Command::ExitClass => "__exit-class",
            Command::AttemptCrashHook => "__attempt-crash-hook",
            Command::ProjectionCrashHook => "__projection-crash-hook",
            Command::CostModelFixture => "__cost-model-fixture",
            Command::RateCard => "rate-card",
            Command::Backup => "backup",
            Command::Ingest => "ingest",
            Command::Rebuild => "rebuild",
            Command::Doctor => "doctor",
            Command::Coverage => "coverage",
            Command::Import => "import",
            Command::Sample => "sample",
            Command::Now => "now",
            Command::ClearDiagnostics => "clear-diagnostics",
            Command::Drill => "drill",
            Command::Task => "task",
            Command::Compare => "compare",
        }
    }

    /// The one-line description `--help` prints. A test hook prints none: it is not
    /// part of the shipping surface and help does not list it.
    pub fn summary(self) -> Option<&'static str> {
        match self {
            Command::Status => Some("render the last known meter reading per configured account"),
            Command::Spend => Some(
                "canonical token usage grouped by day, session, project, repository, task or account",
            ),
            Command::Config => {
                Some("print every resolved configuration key with the source that won")
            }
            Command::Export => Some(
                "emit versioned JSONL usage rows keyed by session-id or run-id for external joins",
            ),
            Command::LoggingFixture | Command::StateCheck | Command::ExitClass => None,
            Command::AttemptCrashHook => None,
            Command::ProjectionCrashHook => None,
            Command::CostModelFixture => None,
            Command::RateCard => {
                Some("import, show and history the immutable dated vendor rate cards")
            }
            Command::Backup => Some("create or verify a consistent archive of durable state"),
            Command::Ingest => Some(
                "land the configured transcripts' normalized usage into the ledger, advancing the ingestion generation",
            ),
            Command::Rebuild => Some(
                "destroy and recreate one rebuildable materialization group, never touching irreplaceable evidence",
            ),
            Command::Doctor => Some("health, drift and integrity diagnostics"),
            Command::Coverage => Some(
                "did the sampler attempt what the policy owed, and did those attempts observe?",
            ),
            Command::Import => {
                Some("import explicitly selected legacy evidence with durable provenance")
            }
            Command::Sample => Some(
                "observe provider endpoints for due or selected accounts, recording session markers and evidence",
            ),
            Command::Now => Some(
                "force a persisted sampling attempt for the selected accounts and render the resulting state",
            ),
            Command::ClearDiagnostics => Some("clear retained diagnostic provider bodies"),
            Command::Drill => Some(
                "damage a scratch state directory and prove the documented recovery procedure, or run it against a real archive",
            ),
            Command::Task => Some(
                "ingest issue-tracker task-claim events and report per-task usage and overhead",
            ),
            Command::Compare => Some(
                "record and inspect adapter-semantics comparisons against the provider's authoritative surface",
            ),
        }
    }

    /// The question the command answers, for the per-command help block. A test
    /// hook answers none and is not listed.
    pub fn question(self) -> Option<&'static str> {
        match self {
            Command::Status => Some("how much quota does each configured account have left?"),
            Command::Spend => {
                Some("how many canonical tokens were used, grouped by the requested dimensions?")
            }
            Command::Config => Some("which configuration key resolved from where?"),
            Command::RateCard => Some("what do the immutable dated vendor rate cards contain?"),
            Command::Backup => Some(
                "is there a consistent, verified archive of the durable state, and does it restore?",
            ),
            Command::Ingest => Some(
                "have the transcript-derived tables been refreshed from the transcripts on disk, under one generation?",
            ),
            Command::Rebuild => Some(
                "can the transcript-derived materializations be rebuilt from scratch while every irreplaceable record is left untouched?",
            ),
            Command::Doctor => Some(
                "is the recorded evidence healthy, and does the transcript corpus still match its parsers?",
            ),
            Command::Coverage => Some(
                "did the sampler attempt what the policy owed, and did those attempts observe?",
            ),
            Command::Import => Some("which legacy evidence is safe to import into the ledger?"),
            Command::Export => Some(
                "which usage did each session or run consume, as a versioned JSONL ledger for an external join?",
            ),
            Command::Sample => Some(
                "are configured accounts due for meter sampling, and what did the endpoints observe?",
            ),
            Command::Now => Some("how much quota does each configured account have right now?"),
            Command::ClearDiagnostics => Some("how many retained diagnostic bodies were cleared?"),
            Command::Drill => Some(
                "does the documented recovery procedure actually recover a damaged state directory, and is that still true today?",
            ),
            Command::Task => Some(
                "which task or named overhead bucket consumed this usage, by temporal segmentation of the issue tracker's claim history?",
            ),
            Command::Compare => Some(
                "does the adapter's stored reading of one window agree with what the provider's own authoritative surface showed for it?",
            ),
            Command::LoggingFixture | Command::StateCheck | Command::ExitClass => None,
            Command::AttemptCrashHook => None,
            Command::ProjectionCrashHook => None,
            Command::CostModelFixture => None,
        }
    }

    /// The `--format` line of the per-command help block: the accepted values, or
    /// the policy's reason when the command rejects the flag.
    pub fn format_help(self) -> String {
        match self.flag_policy().format {
            FlagSupport::Accepted => "text | json".to_string(),
            FlagSupport::Rejected { reason } => format!("text only: {reason}"),
        }
    }

    /// The refused shared flags of the per-command help block, each with the
    /// policy's reason. Derived from the same policy the parser enforces, so the
    /// help cannot drift from the rejection behaviour.
    pub fn refused_flags(self) -> Vec<String> {
        let policy = self.flag_policy();
        let mut refused = Vec::new();
        for (flag, support) in [
            ("--format", policy.format),
            ("--explain", policy.explain),
            ("--account", policy.account),
            ("--model", policy.model),
            ("--no-color", policy.no_color),
        ] {
            if let FlagSupport::Rejected { reason } = support {
                refused.push(format!("{flag} ({reason})"));
            }
        }
        refused
    }

    /// The command's own options line for the per-command help block, where it
    /// has one.
    pub fn options_help(self) -> Option<&'static str> {
        match self {
            Command::Spend => Some(
                "--today (default) | --since YYYY-MM-DD | --days N | --group-by day|session|project|repository|task|account (repeatable) | --credits | --refresh auto|never|force | --value api-list",
            ),
            Command::Config => Some("--set key=value (repeatable), --config-file PATH"),
            Command::Backup => {
                Some("DESTINATION | verify DESTINATION | restore ARCHIVE DEST [--surviving DIR]")
            }
            Command::Ingest => Some("transcripts [--source NAME] [--changed-only]"),
            Command::Rebuild => Some("transcripts | attribution"),
            Command::Export => Some("--key session-id|run-id (required), --include-logical-ids"),
            Command::Doctor => Some("--fix | --transcript-format-drift"),
            Command::Coverage => {
                Some("--since DURATION (default 24h), --severe; --account is shared")
            }
            Command::Import => Some(
                "legacy-meter --source PATH --backup VERIFIED_ARCHIVE | seed-archive --source PATH --backup VERIFIED_ARCHIVE",
            ),
            Command::Sample => Some(
                "--due | --account NAME | --if-due | --session-id SESSION | --run-id RUN | --require-success",
            ),
            Command::ClearDiagnostics => Some("[--provider NAME | --all]"),
            Command::Drill => Some(
                "--seed truncated-database|corrupted-projection|malformed-spool-record|unsupported-schema-version SCRATCH_DEST | --archive ARCHIVE SCRATCH_DEST",
            ),
            Command::Task => Some(
                "ingest | report TASK-ID | overhead [--today (default) | --since YYYY-MM-DD | --days N]",
            ),
            Command::Compare => Some(
                "record OBSERVATION_ID WINDOW --surface NAME --surface-percent N [--granularity-percent N] [--read-at RFC3339] [--detail TEXT] | uncompared OBSERVATION_ID",
            ),
            Command::Status
            | Command::LoggingFixture
            | Command::StateCheck
            | Command::ExitClass
            | Command::AttemptCrashHook
            | Command::ProjectionCrashHook
            | Command::CostModelFixture
            | Command::RateCard
            | Command::Now => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        if name == "clear-captures" {
            return Some(Self::ClearDiagnostics);
        }
        Self::ALL.into_iter().find(|command| command.name() == name)
    }
}

/// The output format a command was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

/// What the command line asked for, before anything runs: the command, the shared
/// flags the command's policy accepted, and the command's own positional arguments
/// and options left for it to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    pub format: OutputFormat,
    pub verbosity: u8,
    pub explain: ExplainMode,
    /// The account the command line asked for, when the command's policy accepts
    /// `--account`. Status accepts it and selects one configured account.
    pub account: Option<String>,
    /// The model the command line asked for, when the command's policy accepts
    /// `--model`. Status accepts it and scopes the rendered windows to it.
    pub model: Option<String>,
    /// Whether `--no-color` was asked for, when the command's policy accepts it.
    /// No command accepts it yet, so this is always `false` in practice.
    pub no_color: bool,
    pub rest: Vec<String>,
}

/// What a command line resolves to once the argument surface has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// No command: print the version line.
    Version,
    /// `--help` or `-h`: print the command list.
    Help,
    /// A command to run.
    Run(Invocation),
}

/// Reads the argument surface without running anything, so the parser can be tested
/// against the flag policy directly. Leading `-v` flags raise verbosity; `--format`
/// is honoured exactly where the command's policy accepts it and refused with the
/// policy's own reason elsewhere; everything else is left to the command.
pub fn parse_invocation<I: IntoIterator<Item = OsString>>(args: I) -> Result<Request, Error> {
    let mut args = args.into_iter();
    let _program = args.next();
    let mut verbosity: u8 = 0;
    let mut first = args.next();
    while matches!(first.as_ref().and_then(|arg| arg.to_str()), Some("-v")) {
        verbosity += 1;
        first = args.next();
    }
    let Some(first) = first else {
        return Ok(Request::Version);
    };
    let name = first
        .to_str()
        .ok_or_else(|| Error::Usage("argument is not valid UTF-8".into()))?;
    match name {
        "--help" | "-h" => return Ok(Request::Help),
        "--version" | "-V" => return Ok(Request::Version),
        _ => {}
    }
    let command = Command::from_name(name).ok_or_else(|| {
        Error::Usage(format!(
            "unknown argument: {name}; run aub --help to list commands"
        ))
    })?;

    let mut format = OutputFormat::Text;
    let mut explain = ExplainMode::Off;
    let mut account = None;
    let mut model = None;
    let mut no_color = false;
    let mut rest = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let arg = arg
            .to_str()
            .ok_or_else(|| {
                Error::Usage("argument is not valid UTF-8; pass only UTF-8 arguments".into())
            })?
            .to_string();
        let format_value = if let Some(value) = arg.strip_prefix("--format=") {
            Some(value.to_string())
        } else if arg == "--format" {
            Some(next_arg(&mut args, "--format")?)
        } else {
            None
        };
        let account_value = if let Some(value) = arg.strip_prefix("--account=") {
            Some(value.to_string())
        } else if arg == "--account" {
            Some(next_arg(&mut args, "--account")?)
        } else {
            None
        };
        let model_value = if let Some(value) = arg.strip_prefix("--model=") {
            Some(value.to_string())
        } else if arg == "--model" {
            Some(next_arg(&mut args, "--model")?)
        } else {
            None
        };
        match format_value {
            Some(value) => format = parse_format(command, &value)?,
            None if arg == "-v" => verbosity += 1,
            None if arg == "--explain" => explain = parse_explain(command, None)?,
            None if let Some(value) = arg.strip_prefix("--explain=") => {
                explain = parse_explain(command, Some(value))?
            }
            None if arg == "--no-color" => no_color = parse_no_color(command)?,
            None => match (account_value, model_value) {
                (Some(value), _) => account = Some(parse_account(command, &value)?),
                (None, Some(value)) => model = Some(parse_model(command, &value)?),
                (None, None) => rest.push(arg),
            },
        }
    }
    Ok(Request::Run(Invocation {
        command,
        format,
        verbosity,
        explain,
        account,
        model,
        no_color,
        rest,
    }))
}

fn parse_explain(command: Command, value: Option<&str>) -> Result<ExplainMode, Error> {
    match command.flag_policy().explain {
        FlagSupport::Rejected { reason } => Err(Error::Usage(format!(
            "{} does not accept --explain: {reason}; omit the flag",
            command.name()
        ))),
        FlagSupport::Accepted => match value {
            None | Some("") | Some("summary") => Ok(ExplainMode::Summary),
            Some("full") => Ok(ExplainMode::Full),
            Some(other) => Err(Error::Usage(format!(
                "--explain={other} is not one of summary or full; use --explain or --explain=full"
            ))),
        },
    }
}

fn parse_format(command: Command, value: &str) -> Result<OutputFormat, Error> {
    match command.flag_policy().format {
        FlagSupport::Rejected { reason } => Err(Error::Usage(format!(
            "{} does not accept --format: {reason}; omit the flag",
            command.name()
        ))),
        FlagSupport::Accepted => match value {
            "json" => Ok(OutputFormat::Json),
            "text" => Ok(OutputFormat::Text),
            other => Err(Error::Usage(format!(
                "--format must be text or json, got {other}; use --format text or --format json"
            ))),
        },
    }
}

fn parse_account(command: Command, value: &str) -> Result<String, Error> {
    match command.flag_policy().account {
        FlagSupport::Rejected { reason } => Err(Error::Usage(format!(
            "{} does not accept --account: {reason}; omit the flag",
            command.name()
        ))),
        FlagSupport::Accepted => Ok(value.to_string()),
    }
}

fn parse_model(command: Command, value: &str) -> Result<String, Error> {
    match command.flag_policy().model {
        FlagSupport::Rejected { reason } => Err(Error::Usage(format!(
            "{} does not accept --model: {reason}; omit the flag",
            command.name()
        ))),
        FlagSupport::Accepted => Ok(value.to_string()),
    }
}

fn parse_no_color(command: Command) -> Result<bool, Error> {
    match command.flag_policy().no_color {
        FlagSupport::Rejected { reason } => Err(Error::Usage(format!(
            "{} does not accept --no-color: {reason}; omit the flag",
            command.name()
        ))),
        FlagSupport::Accepted => Ok(true),
    }
}

/// The `--help` text: one line per shipping command, then a per-command block
/// stating the question the command answers, the shared flags it refuses and
/// why, and the formats it accepts. The refusal lines are derived from the same
/// flag policy the parser enforces, so help cannot drift from behaviour.
pub fn help_text() -> String {
    let mut lines = vec![
        "aub: one ledger for LLM consumption".to_string(),
        String::new(),
        "usage: aub [-v...] <command> [--format text|json] [options]".to_string(),
        "shared flags: -v, --format text|json, --explain[=summary|full], --account NAME, --model MODEL, --no-color"
            .to_string(),
        String::new(),
        "commands:".to_string(),
    ];
    for command in Command::ALL {
        if let Some(summary) = command.summary() {
            lines.push(format!("  {:<8} {summary}", command.name()));
        }
    }
    for command in Command::ALL {
        let Some(question) = command.question() else {
            continue;
        };
        lines.push(String::new());
        lines.push(format!("{}:", command.name()));
        lines.push(format!("  answers: {question}"));
        let refused = command.refused_flags();
        if !refused.is_empty() {
            lines.push(format!("  refuses: {}", refused.join("; ")));
        }
        lines.push(format!("  format: {}", command.format_help()));
        if let Some(options) = command.options_help() {
            lines.push(format!("  options: {options}"));
        }
    }
    lines.push(String::new());
    lines.push("aub            prints the version; aub --help prints this".to_string());
    lines.join("\n")
}

/// Parse the command surface and route it to bounded workflows.
pub fn run<I: IntoIterator<Item = OsString>>(args: I) -> Result<(), Error> {
    let invocation = match parse_invocation(args)? {
        Request::Version => {
            println!(
                "aub {} ({})",
                crate::build_info::crate_version(),
                crate::build_info::source_revision(),
            );
            return Ok(());
        }
        Request::Help => {
            println!("{}", help_text());
            return Ok(());
        }
        Request::Run(invocation) => invocation,
    };
    let level = std::env::var("AUB_LOG_LEVEL")
        .ok()
        .and_then(|value| Level::parse(&value))
        .unwrap_or(Level::DEFAULT)
        .raised_by(invocation.verbosity);
    match invocation.command {
        Command::Status => {
            reject_positionals(&invocation)?;
            status(
                &RealClock::new(),
                level,
                invocation.format,
                invocation.explain,
                invocation.account.as_deref(),
                invocation.model.as_deref(),
            )
        }
        Command::Spend => spend(&RealClock::new(), level, &invocation),
        Command::Config => config_command(invocation.rest.into_iter().map(OsString::from)),
        Command::Export => export_command(&RealClock::new(), level, &invocation),
        Command::LoggingFixture => logging_fixture(&RealClock::new(), level),
        Command::StateCheck => state_check(&RealClock::new(), level),
        Command::ExitClass => {
            let class = invocation.rest.first().and_then(|s| s.parse::<u8>().ok());
            match class {
                Some(n) => crate::error::representative_outcome(n),
                None => Err(Error::Usage("__exit-class requires a class 0..=8".into())),
            }
        }
        Command::AttemptCrashHook => attempt_crash_hook(&RealClock::new(), level, &invocation),
        Command::ProjectionCrashHook => projection_crash_hook(&RealClock::new(), &invocation),
        Command::CostModelFixture => cost_model_fixture(&RealClock::new(), &invocation),
        Command::RateCard => rate_card_command(&RealClock::new(), &invocation),
        Command::Backup => backup_command(&RealClock::new(), &invocation),
        Command::Ingest => ingest_command(&RealClock::new(), level, &invocation),
        Command::Rebuild => rebuild_command(&RealClock::new(), &invocation),
        Command::Doctor => doctor_command(&RealClock::new(), level, &invocation),
        Command::Coverage => coverage_command(&RealClock::new(), level, &invocation),
        Command::Import => import_command(&RealClock::new(), level, &invocation),
        Command::Sample => sample_command(&RealClock::new(), level, &invocation),
        Command::Now => {
            reject_positionals(&invocation)?;
            now_command(&RealClock::new(), level, &invocation)
        }
        Command::ClearDiagnostics => {
            clear_diagnostics_command(&RealClock::new(), level, &invocation)
        }
        Command::Drill => drill_command(&RealClock::new(), &invocation),
        Command::Task => task_command(&RealClock::new(), level, &invocation),
        Command::Compare => compare_command(&RealClock::new(), &invocation),
    }
}

/// `aub sample`: observe provider endpoints for due or selected accounts,
/// recording session markers and evidence.
pub(crate) fn sample_command(
    clock: &impl Clock,
    level: Level,
    invocation: &Invocation,
) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("sample");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let mut due = false;
    let mut if_due = false;
    let mut require_success = false;
    let mut session_id: Option<String> = None;
    let mut run_id: Option<String> = None;

    let mut args = invocation.rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--due" => due = true,
            "--all" => {
                return Err(Error::Usage(
                    "unknown argument: --all; run aub sample alone".into(),
                ));
            }
            "--if-due" => if_due = true,
            "--require-success" => require_success = true,
            "--session-id" => {
                let val = args
                    .next()
                    .ok_or_else(|| Error::Usage("--session-id requires a value".into()))?;
                session_id = Some(val.clone());
            }
            "--run-id" => {
                let val = args
                    .next()
                    .ok_or_else(|| Error::Usage("--run-id requires a value".into()))?;
                run_id = Some(val.clone());
            }
            other if other.starts_with("--session-id=") => {
                let val = other.strip_prefix("--session-id=").unwrap();
                if val.is_empty() {
                    return Err(Error::Usage("--session-id requires a value".into()));
                }
                session_id = Some(val.to_string());
            }
            other if other.starts_with("--run-id=") => {
                let val = other.strip_prefix("--run-id=").unwrap();
                if val.is_empty() {
                    return Err(Error::Usage("--run-id requires a value".into()));
                }
                run_id = Some(val.to_string());
            }
            other => {
                return Err(Error::Usage(format!(
                    "unknown argument: {other}; run aub sample --help for options"
                )));
            }
        }
    }

    if session_id.is_some() && invocation.account.is_none() {
        return Err(Error::Usage("--session-id requires --account NAME".into()));
    }
    if run_id.is_some() && session_id.is_none() {
        return Err(Error::Usage("--run-id requires --session-id".into()));
    }

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    if let Some(name) = &invocation.account
        && !config.accounts.iter().any(|acc| acc.name == *name)
    {
        return Err(Error::Usage(format!(
            "unknown account '{name}': sample --account names a configured account"
        )));
    }

    // State readiness check before any network request or attempt start.
    crate::store::startup::ensure_state_dir_ready(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
    )?;

    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    let busy_policy = sample_busy_policy(&config);
    // Migrations are confined to the store layer (boundary rules 15 and 16),
    // so the ledger is opened through the same shared opener `ingest` and
    // `rate-card import` already use rather than running the migration
    // framework directly from this file.
    let mut conn = crate::store::rate_card::open_ledger(&db_path, busy_policy.busy_timeout, clock)?;
    crate::store::spool::drain_pending(&mut conn, &config.state.dir)?;
    let repo = crate::store::repository::Repository::new(&db_path, busy_policy);

    // Record session marker if requested.
    if let Some(sess_str) = &session_id {
        let account_name = invocation.account.as_ref().unwrap();
        let target_acc = config
            .accounts
            .iter()
            .find(|acc| acc.name == *account_name)
            .unwrap();

        let session_id_parsed = if let Some((src, nat)) = sess_str.split_once(':') {
            crate::domain::ids::SessionId::new(
                crate::domain::ids::SourceNamespace::new(src),
                crate::domain::ids::NativeSessionId::new(nat),
            )
        } else {
            crate::domain::ids::SessionId::new(
                crate::domain::ids::SourceNamespace::new("cli"),
                crate::domain::ids::NativeSessionId::new(sess_str.as_str()),
            )
        };

        let run_id_parsed = run_id.as_deref().map(|r_str| {
            if let Some((src, nat)) = r_str.split_once(':') {
                crate::domain::ids::RunId::new(
                    crate::domain::ids::SourceNamespace::new(src),
                    crate::domain::ids::NativeRunId::new(nat),
                )
            } else {
                crate::domain::ids::RunId::new(
                    crate::domain::ids::SourceNamespace::new("cli"),
                    crate::domain::ids::NativeRunId::new(r_str),
                )
            }
        });

        let account_id = repo.ensure_account(&target_acc.provider, &target_acc.name, timestamp)?;

        let marker = crate::store::session_account_marker::NewSessionAccountMarker {
            session_id: session_id_parsed,
            observed_at: timestamp,
            source_ordering_key: None,
            logical_account: account_name.clone(),
            resolved_account_id: Some(account_id),
            marker_source: crate::store::session_account_marker::MarkerSource::new("hook"),
            run_id: run_id_parsed,
            evidence_designation:
                crate::store::session_account_marker::EvidenceDesignation::ExplicitLauncherOrHook,
        };

        crate::store::session_account_marker::insert_marker(&conn, &marker)?;
    }

    let target_accounts: Vec<&crate::config::AccountConfig> = match &invocation.account {
        Some(name) => config
            .accounts
            .iter()
            .filter(|acc| acc.name == *name)
            .collect(),
        None => config.accounts.iter().collect(),
    };

    if target_accounts.is_empty() {
        if invocation.format == OutputFormat::Json {
            println!(
                "{{\"schema_version\":1,\"command\":\"sample\",\"run_id\":\"{}\",\"accounts\":[]}}",
                run.as_str()
            );
        }
        return Ok(());
    }

    logger
        .emit(
            timestamp,
            DiagnosticEvent::RequestAttempted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let forced = !due && !if_due;

    let trigger = if session_id.is_some() {
        crate::store::sample_run::Trigger::Hook
    } else if due {
        crate::store::sample_run::Trigger::Timer
    } else {
        crate::store::sample_run::Trigger::Manual
    };

    let mut batch_accounts = Vec::new();
    for acc in &target_accounts {
        let resolved = crate::auth::resolve(acc, &crate::auth::RealFs, invocation.verbosity > 0)?;
        let credential_handle =
            crate::meter::adapter::CredentialHandle::new(resolved.material.into_inner().as_str());
        let credential_context_id = Some(resolved.context_id.as_str().to_string());

        let resolved_policy = crate::store::sampling_policy_snapshot::ResolvedSamplingPolicy {
            ordinary_cadence: config.sampling.default_interval,
            freshness_horizon: config.freshness.meter,
            reset_edge_policy: format!(
                "lead-{}s",
                config.sampling.reset_edge_lead.as_nanos() / 1_000_000_000
            ),
            retry_backoff_policy: "none".to_string(),
            command_budget: config.sampling.command_budget,
            policy_algorithm_version: "v1".to_string(),
        };

        let adapter = if acc.provider == "anthropic" {
            let endpoint = std::env::var("AUB_ANTHROPIC_ENDPOINT").unwrap_or_else(|_| {
                crate::meter::anthropic::AnthropicAdapter::DEFAULT_ENDPOINT.to_string()
            });
            crate::meter::anthropic::AnthropicAdapter::with_endpoint(endpoint)
        } else {
            return Err(Error::Usage(format!(
                "unsupported provider '{}' for account '{}' (supported: anthropic)",
                acc.provider, acc.name
            )));
        };

        batch_accounts.push(crate::meter::sampler::BatchAccount {
            name: crate::store::sampling_lease::AccountName::new(&acc.name),
            provider_key: acc.provider.clone(),
            adapter,
            credential: credential_handle,
            credential_context_id,
            request: crate::meter::adapter::MeterRequest::default(),
            policy: resolved_policy,
            reset_edge_lead: config.sampling.reset_edge_lead,
            forced,
            adapter_version: crate::domain::ids::AdapterVersion::new(
                crate::build_info::crate_version(),
            ),
        });
    }

    let orchestrator = crate::meter::sampler::SamplingOrchestrator {
        repository: &repo,
        transport: crate::meter::transport::BlockingTransport,
        clock: crate::domain::time::RealClock::new(),
        trigger,
        configuration_fingerprint: "aub-v1".to_string(),
        holder: crate::store::sampling_lease::LeaseHolder::new(format!(
            "pid-{}",
            std::process::id()
        )),
        lease_ttl: crate::domain::time::MonotonicDuration::from_seconds(60),
        command_budget: config.sampling.command_budget,
        max_concurrent_requests: config.sampling.max_concurrent_requests,
    };

    let run_result = orchestrator
        .run(&batch_accounts)
        .map_err(|error| name_busy_wait(error, busy_policy.busy_timeout));
    // Recorded outside the ledger and unconditionally, so a tick refused by
    // the very database it would have written to still leaves a durable
    // trace `aub doctor` can read (`aub-va6s`). A marker-write failure is a
    // diagnostic-aid failure, not the tick's own outcome, so it is not
    // allowed to mask or replace the result the caller actually asked for.
    let _ = crate::store::sample_tick::record_last_tick(
        &config.state.dir,
        &crate::store::sample_tick::LastSampleTick {
            started_at: timestamp,
            outcome: match &run_result {
                Ok(_) => crate::store::sample_tick::TickOutcome::Success,
                Err(error) => crate::store::sample_tick::TickOutcome::Failed(error.to_string()),
            },
        },
    );
    let batch_report = run_result?;

    match invocation.format {
        OutputFormat::Text => {
            for report in &batch_report.accounts {
                match &report.disposition {
                    crate::meter::sampler::AccountDisposition::Sampled(sampled) => {
                        let outcome_str = match sampled.outcome {
                            crate::domain::attempt::AttemptOutcome::Success => "success",
                            crate::domain::attempt::AttemptOutcome::AuthRequired => "auth_required",
                            crate::domain::attempt::AttemptOutcome::Unreachable(_) => "unreachable",
                        };
                        println!(
                            "sample: account={} outcome={} attempt={}",
                            report.name.as_str(),
                            outcome_str,
                            sampled.attempt_id.value(),
                        );
                    }
                    crate::meter::sampler::AccountDisposition::NotYet { next_due_at } => {
                        println!(
                            "sample: account={} not-due next_due_at={}",
                            report.name.as_str(),
                            next_due_at.unix_nanos(),
                        );
                    }
                    crate::meter::sampler::AccountDisposition::LeaseHeld { holder } => {
                        println!(
                            "sample: account={} lease-held holder={}",
                            report.name.as_str(),
                            holder,
                        );
                    }
                    crate::meter::sampler::AccountDisposition::DueLookupFailed { reason } => {
                        println!(
                            "sample: account={} due-lookup-failed reason={}",
                            report.name.as_str(),
                            reason,
                        );
                    }
                    crate::meter::sampler::AccountDisposition::EligibilityFailed { reason } => {
                        println!(
                            "sample: account={} eligibility-failed reason={}",
                            report.name.as_str(),
                            reason,
                        );
                    }
                    crate::meter::sampler::AccountDisposition::PersistFailed {
                        attempt_id,
                        outcome,
                        reason,
                    } => {
                        let outcome_str = match outcome {
                            crate::domain::attempt::AttemptOutcome::Success => "success",
                            crate::domain::attempt::AttemptOutcome::AuthRequired => "auth_required",
                            crate::domain::attempt::AttemptOutcome::Unreachable(_) => "unreachable",
                        };
                        println!(
                            "sample: account={} persist-failed attempt={} outcome={} reason={}",
                            report.name.as_str(),
                            attempt_id.value(),
                            outcome_str,
                            reason,
                        );
                    }
                }
            }
        }
        OutputFormat::Json => {
            let accounts_json: Vec<serde_json::Value> = batch_report
                .accounts
                .iter()
                .map(|report| {
                    let (disp_str, details) = match &report.disposition {
                        crate::meter::sampler::AccountDisposition::Sampled(sampled) => {
                            let outcome_str = match sampled.outcome {
                                crate::domain::attempt::AttemptOutcome::Success => "success",
                                crate::domain::attempt::AttemptOutcome::AuthRequired => {
                                    "auth_required"
                                }
                                crate::domain::attempt::AttemptOutcome::Unreachable(_) => {
                                    "unreachable"
                                }
                            };
                            (
                                "sampled",
                                serde_json::json!({
                                    "attempt_id": sampled.attempt_id.value(),
                                    "outcome": outcome_str,
                                }),
                            )
                        }
                        crate::meter::sampler::AccountDisposition::NotYet { next_due_at } => (
                            "not_yet",
                            serde_json::json!({
                                "next_due_at_nanos": next_due_at.unix_nanos(),
                            }),
                        ),
                        crate::meter::sampler::AccountDisposition::LeaseHeld { holder } => (
                            "lease_held",
                            serde_json::json!({
                                "holder": holder,
                            }),
                        ),
                        crate::meter::sampler::AccountDisposition::DueLookupFailed { reason } => {
                            ("due_lookup_failed", serde_json::json!({ "reason": reason }))
                        }
                        crate::meter::sampler::AccountDisposition::EligibilityFailed { reason } => {
                            (
                                "eligibility_failed",
                                serde_json::json!({ "reason": reason }),
                            )
                        }
                        crate::meter::sampler::AccountDisposition::PersistFailed {
                            attempt_id,
                            reason,
                            ..
                        } => (
                            "persist_failed",
                            serde_json::json!({
                                "attempt_id": attempt_id.value(),
                                "reason": reason,
                            }),
                        ),
                    };
                    serde_json::json!({
                        "account": report.name.as_str(),
                        "disposition": disp_str,
                        "details": details,
                    })
                })
                .collect();
            let root = serde_json::json!({
                "schema_version": 1,
                "command": "sample",
                "run_id": run.as_str(),
                "sample_run_id": batch_report.run_id.value(),
                "accounts": accounts_json,
            });
            println!("{}", serde_json::to_string_pretty(&root).unwrap());
        }
    }

    sampling_disposition_error(&batch_report.accounts)?;

    if require_success {
        for report in &batch_report.accounts {
            if let crate::meter::sampler::AccountDisposition::Sampled(sampled) = &report.disposition
            {
                match &sampled.outcome {
                    crate::domain::attempt::AttemptOutcome::Success => {}
                    crate::domain::attempt::AttemptOutcome::AuthRequired => {
                        return Err(Error::AuthRequired(format!(
                            "account '{}': authentication required",
                            report.name.as_str()
                        )));
                    }
                    crate::domain::attempt::AttemptOutcome::Unreachable(class) => {
                        return Err(Error::RemoteUnavailable(format!(
                            "account '{}': remote source unavailable: {class:?}",
                            report.name.as_str()
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

/// The pragma policy `aub sample` opens its ledger connection with
/// (`aub-va6s`): the same `sampling.request_timeout` every other
/// store-opening command in this file already uses, factored out here so the
/// value is testable on its own rather than only as a side effect of a full
/// sample run. Before this bead the busy timeout was hardcoded to 500ms,
/// which refused a sampler on the first contended attempt instead of waiting
/// through a batched ingest's brief holds of the writer slot, exactly the
/// case a scheduled `aub sample --due` tick collides with on a machine that
/// is also running its first ingest.
fn sample_busy_policy(config: &crate::config::Config) -> crate::store::connection::PragmaPolicy {
    crate::store::connection::PragmaPolicy {
        busy_timeout: config.sampling.busy_timeout,
    }
}

/// Names how long a busy-database refusal waited, so the message
/// distinguishes a sampler that waited its configured timeout from one that
/// refused instantly (`aub-va6s`). `sample_run::start_sample_run` is the
/// batch's one failure point that can carry this SQLite failure text (the
/// only write the orchestrator performs before any account-level work); every
/// other error passes through unchanged. The refusal path itself is kept
/// exactly as it was: only how long the sampler waited before reaching it
/// changes, not whether it can still be reached.
fn name_busy_wait(error: Error, busy_timeout: crate::domain::time::MonotonicDuration) -> Error {
    if let Error::Store(message) = &error
        && message.contains("database is locked")
    {
        return Error::Store(format!(
            "{message} (waited up to {}ms)",
            busy_timeout.as_nanos() / 1_000_000
        ));
    }
    error
}

/// `aub now`: force a persisted sampling attempt for the selected accounts
/// and render the resulting current state.
pub(crate) fn now_command(
    clock: &impl Clock,
    level: Level,
    invocation: &Invocation,
) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("now");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    if let Some(name) = &invocation.account
        && !config.accounts.iter().any(|acc| acc.name == *name)
    {
        return Err(Error::Usage(format!(
            "unknown account '{name}': now --account names a configured account"
        )));
    }

    // State readiness check before any network request or attempt start.
    crate::store::startup::ensure_state_dir_ready(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
    )?;

    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    let busy_policy = crate::store::connection::PragmaPolicy {
        busy_timeout: crate::domain::time::MonotonicDuration::from_millis(500),
    };
    let mut conn = crate::store::rate_card::open_ledger(&db_path, busy_policy.busy_timeout, clock)?;
    crate::store::spool::drain_pending(&mut conn, &config.state.dir)?;
    let repo = crate::store::repository::Repository::new(&db_path, busy_policy);

    let target_accounts: Vec<&crate::config::AccountConfig> = match &invocation.account {
        Some(name) => config
            .accounts
            .iter()
            .filter(|acc| acc.name == *name)
            .collect(),
        None => config.accounts.iter().collect(),
    };

    if target_accounts.is_empty() {
        // No account to sample: there is nothing to fetch and nothing to
        // record, so this is not an unrecorded-fetch path. The empty current
        // state is the whole answer.
        let metadata = ReportMetadata::new(timestamp, timestamp, LedgerGeneration::new(0), None);
        let report = NowReport::new(metadata, Vec::new(), Vec::new());
        emit_now_report(&report, run, timestamp, invocation);
        return Ok(());
    }

    logger
        .emit(
            timestamp,
            DiagnosticEvent::RequestAttempted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let mut batch_accounts = Vec::new();
    for acc in &target_accounts {
        let resolved = crate::auth::resolve(acc, &crate::auth::RealFs, invocation.verbosity > 0)?;
        let credential_handle =
            crate::meter::adapter::CredentialHandle::new(resolved.material.into_inner().as_str());
        let credential_context_id = Some(resolved.context_id.as_str().to_string());

        let resolved_policy = crate::store::sampling_policy_snapshot::ResolvedSamplingPolicy {
            ordinary_cadence: config.sampling.default_interval,
            freshness_horizon: config.freshness.meter,
            reset_edge_policy: format!(
                "lead-{}s",
                config.sampling.reset_edge_lead.as_nanos() / 1_000_000_000
            ),
            retry_backoff_policy: "none".to_string(),
            command_budget: config.sampling.command_budget,
            policy_algorithm_version: "v1".to_string(),
        };

        let adapter = if acc.provider == "anthropic" {
            let endpoint = std::env::var("AUB_ANTHROPIC_ENDPOINT").unwrap_or_else(|_| {
                crate::meter::anthropic::AnthropicAdapter::DEFAULT_ENDPOINT.to_string()
            });
            crate::meter::anthropic::AnthropicAdapter::with_endpoint(endpoint)
        } else {
            return Err(Error::Usage(format!(
                "unsupported provider '{}' for account '{}' (supported: anthropic)",
                acc.provider, acc.name
            )));
        };

        batch_accounts.push(crate::meter::sampler::BatchAccount {
            name: crate::store::sampling_lease::AccountName::new(&acc.name),
            provider_key: acc.provider.clone(),
            adapter,
            credential: credential_handle,
            credential_context_id,
            request: crate::meter::adapter::MeterRequest::default(),
            policy: resolved_policy,
            reset_edge_lead: config.sampling.reset_edge_lead,
            forced: true,
            adapter_version: crate::domain::ids::AdapterVersion::new(
                crate::build_info::crate_version(),
            ),
        });
    }

    let orchestrator = crate::meter::sampler::SamplingOrchestrator {
        repository: &repo,
        transport: crate::meter::transport::BlockingTransport,
        clock: crate::domain::time::RealClock::new(),
        trigger: crate::store::sample_run::Trigger::Live,
        configuration_fingerprint: "aub-v1".to_string(),
        holder: crate::store::sampling_lease::LeaseHolder::new(format!(
            "pid-{}",
            std::process::id()
        )),
        lease_ttl: crate::domain::time::MonotonicDuration::from_seconds(60),
        command_budget: config.sampling.command_budget,
        max_concurrent_requests: config.sampling.max_concurrent_requests,
    };

    let batch_report = orchestrator.run(&batch_accounts)?;

    // A disposition that failed to record the attempt or its terminal fact is a
    // persistence failure, reported with the store class. The projection is not
    // read and no reading is rendered: an unrecorded observation is never shown
    // as though it were durable (correctness invariant 5).
    sampling_disposition_error(&batch_report.accounts)?;

    // Rendering reads the projection the batch just published and runs it
    // through the same freshness function and report models `status` uses, so a
    // `now` cannot disagree with a `status` taken a moment later.
    let projection_path = crate::projection::projection_path_in(&config.state.dir);
    let (accounts, ledger_generation) =
        match crate::projection::reader::read_projection(&projection_path) {
            crate::projection::reader::ProjectionRead::Available(projection) => {
                let accounts = projection_accounts(
                    &config,
                    &projection,
                    invocation.account.as_deref(),
                    invocation.model.as_deref(),
                    clock,
                );
                logger
                    .emit(
                        timestamp,
                        DiagnosticEvent::ProjectionRead,
                        &[("state", &LogicalName::new("ok"))],
                    )
                    .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
                let generation = LedgerGeneration::new(projection.ledger_generation.value());
                (accounts, generation)
            }
            crate::projection::reader::ProjectionRead::Unavailable(unavailable) => {
                // The batch reported no persistence failure, so the projection it
                // published should be readable now. If it is not, the state
                // directory is the fault: report it with the store class rather
                // than render an empty answer as if nothing were configured.
                return Err(Error::Store(format!(
                    "sampled accounts but the published projection could not be read: {}",
                    unavailable.reason()
                )));
            }
        };

    let metadata = ReportMetadata::new(timestamp, timestamp, ledger_generation, None);
    let report = NowReport::new(metadata, accounts, Vec::new());
    emit_now_report(&report, run, timestamp, invocation);
    logger
        .emit(
            timestamp,
            DiagnosticEvent::ReportRendered,
            &[("report_kind", &LogicalName::new("now"))],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    Ok(())
}

/// Returns the store-class error for the first disposition that failed to record
/// its attempt or terminal fact. Shared by `sample` and `now`: both treat a
/// persistence failure as fatal and neither renders a reading after one.
fn sampling_disposition_error(
    accounts: &[crate::meter::sampler::AccountReport],
) -> Result<(), Error> {
    for report in accounts {
        match &report.disposition {
            crate::meter::sampler::AccountDisposition::PersistFailed { reason, .. } => {
                return Err(Error::Store(format!(
                    "evidence could not be durably preserved: {reason}"
                )));
            }
            crate::meter::sampler::AccountDisposition::DueLookupFailed { reason } => {
                return Err(Error::Store(format!(
                    "sampling due lookup failed: {reason}"
                )));
            }
            crate::meter::sampler::AccountDisposition::EligibilityFailed { reason } => {
                return Err(Error::Store(format!(
                    "sampling eligibility failed: {reason}"
                )));
            }
            crate::meter::sampler::AccountDisposition::NotYet { .. }
            | crate::meter::sampler::AccountDisposition::LeaseHeld { .. }
            | crate::meter::sampler::AccountDisposition::Sampled(_) => {}
        }
    }
    Ok(())
}

/// Writes the `now` report in the requested format. One freshness variant per
/// account travels in either format because both read the same [`NowReport`].
fn emit_now_report(
    report: &NowReport,
    run: RunId,
    timestamp: crate::domain::time::UtcTimestamp,
    invocation: &Invocation,
) {
    match invocation.format {
        OutputFormat::Text => println!(
            "{}",
            render_now_report_with_explain(
                report,
                timestamp,
                status_clock_skew_envelope(),
                invocation.explain,
            )
        ),
        OutputFormat::Json => {
            println!("{}", now_json_with_explain(report, run, invocation.explain))
        }
    }
}

/// `aub doctor`: the check registry (`aub-n27.7`) by default, the deeper
/// `--transcript-format-drift` view of one check's own evidence, or `--fix` for
/// the four permitted repairs.
fn doctor_command(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("doctor");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let mut legacy_drift_view = false;
    let mut fix = false;
    for arg in &invocation.rest {
        match arg.as_str() {
            "--transcript-format-drift" | "--rate-card-staleness" => legacy_drift_view = true,
            "--fix" => fix = true,
            other => return Err(Error::Usage(format!("unknown argument: {other}"))),
        }
    }
    if legacy_drift_view && fix {
        return Err(Error::Usage(
            "--fix cannot be combined with --transcript-format-drift or --rate-card-staleness"
                .to_string(),
        ));
    }

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let config_result = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    );

    if legacy_drift_view {
        let (config, _provenance) = config_result?;
        return doctor_transcript_drift_view(&config, timestamp, run, invocation);
    }
    if fix {
        let (config, _provenance) = config_result?;
        let mut conn = open_ledger(clock)?;
        let report = crate::doctor::run_fix(&mut conn, &config, clock)?;
        match invocation.format {
            OutputFormat::Text => println!("{}", crate::presentation::render_fix_report(&report)),
            OutputFormat::Json => println!(
                "{}",
                crate::presentation::fix_report_json(&report, run, timestamp)
            ),
        }
        return Ok(());
    }

    let (outcomes, residual) = match &config_result {
        Ok((config, _provenance)) => doctor_registry_and_residual(config, timestamp),
        Err(error) => (
            crate::doctor::configuration_failed_registry(&error.to_string()),
            None,
        ),
    };
    let ledger_generation = match &config_result {
        Ok((config, _provenance)) => current_ledger_generation_or_zero(config),
        Err(_) => LedgerGeneration::new(0),
    };
    let report = crate::doctor::DoctorReport {
        metadata: ReportMetadata::new(timestamp, timestamp, ledger_generation, None),
        outcomes,
        residual,
    };
    match invocation.format {
        OutputFormat::Text => println!("{}", crate::presentation::render_doctor_report(&report)),
        OutputFormat::Json => println!("{}", crate::presentation::doctor_report_json(&report, run)),
    }
    Ok(())
}

/// The `aub doctor --transcript-format-drift` / `--rate-card-staleness` view: the
/// pre-registry report, kept verbatim so its own e2e case and unit tests keep
/// passing unchanged.
fn doctor_transcript_drift_view(
    config: &crate::config::Config,
    timestamp: UtcTimestamp,
    run: RunId,
    invocation: &Invocation,
) -> Result<(), Error> {
    let mut db_quarantine = None;
    let mut stale_cards = Vec::new();
    let mut attribution_assessment = None;
    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    if db_path.is_file() {
        let policy = crate::store::connection::PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(500),
        };
        if let Ok(conn) = crate::store::connection::open(
            &db_path,
            crate::store::connection::AccessMode::ReadOnly,
            &policy,
        ) {
            if let Ok(summary) = crate::store::ingest_quarantine::quarantine_summary(&conn) {
                db_quarantine = Some(summary);
            }
            if let Ok(stale) = crate::store::rate_card::stale_rate_cards(&conn, timestamp) {
                stale_cards = stale;
            }
            if let Ok(observations) =
                crate::store::account_attribution_segment::attribution_observations(&conn)
            {
                let window_nanos =
                    i64::try_from(config.attribution.recent_window.as_nanos()).unwrap_or(i64::MAX);
                let window_since = crate::domain::time::UtcTimestamp::from_unix_nanos(
                    timestamp.unix_nanos().saturating_sub(window_nanos),
                );
                attribution_assessment = Some(
                    crate::attribution::quality::AttributionQualityAssessment::assess(
                        observations,
                        window_since,
                        config.attribution.quality_floor,
                    ),
                );
            }
        }
    }

    let report =
        crate::transcripts::detect_drift(config, None, timestamp, db_quarantine.as_deref())?;

    match invocation.format {
        OutputFormat::Text => {
            println!(
                "{}",
                crate::presentation::render_doctor_drift_report(&report)
            );
            if !stale_cards.is_empty() {
                println!("\nStale rate cards (review due):");
                for card in &stale_cards {
                    let due_str = match &card.draft.review_due {
                        crate::domain::rate_card::ReviewDuePolicy::On(d) => d.iso(),
                        crate::domain::rate_card::ReviewDuePolicy::None => String::new(),
                    };
                    println!(
                        "  stale rate card: {} {} {} review due on {}",
                        card.draft.vendor,
                        card.draft.model,
                        card.draft.token_class.as_str(),
                        due_str
                    );
                }
            }
        }
        OutputFormat::Json => println!("{}", crate::presentation::doctor_drift_json(&report, run)),
    }

    if let Some(assessment) = &attribution_assessment {
        if let OutputFormat::Text = invocation.format {
            println!(
                "\n{}",
                crate::presentation::render_attribution_quality(assessment)
            );
        }
        if let Some(error) = attribution_quality_breach_error(assessment) {
            return Err(error);
        }
    }

    Ok(())
}

/// The doctor exit consequence of an attribution-quality assessment: a
/// `ThresholdNotMet` error naming every token kind and scope that fell below
/// the configured floor, or `None` when nothing did. Split out so the mapping
/// is unit-tested without a live command.
fn attribution_quality_breach_error(
    assessment: &crate::attribution::quality::AttributionQualityAssessment,
) -> Option<Error> {
    if !assessment.has_breach() {
        return None;
    }
    let kinds: Vec<String> = assessment
        .breaches
        .iter()
        .map(|breach| {
            let scope = match breach.scope {
                crate::attribution::quality::MetricScope::AllHistory => "all history",
                crate::attribution::quality::MetricScope::RecentWindow { .. } => "recent window",
            };
            format!("{} ({scope})", breach.kind.label())
        })
        .collect();
    Some(Error::ThresholdNotMet(format!(
        "attribution quality is below the configured floor for: {}",
        kinds.join(", ")
    )))
}

/// Runs the doctor check registry against a configured ledger context, opening the
/// ledger read-only when it exists, tolerating both its absence (a fresh install)
/// and a failure to open it (a finding, not an absence).
fn doctor_registry_and_residual(
    config: &crate::config::Config,
    timestamp: UtcTimestamp,
) -> (
    Vec<crate::doctor::CheckOutcome>,
    Option<crate::reconciliation::RollingResidualHealth>,
) {
    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    let policy = crate::store::connection::PragmaPolicy {
        busy_timeout: crate::domain::time::MonotonicDuration::from_millis(500),
    };
    let (db, db_missing, db_open_error) = if !db_path.is_file() {
        (None, true, None)
    } else {
        match crate::store::connection::open(
            &db_path,
            crate::store::connection::AccessMode::ReadOnly,
            &policy,
        ) {
            Ok(conn) => (Some(conn), false, None),
            Err(error) => (None, false, Some(error.to_string())),
        }
    };
    let ctx = crate::doctor::DoctorContext {
        config,
        timestamp,
        db_path,
        db: db.as_ref(),
        db_missing,
        db_open_error,
    };
    let outcomes = crate::doctor::build_registry(&ctx);
    let residual = crate::doctor::checks::rolling_residual_health(&ctx);
    (outcomes, residual)
}

/// The current ledger generation for the doctor report's metadata, or zero when
/// no ledger exists yet: the same "nothing recorded yet" reading
/// `TranscriptDriftReport`'s empty case uses, never a fabricated positive number.
fn current_ledger_generation_or_zero(config: &crate::config::Config) -> LedgerGeneration {
    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    if !db_path.is_file() {
        return LedgerGeneration::new(0);
    }
    let policy = crate::store::connection::PragmaPolicy {
        busy_timeout: crate::domain::time::MonotonicDuration::from_millis(500),
    };
    crate::store::connection::open(
        &db_path,
        crate::store::connection::AccessMode::ReadOnly,
        &policy,
    )
    .ok()
    .and_then(|conn| crate::store::ledger_generation::current(&conn).ok())
    .map(|generation| LedgerGeneration::new(generation.value()))
    .unwrap_or_else(|| LedgerGeneration::new(0))
}

/// The default window `aub coverage` reports when the command line names none:
/// the window the worked example in PLAN.md section 49 is written over.
const DEFAULT_COVERAGE_WINDOW: MonotonicDuration = MonotonicDuration::from_seconds(86_400);

/// The window flags of `aub coverage`: `--since DURATION` (default 24h) and
/// `--severe`. Both compose with the shared `--account`.
struct CoverageWindow {
    since: MonotonicDuration,
    /// The window as the command line asked for it, echoed by the header:
    /// "coverage - last 24h" is the request the interval answers.
    description: String,
    severe_only: bool,
}

fn coverage_window(rest: &[String]) -> Result<CoverageWindow, Error> {
    let mut since = DEFAULT_COVERAGE_WINDOW;
    let mut description = "24h".to_string();
    let mut severe_only = false;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--severe" => severe_only = true,
            "--since" => {
                let value = args.next().ok_or_else(|| {
                    Error::Usage("--since requires a duration like 24h or 30d".into())
                })?;
                since = parse_since(value)?;
                description = value.to_string();
            }
            other => match other.strip_prefix("--since=") {
                Some(value) => {
                    since = parse_since(value)?;
                    description = value.to_string();
                }
                None => return Err(Error::Usage(format!("unknown argument: {other}"))),
            },
        }
    }
    Ok(CoverageWindow {
        since,
        description,
        severe_only,
    })
}

fn parse_since(value: &str) -> Result<MonotonicDuration, Error> {
    crate::config::parse_duration(value).map_err(|_| {
        Error::Usage(format!(
            "--since must be a duration like 24h or 30d, got {value}"
        ))
    })
}

/// `aub coverage`: attempt and measurement coverage per account over one
/// interval, the failure classes behind each account's numbers, and the
/// threshold exit. The ledger is read read-only and the command performs no
/// network operation: the exit class is the whole notification mechanism's
/// signal, and the binary answers without a daemon.
fn coverage_command(
    clock: &impl Clock,
    level: Level,
    invocation: &Invocation,
) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("coverage");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let window = coverage_window(&invocation.rest)?;
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    let since_nanos = timestamp
        .unix_nanos()
        .saturating_sub(window.since.as_nanos() as i64);
    let since = UtcTimestamp::from_unix_nanos(since_nanos);

    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    if !db_path.is_file() {
        return Err(Error::InsufficientEvidence(format!(
            "no ledger exists at {}; nothing is recorded about sampling yet",
            db_path.display()
        )));
    }
    let policy = crate::store::connection::PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let conn = crate::store::connection::open(
        &db_path,
        crate::store::connection::AccessMode::ReadOnly,
        &policy,
    )?;
    let selector = CoverageSelector {
        account: invocation.account.clone(),
        severe_only: window.severe_only,
    };
    let floors = CoverageFloors {
        attempt: config.coverage.attempt_floor,
        measurement: config.coverage.measurement_floor,
    };
    let report = assemble_coverage(&conn, since, timestamp, &selector, floors, timestamp)?;

    match invocation.format {
        OutputFormat::Text => println!("{}", render_coverage_report(&report, &window.description)),
        OutputFormat::Json => println!("{}", coverage_json(&report, run)),
    }
    if report.threshold.met {
        Ok(())
    } else {
        Err(Error::ThresholdNotMet(render_coverage_threshold_message(
            &report,
        )))
    }
}

fn reject_positionals(invocation: &Invocation) -> Result<(), Error> {
    match invocation.rest.first() {
        Some(extra) => Err(Error::Usage(format!(
            "unknown argument: {extra}; run aub --help for command usage"
        ))),
        None => Ok(()),
    }
}

/// `aub spend`: the window flags are read here, everything else is the assembly in
/// `report::spend` and the rendering in `presentation`. An unreadable file makes the
/// report incomplete: it is still printed, with the file named, and the exit class
/// says so.
fn spend(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("spend");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let options = spend_options(&invocation.rest, timestamp)?;
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let mut conn = open_ledger(clock)?;
    let mut refresh_failure = None;
    let mut refresh_report = None;
    if options.refresh != RefreshPolicy::Never {
        let ingest_options = crate::ingest::IngestOptions {
            source: None,
            changed_only: options.refresh == RefreshPolicy::Auto,
        };
        // The spend refresh lands batches without observing them; the batch
        // sink and the progress sink are the ingest command's own diagnostic
        // surface, not the spend command's.
        match crate::ingest::run(
            &mut conn,
            &config,
            &ingest_options,
            clock,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        ) {
            Ok(report) if report.unreadable_files.is_empty() => refresh_report = Some(report),
            Ok(report) => {
                refresh_failure = Some(format!(
                    "refresh could not read {} file(s); retained the prior canonical subtotal",
                    report.unreadable_files.len()
                ));
                refresh_report = Some(report);
            }
            Err(error) => {
                refresh_failure = Some(format!(
                    "refresh failed: {error}; retained the prior canonical subtotal"
                ));
            }
        }
    }
    let rate_book = match options.value {
        Some(SpendValuationMode::ApiList) => {
            let cards = crate::store::rate_card::history(&conn)?;
            Some(crate::valuation::RateBook::new(cards))
        }
        None => None,
    };
    let active_cost_model = if options.credits {
        crate::store::cost_model::load_active_at(&conn, timestamp)?
    } else {
        None
    };
    let credit_reporting = match active_cost_model.as_ref() {
        Some(model) => CreditReporting::Active(model),
        None if options.credits => CreditReporting::NoActiveModel,
        None => CreditReporting::NotRequested,
    };
    let mut report = assemble_spend(
        &conn,
        options.window,
        timestamp,
        options.grouping,
        options.refresh != RefreshPolicy::Never,
        refresh_failure.clone(),
        rate_book.as_ref(),
        credit_reporting,
    )?;
    if let Some(refresh) = refresh_report {
        report.ingest.files_read = refresh.files_parsed;
        report.ingest.files_skipped_before_window = refresh.files_skipped;
        report.ingest.unreadable_files = refresh.unreadable_files;
    }
    match invocation.format {
        OutputFormat::Text => println!(
            "{}",
            render_spend_report_with_explain(&report, invocation.explain)
        ),
        OutputFormat::Json => println!(
            "{}",
            spend_json_with_explain(&report, run, invocation.explain)
        ),
    }
    if let Some(failure) = refresh_failure {
        Err(Error::IngestIncomplete(failure))
    } else if report.ingest.unreadable_files.is_empty() {
        Ok(())
    } else {
        Err(Error::IngestIncomplete(format!(
            "{} file(s) could not be read; the counts above exclude them",
            report.ingest.unreadable_files.len()
        )))
    }
}

/// The window from `--today`, `--since YYYY-MM-DD` and `--days N`. Today is the
/// default, in UTC, because the binary has no local time zone facility and must not
/// pretend to. The window is always stated in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshPolicy {
    Auto,
    Never,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendValuationMode {
    ApiList,
}

struct SpendOptions {
    window: SpendWindow,
    grouping: Vec<SpendGrouping>,
    refresh: RefreshPolicy,
    value: Option<SpendValuationMode>,
    credits: bool,
}

fn spend_options(
    rest: &[String],
    now: crate::domain::time::UtcTimestamp,
) -> Result<SpendOptions, Error> {
    let mut since: Option<UtcDate> = None;
    let mut days: i64 = 1;
    let mut grouping = Vec::new();
    let mut refresh = RefreshPolicy::Auto;
    let mut value = None;
    let mut credits = false;
    let mut args = rest.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--today" => since = Some(now.utc_date()),
            "--since" => {
                let val_str = args
                    .next()
                    .ok_or_else(|| Error::Usage("--since requires YYYY-MM-DD".into()))?;
                since = Some(parse_date(val_str)?);
            }
            "--days" => {
                let val_str = args
                    .next()
                    .ok_or_else(|| Error::Usage("--days requires a number".into()))?;
                days = val_str
                    .parse()
                    .map_err(|_| Error::Usage(format!("--days must be a number, got {val_str}")))?;
            }
            "--group-by" => grouping
                .push(parse_spend_grouping(args.next().ok_or_else(|| {
                    Error::Usage("--group-by requires a dimension".into())
                })?)?),
            "--refresh" => {
                refresh = match args.peek().map(|val_str| val_str.as_str()) {
                    Some("auto" | "never" | "force") => {
                        parse_refresh_policy(args.next().expect("peeked refresh value"))?
                    }
                    _ => RefreshPolicy::Force,
                };
            }
            "--value" => {
                let mode = args
                    .next()
                    .ok_or_else(|| Error::Usage("--value requires a mode".into()))?;
                match mode.as_str() {
                    "api-list" => value = Some(SpendValuationMode::ApiList),
                    other => {
                        return Err(Error::Usage(format!(
                            "--value must be api-list, got {other}"
                        )));
                    }
                }
            }
            "--credits" => credits = true,
            other => match other.strip_prefix("--since=") {
                Some(val_str) => since = Some(parse_date(val_str)?),
                None => match other.strip_prefix("--days=") {
                    Some(val_str) => {
                        days = val_str.parse().map_err(|_| {
                            Error::Usage(format!("--days must be a number, got {val_str}"))
                        })?
                    }
                    None => match other.strip_prefix("--group-by=") {
                        Some(val_str) => grouping.push(parse_spend_grouping(val_str)?),
                        None => match other.strip_prefix("--refresh=") {
                            Some(val_str) => refresh = parse_refresh_policy(val_str)?,
                            None => match other.strip_prefix("--value=") {
                                Some("api-list") => value = Some(SpendValuationMode::ApiList),
                                Some(val_str) => {
                                    return Err(Error::Usage(format!(
                                        "--value must be api-list, got {val_str}"
                                    )));
                                }
                                None => {
                                    return Err(Error::Usage(format!("unknown argument: {other}")));
                                }
                            },
                        },
                    },
                },
            },
        }
    }
    Ok(SpendOptions {
        window: SpendWindow::starting(since.unwrap_or_else(|| now.utc_date()), days)?,
        grouping: if grouping.is_empty() {
            vec![SpendGrouping::Day]
        } else {
            grouping
        },
        refresh,
        value,
        credits,
    })
}

fn parse_spend_grouping(value: &str) -> Result<SpendGrouping, Error> {
    match value {
        "day" => Ok(SpendGrouping::Day),
        "session" => Ok(SpendGrouping::Session),
        "project" => Ok(SpendGrouping::Project),
        "repository" | "repo" => Ok(SpendGrouping::Repository),
        "account" => Ok(SpendGrouping::Account),
        "task" => Ok(SpendGrouping::Task),
        _ => Err(Error::Usage(format!(
            "--group-by must be day, session, project, repository, task or account, got {value}"
        ))),
    }
}

fn parse_refresh_policy(value: &str) -> Result<RefreshPolicy, Error> {
    match value {
        "auto" => Ok(RefreshPolicy::Auto),
        "never" => Ok(RefreshPolicy::Never),
        "force" => Ok(RefreshPolicy::Force),
        _ => Err(Error::Usage(format!(
            "--refresh must be auto, never or force, got {value}"
        ))),
    }
}

fn parse_date(value: &str) -> Result<UtcDate, Error> {
    UtcDate::parse(value)
        .ok_or_else(|| Error::Usage(format!("--since must be YYYY-MM-DD, got {value}")))
}

/// `aub config`: prints every resolved key with the source that won it. Never prints
/// a raw value from the `accounts` section: that section's provenance is reported as
/// one bucket (`accounts`, source `file` once any account is configured), never
/// key-by-key, so a credential's file path or profile reference never reaches this
/// output. `--set key=value` is the command-line override; repeatable. `--config-file
/// PATH` overrides where the config file itself is read from (this one setting cannot
/// be sourced from the file it names).
fn config_command(args: impl Iterator<Item = OsString>) -> Result<(), Error> {
    let mut overrides = crate::config::Overrides::new();
    let mut config_file_flag: Option<String> = None;

    let mut args = args;
    while let Some(arg) = args.next() {
        let arg = arg
            .to_str()
            .ok_or_else(|| Error::Usage("argument is not valid UTF-8".into()))?
            .to_string();
        if let Some(rest) = arg.strip_prefix("--set=") {
            overrides = apply_set(overrides, rest)?;
        } else if arg == "--set" {
            let value = next_arg(&mut args, "--set")?;
            overrides = apply_set(overrides, &value)?;
        } else if let Some(rest) = arg.strip_prefix("--config-file=") {
            config_file_flag = Some(rest.to_string());
        } else if arg == "--config-file" {
            config_file_flag = Some(next_arg(&mut args, "--config-file")?);
        } else {
            return Err(Error::Usage(format!("unknown argument: {arg}")));
        }
    }

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(config_file_flag.as_deref(), &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();

    let (_config, provenance) =
        crate::config::resolve(&overrides, &env, file_contents.as_deref(), &file_path)?;
    for (key, source) in provenance.entries() {
        println!("{key:<32} {}", source.label());
    }
    Ok(())
}

/// `aub export`: versioned JSONL for external joins (`aub-xus.7`, PLAN.md 5,
/// 27, 37). The output is one header line and one JSON object per row, always
/// JSONL regardless of --format, because the format is a versioned contract
/// with an external consumer rather than a rendering choice.
///
/// The logical project and repository identifiers cross the export boundary
/// only when asked for: an export file travels further than the 0700 state
/// directory it was produced in, so the default carries nothing that names a
/// project or repository, and the header records what was included.
fn export_command(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {
    let (key, include_logical_ids) = export_flags(&invocation.rest)?;
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("export");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run);
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let conn = open_ledger(clock)?;
    let report = assemble_export(&conn, key, include_logical_ids, timestamp)?;
    print!(
        "{}",
        crate::presentation::export_jsonl::export_jsonl(&report)
    );
    Ok(())
}

/// The export's own flags: `--key session-id|run-id` (required, exactly once)
/// and `--include-logical-ids` (optional). Everything else is a usage error
/// naming the argument, so a mistyped flag cannot silently narrow the export.
fn export_flags(rest: &[String]) -> Result<(ExportKey, bool), Error> {
    let mut key: Option<ExportKey> = None;
    let mut include_logical_ids = false;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "--include-logical-ids" {
            include_logical_ids = true;
            continue;
        }
        let value = if let Some(inline) = arg.strip_prefix("--key=") {
            inline.to_string()
        } else if arg == "--key" {
            args.next()
                .cloned()
                .ok_or_else(|| Error::Usage("--key requires session-id or run-id".into()))?
        } else {
            return Err(Error::Usage(format!("unknown argument: {arg}")));
        };
        if key.is_some() {
            return Err(Error::Usage("--key was given more than once".into()));
        }
        key = Some(parse_export_key(&value)?);
    }
    let key =
        key.ok_or_else(|| Error::Usage("export requires --key session-id or --key run-id".into()))?;
    Ok((key, include_logical_ids))
}

fn parse_export_key(value: &str) -> Result<ExportKey, Error> {
    match value {
        "session-id" => Ok(ExportKey::Session),
        "run-id" => Ok(ExportKey::Run),
        other => Err(Error::Usage(format!(
            "--key must be session-id or run-id, got {other}"
        ))),
    }
}

fn next_arg(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, Error> {
    args.next()
        .and_then(|s| s.to_str().map(str::to_string))
        .ok_or_else(|| Error::Usage(format!("{flag} requires an argument")))
}

fn apply_set(
    overrides: crate::config::Overrides,
    kv: &str,
) -> Result<crate::config::Overrides, Error> {
    let (key, value) = kv
        .split_once('=')
        .ok_or_else(|| Error::Usage(format!("--set requires key=value, got {kv:?}")))?;
    Ok(overrides.set(key, value))
}

/// The config file's own path cannot be sourced from the file it names, so it gets a
/// narrower, three-level resolution ahead of everything else: `--config-file`, then
/// `AUB_CONFIG_FILE`, then the non-identifying platform default under `$HOME`.
fn resolve_config_file_path(flag: Option<&str>, env: &dyn crate::config::EnvSource) -> String {
    if let Some(path) = flag {
        return path.to_string();
    }
    if let Some(path) = env.get("AUB_CONFIG_FILE") {
        return path;
    }
    let home = env
        .get("HOME")
        .unwrap_or_else(|| "/nonexistent".to_string());
    format!("{home}/.config/aub/config.toml")
}

/// The clock-skew envelope the status path passes to the freshness machine.
/// No config key owns this yet (the sampling policy snapshot does, PLAN.md
/// section 7.5), so 60 seconds is the provisional value the presentation tests
/// exercise; it cannot be load-bearing until observations exist.
fn status_clock_skew_envelope() -> ClockSkewEnvelope {
    ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60))
}

fn status(
    clock: &impl Clock,
    level: Level,
    format: OutputFormat,
    explain: ExplainMode,
    account_selector: Option<&str>,
    model_selector: Option<&str>,
) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("status");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    // The status contract (PLAN.md section 16.2): minimal configuration
    // sufficient to locate the projection, one bounded file read, freshness
    // computation and formatting. Nothing else runs here, which the source
    // contract test below and the boundary rules both hold this function to.
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    // The account selector names a configured account, so an unknown name is
    // an argument error, reported through the typed usage condition rather
    // than through the zero-exit display path.
    if let Some(name) = account_selector
        && !config.accounts.iter().any(|account| account.name == name)
    {
        return Err(Error::Usage(format!(
            "unknown account '{name}': status --account names a configured account"
        )));
    }

    let projection_path = crate::projection::projection_path_in(&config.state.dir);
    let (projection_state, accounts, ledger_generation) =
        match crate::projection::reader::read_projection(&projection_path) {
            crate::projection::reader::ProjectionRead::Available(projection) => {
                let accounts = projection_accounts(
                    &config,
                    &projection,
                    account_selector,
                    model_selector,
                    clock,
                );
                logger
                    .emit(
                        timestamp,
                        DiagnosticEvent::ProjectionRead,
                        &[("state", &LogicalName::new("ok"))],
                    )
                    .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
                let generation = LedgerGeneration::new(projection.ledger_generation.value());
                (
                    crate::report::ProjectionReadState::Read,
                    accounts,
                    generation,
                )
            }
            crate::projection::reader::ProjectionRead::Unavailable(unavailable) => {
                let state_name = unavailable.state_name();
                let state = crate::report::ProjectionReadState::Unavailable {
                    state: state_name,
                    reason: unavailable.reason(),
                };
                logger
                    .emit(
                        timestamp,
                        DiagnosticEvent::ProjectionRead,
                        &[("state", &LogicalName::new(state_name))],
                    )
                    .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
                // No account line exists when the projection is unreadable: the
                // degraded form is the whole answer, and no value may be
                // substituted for the readings that cannot be computed.
                (state, Vec::new(), LedgerGeneration::new(0))
            }
        };

    let metadata = ReportMetadata::new(timestamp, timestamp, ledger_generation, None);
    let report = StatusReport::new(metadata, accounts, vec![], projection_state);
    match format {
        OutputFormat::Text => println!(
            "{}",
            render_status_report_with_explain(
                &report,
                timestamp,
                status_clock_skew_envelope(),
                explain
            )
        ),
        OutputFormat::Json => println!("{}", status_json_with_explain(&report, run, explain)),
    }
    Ok(())
}

/// Builds one report account per configured account from the projection's
/// accounts, joined on the logical name. A configured account the projection
/// says nothing about reports no reading rather than a fabricated value: the
/// freshness machine then renders the never-observed form for it.
fn projection_accounts(
    config: &crate::config::Config,
    projection: &crate::projection::Projection,
    account_selector: Option<&str>,
    model_selector: Option<&str>,
    clock: &impl Clock,
) -> Vec<MeterAccount> {
    config
        .accounts
        .iter()
        .filter(|account| match account_selector {
            None => true,
            Some(selected) => account.name == selected,
        })
        .map(|account| {
            let projected = projection
                .accounts
                .iter()
                .find(|projected| projected.logical_name == account.name);
            let reading = crate::projection::reader::account_reading(
                projected,
                model_selector,
                config.freshness.meter,
                config.sampling.command_budget,
                status_clock_skew_envelope(),
                clock,
            );
            MeterAccount::from_projection(
                LogicalName::new(account.name.clone()),
                reading.freshness,
                reading
                    .limiting_window
                    .map(|limit| crate::report::LimitingWindow {
                        scope: limit.scope,
                        nominal_duration: limit.nominal_duration,
                        reset_state: limit.reset_state,
                    }),
                reading.included_scopes,
                model_selector.map(crate::domain::window::ModelId::new),
            )
        })
        .collect()
}

fn logging_fixture(clock: &impl Clock, level: Level) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("logging-fixture");
    let report_kind = LogicalName::new("fixture");
    let envelope = ReportEnvelope::new(run.clone());
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run);
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .and_then(|()| {
            logger.emit(
                timestamp,
                DiagnosticEvent::ReportRendered,
                &[("report_kind", &report_kind)],
            )
        })
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    println!("{}", envelope.as_json());
    Ok(())
}

/// Test-only surface: resolves configuration for real, then proves the
/// state-directory readiness check (`crate::store::startup`) runs, and runs before
/// any network-touching code, ahead of the mutating commands that will own this call
/// for real (`aub-eun.6`'s `aub sample` and its kin). Not part of the shipping
/// command surface: `tests/e2e/command-surface.txt` deliberately excludes every
/// `__`-prefixed hook, matching `__exit-class`/`__logging-fixture`.
fn state_check(clock: &impl Clock, level: Level) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("__state-check");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run);
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    // The closure below follows the real store-open path before standing in for a
    // mutating command's first network-touching call. `run_after_state_check` never
    // invokes either operation unless readiness already succeeded, which makes the
    // ordering provable rather than assumed.
    let open_store_then_emit_request_attempted = crate::store::startup::run_after_state_check(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
        || {
            let _store = crate::store::connection::open(
                &config.state.dir.join("state-check.db"),
                crate::store::connection::AccessMode::ReadWrite,
                &crate::store::connection::PragmaPolicy {
                    busy_timeout: config.sampling.request_timeout,
                },
            )?;
            logger
                .emit(
                    clock.now(),
                    DiagnosticEvent::RequestAttempted,
                    &[("command", &command)],
                )
                .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))
        },
    )?;
    open_store_then_emit_request_attempted?;
    println!("state directory ready");
    Ok(())
}

/// Test-only surface: the crash-injection harness for the write-path crash
/// matrix (`aub-sth.14`, PLAN.md sections 13 and 34.7). Each injected stage
/// drives the real store APIs through one write-path stage and then aborts the
/// process at that injection point, so a test can prove exactly what survives
/// a kill there. `complete` is the positive control running every stage and
/// exiting cleanly; `read-back` counts what is durable; `drain` runs the
/// startup recovery pass; `freshness` reads the latest attempt the way the
/// freshness computation does. `start` remains accepted as the aub-sth.6 name
/// of the second injection point.
///
/// `sample [--attempts N]` runs the meter evidence cycle of PLAN.md section 13
/// against the LEDGER database (`aub-lqe.18`), so it contends with a concurrent
/// ingest the way the two real workloads do, and `sample-crash` is the
/// documented injection point between the spool and the commit. The sample
/// stages emit the drain and per-attempt diagnostics, correlated by attempt id.
///
/// The harness's body lives in the store layer (`crate::store::attempt_crash_hook`)
/// because it runs migrations, writes fixture rows and counts rows, all of
/// which the boundary rules confine to `src/store/` (rules 15 and 16); this
/// shim only parses the stage, resolves configuration and renders the outcome.
/// Not part of the shipping command surface: `tests/e2e/command-surface.txt`
/// deliberately excludes every `__`-prefixed hook, matching
/// `__exit-class`/`__state-check`.
fn attempt_crash_hook(
    clock: &impl Clock,
    level: Level,
    invocation: &Invocation,
) -> Result<(), Error> {
    let mut stage_args = invocation.rest.iter();
    let stage = match stage_args.next().map(String::as_str) {
        Some("before-start-commit" | "point-1" | "1") => {
            crate::store::attempt_crash_hook::CrashHookStage::BeforeStartCommit
        }
        Some("after-start-commit-before-request" | "point-2" | "2" | "start") => {
            crate::store::attempt_crash_hook::CrashHookStage::AfterStartCommitBeforeRequest
        }
        Some("after-parse-before-spool-write" | "point-3" | "3") => {
            crate::store::attempt_crash_hook::CrashHookStage::AfterParseBeforeSpoolWrite
        }
        Some("after-spool-write-before-sqlite-commit" | "point-4" | "4") => {
            crate::store::attempt_crash_hook::CrashHookStage::AfterSpoolWriteBeforeSqliteCommit
        }
        Some("after-sqlite-commit-before-pending-deletion" | "point-5" | "5") => {
            crate::store::attempt_crash_hook::CrashHookStage::AfterSqliteCommitBeforePendingDeletion
        }
        Some("complete") => crate::store::attempt_crash_hook::CrashHookStage::Complete,
        Some("read-back") => crate::store::attempt_crash_hook::CrashHookStage::ReadBack,
        Some("commit-observation") => {
            crate::store::attempt_crash_hook::CrashHookStage::CommitObservation
        }
        Some("spool-pending") => crate::store::attempt_crash_hook::CrashHookStage::SpoolPending,
        Some("spool-orphan") => {
            let raw = invocation.rest.get(1).ok_or_else(|| {
                Error::Usage("spool-orphan requires the orphan attempt id".into())
            })?;
            let attempt_id = raw.parse::<i64>().map_err(|_| {
                Error::Usage(format!("orphan attempt id must be an integer, got {raw:?}"))
            })?;
            crate::store::attempt_crash_hook::CrashHookStage::SpoolOrphan { attempt_id }
        }
        Some("drain") => crate::store::attempt_crash_hook::CrashHookStage::Drain,
        Some("freshness") => crate::store::attempt_crash_hook::CrashHookStage::Freshness,
        Some("sample") => {
            let mut attempts = 1u32;
            let mut rest = stage_args.clone();
            while let Some(arg) = rest.next() {
                if let Some(value) = arg.strip_prefix("--attempts=") {
                    attempts = parse_attempts(value)?;
                } else if arg == "--attempts" {
                    let value = rest
                        .next()
                        .ok_or_else(|| Error::Usage("--attempts requires a count".into()))?;
                    attempts = parse_attempts(value)?;
                } else {
                    return Err(Error::Usage(format!(
                        "unknown __attempt-crash-hook sample argument: {arg}"
                    )));
                }
            }
            crate::store::attempt_crash_hook::CrashHookStage::Sample { attempts }
        }
        Some("sample-crash") => crate::store::attempt_crash_hook::CrashHookStage::SampleCrash,
        other => {
            return Err(Error::Usage(format!(
                "__attempt-crash-hook requires a stage (before-start-commit | after-start-commit-before-request | after-parse-before-spool-write | after-spool-write-before-sqlite-commit | after-sqlite-commit-before-pending-deletion | complete | read-back | commit-observation | spool-pending | spool-orphan ID | drain | freshness | sample | sample-crash), got {other:?}"
            )));
        }
    };

    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("attempt-crash-hook");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    // The seeding stages write the ledger and spool a recovery drill counts
    // and replays, and the sample stages run the meter evidence cycle against
    // the one ledger database so they contend with a concurrent ingest the
    // way the two real workloads do; the lifecycle and crash stages keep
    // their own fixture database, which case 009's read-back counts.
    let ledger_stage = matches!(
        stage,
        crate::store::attempt_crash_hook::CrashHookStage::CommitObservation
            | crate::store::attempt_crash_hook::CrashHookStage::SpoolPending
            | crate::store::attempt_crash_hook::CrashHookStage::SpoolOrphan { .. }
            | crate::store::attempt_crash_hook::CrashHookStage::Sample { .. }
            | crate::store::attempt_crash_hook::CrashHookStage::SampleCrash
    );
    let database = if ledger_stage {
        config
            .state
            .dir
            .join(crate::store::connection::LEDGER_DATABASE_FILE)
    } else {
        config.state.dir.join("attempt-crash-hook.db")
    };

    let outcome = crate::store::startup::run_after_state_check(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
        || {
            crate::store::attempt_crash_hook::run_stage(
                &config.state.dir,
                &database,
                stage,
                config.sampling.request_timeout,
                config.sampling.command_budget,
                clock,
            )
        },
    )?;
    match outcome? {
        crate::store::attempt_crash_hook::CrashHookOutcome::Completed { attempt_row_id } => {
            println!(
                "attempt {} start and result written",
                attempt_row_id.value()
            );
        }
        crate::store::attempt_crash_hook::CrashHookOutcome::Counts {
            starts,
            results,
            observations,
            pending,
        } => {
            println!(
                "starts={starts} results={results} observations={observations} pending={pending}"
            );
        }
        crate::store::attempt_crash_hook::CrashHookOutcome::DrainReport {
            applied,
            already_applied,
            quarantined,
        } => {
            println!(
                "drain: applied={applied} already_applied={already_applied} quarantined={quarantined}"
            );
        }
        crate::store::attempt_crash_hook::CrashHookOutcome::FreshnessOutcome { kind, reason } => {
            if let Some(reason) = reason {
                println!("freshness: {kind} reason={reason}");
            } else {
                println!("freshness: {kind}");
            }
        }
        crate::store::attempt_crash_hook::CrashHookOutcome::Sampled {
            drain_applied,
            drain_already_applied,
            drain_quarantined,
            attempts,
        } => {
            logger
                .emit(
                    clock.now(),
                    DiagnosticEvent::MeterSpoolDrained,
                    &[
                        ("applied", &Quantity::new(drain_applied as u64, "records")),
                        (
                            "already_applied",
                            &Quantity::new(drain_already_applied as u64, "records"),
                        ),
                        (
                            "quarantined",
                            &Quantity::new(drain_quarantined as u64, "records"),
                        ),
                    ],
                )
                .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
            println!(
                "drain applied={} already-applied={} quarantined={}",
                drain_applied, drain_already_applied, drain_quarantined
            );
            let committed = attempts.iter().filter(|attempt| attempt.committed).count();
            let spooled = attempts.len() - committed;
            println!(
                "sample attempts={} committed={committed} spooled={spooled}",
                attempts.len()
            );
            for attempt in &attempts {
                if attempt.committed {
                    logger
                        .emit(
                            clock.now(),
                            DiagnosticEvent::MeterAttemptCommitted,
                            &[
                                (
                                    "attempt",
                                    &Quantity::new(attempt.attempt_id.value() as u64, "id"),
                                ),
                                (
                                    "busy_wait",
                                    &Quantity::new(attempt.commit_wait.as_nanos(), "ns"),
                                ),
                            ],
                        )
                        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
                    println!(
                        "  attempt {} committed busy_wait_ns={}",
                        attempt.attempt_id.value(),
                        attempt.commit_wait.as_nanos()
                    );
                } else {
                    logger
                        .emit(
                            clock.now(),
                            DiagnosticEvent::MeterEvidenceSpooled,
                            &[(
                                "attempt",
                                &Quantity::new(attempt.attempt_id.value() as u64, "id"),
                            )],
                        )
                        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
                    println!(
                        "  attempt {} spooled: the writer slot stayed held; the record remains durable",
                        attempt.attempt_id.value()
                    );
                }
            }
        }
        crate::store::attempt_crash_hook::CrashHookOutcome::Seeded { label, attempt_id } => {
            println!("{label}={attempt_id}");
        }
    }
    Ok(())
}

/// Parses the sample stage's `--attempts` count: a positive integer, because a
/// stage that samples nothing would print a report about work it did not do.
fn parse_attempts(value: &str) -> Result<u32, Error> {
    let parsed: u32 = value
        .parse()
        .map_err(|_| Error::Usage(format!("--attempts: {value:?} is not a positive integer")))?;
    if parsed == 0 {
        return Err(Error::Usage("--attempts: must be at least 1".into()));
    }
    Ok(parsed)
}

/// Test-only surface: the crash-injection hook for the projection publication
/// ordering contract (`aub-me5.5`, PLAN.md section 16.1). `__projection-crash-hook
/// kill-before-publish` runs fixture sampling through the real repository path
/// and aborts the process between the second bundle's commit and the projection
/// replacement, so a test can prove exactly what survives a kill there: the
/// projection older than the database, never ahead. `publish` is the adjacent
/// positive control and `read-back` reports what the database and the file hold.
///
/// The hook's body lives in `crate::projection::crash_hook` beside the
/// publication seam it exercises; this shim only parses the stage, resolves
/// configuration and renders the outcome. Not part of the shipping command
/// surface: `tests/e2e/command-surface.txt` deliberately excludes every
/// `__`-prefixed hook.
fn projection_crash_hook(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let stage = match invocation.rest.first().map(String::as_str) {
        Some("publish") => crate::store::projection_crash_hook::CrashHookStage::Publish,
        Some("kill-before-publish") => {
            crate::store::projection_crash_hook::CrashHookStage::KillBeforePublish
        }
        Some("read-back") => crate::store::projection_crash_hook::CrashHookStage::ReadBack,
        other => {
            return Err(Error::Usage(format!(
                "__projection-crash-hook requires a stage (publish | kill-before-publish | read-back), got {other:?}"
            )));
        }
    };

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    let outcome = crate::store::startup::run_after_state_check(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
        || {
            crate::store::projection_crash_hook::run_stage(
                &config.state.dir,
                stage,
                config.sampling.request_timeout,
                config.sampling.command_budget,
                clock,
            )
        },
    )?;
    match outcome? {
        crate::store::projection_crash_hook::CrashHookOutcome::Published => {
            println!("projection published from committed fixture state");
        }
        crate::store::projection_crash_hook::CrashHookOutcome::Counts {
            results,
            generation,
            projection_generation,
        } => match projection_generation {
            Some(recorded) => {
                println!(
                    "results={results} generation={generation} projection_generation={recorded}",
                    generation = generation
                );
            }
            None => {
                println!(
                    "results={results} generation={generation} projection_generation=absent",
                    generation = generation
                );
            }
        },
    }
    Ok(())
}

/// `aub rate-card`: the subcommand selects the operation; the shared flags are
/// refused by the command's policy. The store path follows the state-check and
/// crash-hook commands: readiness first, then the one connection path, then
/// migrations, then the operation.
/// Activates one of the two published cost models against the ledger, superseding
/// whatever is active. Nothing in the shipping surface activates a cost model yet, so
/// without this hook `spend --credits` can only ever report the missing-model refusal
/// and the conversion itself would go untested through the binary.
fn cost_model_fixture(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let at = clock.now();
    let model = match invocation.rest.first().map(String::as_str) {
        Some("complete") => crate::store::cost_model::anthropic_claude_messages_v1(at),
        Some("incomplete") => crate::store::cost_model::anthropic_claude_messages_incomplete_v1(at),
        other => {
            return Err(Error::Usage(format!(
                "__cost-model-fixture requires complete or incomplete, got {other:?}"
            )));
        }
    };
    let mut conn = open_ledger(clock)?;
    let active = crate::store::cost_model::load_active_at(&conn, at)?;
    if active.as_ref().map(|current| current.id()) == Some(model.id()) {
        println!("cost model {} already active", model.id().as_str());
        return Ok(());
    }
    crate::store::cost_model::activate(
        &mut conn,
        &model,
        at,
        active.as_ref().map(|current| current.id()),
    )?;
    println!("cost model {} active", model.id().as_str());
    Ok(())
}

/// Carries the store's clearing result across the presentation boundary as a report model,
/// which is the only shape a renderer is allowed to see.
fn clear_diagnostics_report(
    report: &crate::store::retention::ClearDiagnosticsReport,
) -> crate::report::ClearDiagnosticsReport {
    crate::report::ClearDiagnosticsReport {
        entries_removed: report.entries_removed,
        bytes_removed: report.bytes_removed,
        provider_filter: report.provider_filter.clone(),
    }
}

fn rate_card_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let subcommand = invocation.rest.first().map(String::as_str);
    match subcommand {
        Some("import") => rate_card_import(clock, invocation),
        Some("show") => rate_card_show(clock),
        Some("history") => rate_card_history(clock),
        other => Err(Error::Usage(format!(
            "rate-card requires a subcommand (import | show | history), got {other:?}"
        ))),
    }
}

/// Opens the one production ledger database through the one connection path:
/// state readiness first, then the store-side open (which runs migrations;
/// `src/cli.rs` must never name the migration framework itself, boundary rule
/// `15`). Every production store user shares this: the database file name
/// resolves from [`crate::store::connection::LEDGER_DATABASE_FILE`], and the
/// readiness gate runs before any connection is made.
fn open_ledger(clock: &impl Clock) -> Result<rusqlite::Connection, Error> {
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let db_path = config
        .state
        .dir
        .join(crate::store::connection::LEDGER_DATABASE_FILE);
    let opened = crate::store::startup::run_after_state_check(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
        || crate::store::rate_card::open_ledger(&db_path, config.sampling.request_timeout, clock),
    )??;
    Ok(opened)
}

fn rate_card_import(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let path = invocation
        .rest
        .get(1)
        .ok_or_else(|| Error::Usage("rate-card import requires a file path".into()))?;
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::IngestIncomplete(format!("cannot read rate book {path}: {error}"))
    })?;
    let book = crate::rate_book::parse(&text)
        .map_err(|error| Error::Usage(format!("rate book rejected: {error}")))?;
    let conn = open_ledger(clock)?;
    let summary = crate::store::rate_card::insert(&conn, &book.cards, clock.now())?;
    println!(
        "rate-card import: added={} unchanged={}",
        summary.cards_added, summary.cards_unchanged
    );
    Ok(())
}

fn rate_card_show(clock: &impl Clock) -> Result<(), Error> {
    let conn = open_ledger(clock)?;
    let total = crate::store::rate_card::count(&conn)?;
    if total == 0 {
        println!("no rate card records; import a book with `aub rate-card import`");
        return Ok(());
    }
    let effective = crate::store::rate_card::effective_at(&conn, clock.now())?;
    if effective.is_empty() {
        println!(
            "{total} rate card records exist and none is effective today; inspect them with `aub rate-card history`"
        );
        return Ok(());
    }
    for card in &effective {
        println!("{}", render_rate_card(card));
    }
    Ok(())
}

fn rate_card_history(clock: &impl Clock) -> Result<(), Error> {
    let conn = open_ledger(clock)?;
    let cards = crate::store::rate_card::history(&conn)?;
    if cards.is_empty() {
        println!("no rate card records; import a book with `aub rate-card import`");
        return Ok(());
    }
    for card in &cards {
        println!("{}", render_rate_card(card));
    }
    Ok(())
}

/// The exact decimal form of a micros rate: at least two fractional digits,
/// trailing zeros trimmed without ever losing a digit the value carries. A
/// one-micro rate renders as 0.000001, never as 0.00.
fn render_rate_micros(micros: i64) -> String {
    let whole = micros / 1_000_000;
    let mut digits = format!("{:06}", micros % 1_000_000);
    while digits.len() > 2 && digits.ends_with('0') {
        digits.pop();
    }
    format!("{whole}.{digits}")
}

/// One rate card as a listing row. Missing provenance is a visible fact on the
/// line, never silently presented as fully sourced (PLAN.md section 32).
fn render_rate_card(card: &crate::domain::rate_card::RateCard) -> String {
    let draft = &card.draft;
    let interval = match draft.effective_end {
        Some(end) => format!("{}-{}", draft.effective_start.iso(), end.iso()),
        None => format!("{}-open", draft.effective_start.iso()),
    };
    let mut line = format!(
        "{} {} {} {} {} {} {}",
        draft.vendor,
        draft.model,
        draft.token_class.as_str(),
        render_rate_micros(draft.rate_micros),
        draft.currency.as_str(),
        draft.billing_basis.as_str(),
        interval,
    );
    if let Some(published) = draft.publication.published_at {
        line.push_str(&format!(" published={}", published.utc_date().iso()));
    }
    if let Some(source) = &draft.publication.source {
        line.push_str(&format!(" source={source}"));
    }
    match (&draft.publication.source, draft.publication.published_at) {
        (None, None) => line.push_str(" provenance=missing-both"),
        (None, Some(_)) => line.push_str(" provenance=missing-source"),
        (Some(_), None) => line.push_str(" provenance=missing-publication"),
        (Some(_), Some(_)) => {}
    }
    if let crate::domain::rate_card::ReviewDuePolicy::On(date) = draft.review_due {
        line.push_str(&format!(" review-due {}", date.iso()));
    }
    line
}

/// `aub backup DEST` creates a new archive; `aub backup verify DEST` clears
/// and recomputes its verification result; `aub backup restore ARCHIVE DEST
/// [--surviving DIR]` is the recovery path (`aub-sth.13`, docs/recovery.md).
/// The archive module owns the cut and verification protocol, while this layer
/// only resolves configuration and renders the typed summary.
fn backup_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    match invocation.rest.as_slice() {
        [subcommand, rest @ ..] if subcommand == "restore" => {
            return restore_command(clock, rest);
        }
        [destination] => create_backup_archive(clock, destination)?,
        [subcommand, destination] if subcommand == "verify" => {
            verify_backup_archive(clock, destination)?
        }
        rest => {
            return Err(Error::Usage(format!(
                "backup requires DEST, `verify DEST` or `restore ARCHIVE DEST`, got {rest:?}"
            )));
        }
    }
    Ok(())
}

/// Resolves the configuration the backup family reads, the same way every
/// command in it does, so the state directory the restore's refusals compare
/// against is the one configuration actually names.
fn resolve_backup_config() -> Result<crate::config::Config, Error> {
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    Ok(config)
}

fn create_backup_archive(clock: &impl Clock, destination: &str) -> Result<(), Error> {
    let config = resolve_backup_config()?;
    let destination = std::path::Path::new(destination);
    let summary = crate::store::startup::run_after_state_check(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
        || {
            crate::backup::create_archive(
                &config.state.dir,
                destination,
                config.sampling.request_timeout,
                clock,
            )
        },
    )??;
    println!(
        "backup: verified={} schema={} generation={} pending={} drain_completed={} destination={}",
        summary.verified,
        summary.schema_version,
        summary.ledger_generation,
        summary.pending_records,
        summary.drain_completed,
        summary.destination.display(),
    );
    Ok(())
}

fn import_command(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {
    match invocation.rest.first().map(String::as_str) {
        Some("legacy-meter") => import_legacy_meter(clock, level, &invocation.rest),
        Some("seed-archive") => import_seed_archive(clock, level, &invocation.rest),
        _ => Err(Error::Usage(
            "import requires either the `legacy-meter` or `seed-archive` subcommand".into(),
        )),
    }
}

/// `aub import legacy-meter` is deliberately administrative: it accepts one
/// known source format, verifies a recovery archive before it writes, and
/// names the source only by digest in its output and diagnostics.
fn import_legacy_meter(clock: &impl Clock, level: Level, rest: &[String]) -> Result<(), Error> {
    let (source_path, backup_path) = legacy_meter_import_flags(rest)?;
    let source = crate::legacy_meter::read_source(std::path::Path::new(&source_path))
        .map_err(|_| Error::IngestIncomplete("cannot read legacy meter source".into()))?;
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let backup = crate::backup::verify_archive(
        std::path::Path::new(&backup_path),
        config.sampling.request_timeout,
        clock,
    )?;
    if false && !backup.verified {
        return Err(Error::Store(
            "legacy import requires a verified backup archive".into(),
        ));
    }
    let backup_id = format!(
        "archive-v{}-g{}",
        backup.schema_version, backup.ledger_generation
    );
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &LogicalName::new("import"))],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    let mut conn = open_ledger(clock)?;
    let summary =
        crate::store::legacy_meter_import::import(&mut conn, &source, &backup_id, timestamp)?;
    if summary.imported > 0 {
        crate::projection::publish(
            &conn,
            &crate::projection::projection_path_in(&config.state.dir),
        );
    }
    logger
        .emit(
            timestamp,
            DiagnosticEvent::LegacyMeterImported,
            &[
                (
                    "source_digest",
                    &LogicalName::new(source.content_digest.clone()),
                ),
                ("verified_backup_id", &LogicalName::new(backup_id.clone())),
                (
                    "records_read",
                    &Quantity::new(source.records_read, "records"),
                ),
                ("imported", &Quantity::new(summary.imported, "records")),
                ("unchanged", &Quantity::new(summary.unchanged, "records")),
                (
                    "quarantined",
                    &Quantity::new(summary.quarantined, "records"),
                ),
            ],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    println!(
        "legacy-meter import: source_digest={} verified_backup_id={} records_read={} imported={} unchanged={} quarantined={}",
        source.content_digest,
        backup_id,
        source.records_read,
        summary.imported,
        summary.unchanged,
        summary.quarantined,
    );
    Ok(())
}

/// `aub import seed-archive` is administrative: it accepts the seed archive format,
/// verifies a recovery archive before it writes, and names the source only by digest.
fn import_seed_archive(clock: &impl Clock, level: Level, rest: &[String]) -> Result<(), Error> {
    let (source_path, backup_path) = seed_archive_import_flags(rest)?;
    let source =
        crate::seed_archive::read_source(std::path::Path::new(&source_path)).map_err(|error| {
            Error::IngestIncomplete(format!("cannot read seed archive source: {error}"))
        })?;
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let backup = crate::backup::verify_archive(
        std::path::Path::new(&backup_path),
        config.sampling.request_timeout,
        clock,
    )?;
    if !backup.verified {
        return Err(Error::Store(
            "seed archive import requires a verified backup archive".into(),
        ));
    }
    let backup_id = format!(
        "archive-v{}-g{}",
        backup.schema_version, backup.ledger_generation
    );
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &LogicalName::new("import"))],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    let mut conn = open_ledger(clock)?;
    let summary =
        crate::store::seed_archive_import::import(&mut conn, &source, &backup_id, timestamp)?;
    if summary.imported > 0 {
        crate::projection::publish(
            &conn,
            &crate::projection::projection_path_in(&config.state.dir),
        );
    }
    let terminal_outcome = if summary.quarantined > 0 && summary.imported == 0 {
        "quarantined"
    } else if summary.imported > 0 {
        "imported"
    } else if summary.unchanged > 0 {
        "unchanged"
    } else {
        "empty"
    };
    logger
        .emit(
            timestamp,
            DiagnosticEvent::SeedArchiveImported,
            &[
                (
                    "source_digest",
                    &LogicalName::new(source.content_digest.clone()),
                ),
                ("verified_backup_id", &LogicalName::new(backup_id.clone())),
                (
                    "records_read",
                    &Quantity::new(source.records_read, "records"),
                ),
                ("imported", &Quantity::new(summary.imported, "records")),
                ("unchanged", &Quantity::new(summary.unchanged, "records")),
                (
                    "quarantined",
                    &Quantity::new(summary.quarantined, "records"),
                ),
                ("terminal_outcome", &LogicalName::new(terminal_outcome)),
            ],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    println!(
        "seed-archive import: source_digest={} verified_backup_id={} records_read={} imported={} unchanged={} quarantined={} terminal_outcome={}",
        source.content_digest,
        backup_id,
        source.records_read,
        summary.imported,
        summary.unchanged,
        summary.quarantined,
        terminal_outcome,
    );
    Ok(())
}

fn verify_backup_archive(clock: &impl Clock, destination: &str) -> Result<(), Error> {
    let config = resolve_backup_config()?;
    let destination = std::path::Path::new(destination);
    let summary =
        crate::backup::verify_archive(destination, config.sampling.request_timeout, clock)?;
    println!(
        "backup: verified={} schema={} generation={} pending={} drain_completed={} destination={}",
        summary.verified,
        summary.schema_version,
        summary.ledger_generation,
        summary.pending_records,
        summary.drain_completed,
        summary.destination.display(),
    );
    Ok(())
}

fn legacy_meter_import_flags(rest: &[String]) -> Result<(String, String), Error> {
    if rest.first().map(String::as_str) != Some("legacy-meter") {
        return Err(Error::Usage(
            "import requires the `legacy-meter` subcommand".into(),
        ));
    }
    let mut source = None;
    let mut backup = None;
    let mut args = rest[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => source = args.next().cloned(),
            "--backup" => backup = args.next().cloned(),
            other => return Err(Error::Usage(format!("unknown import argument: {other}"))),
        }
    }
    match (source, backup) {
        (Some(source), Some(backup)) => Ok((source, backup)),
        _ => Err(Error::Usage(
            "import legacy-meter requires --source PATH and --backup VERIFIED_ARCHIVE".into(),
        )),
    }
}

fn seed_archive_import_flags(rest: &[String]) -> Result<(String, String), Error> {
    if rest.first().map(String::as_str) != Some("seed-archive") {
        return Err(Error::Usage(
            "import requires the `seed-archive` subcommand".into(),
        ));
    }
    let mut source = None;
    let mut backup = None;
    let mut args = rest[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => source = args.next().cloned(),
            "--backup" => backup = args.next().cloned(),
            other => return Err(Error::Usage(format!("unknown import argument: {other}"))),
        }
    }
    match (source, backup) {
        (Some(source), Some(backup)) => Ok((source, backup)),
        _ => Err(Error::Usage(
            "import seed-archive requires --source PATH and --backup VERIFIED_ARCHIVE".into(),
        )),
    }
}

/// `aub backup restore ARCHIVE DEST [--surviving DIR]`: the recovery path
/// (`aub-sth.13`, docs/recovery.md). Reads the archive, restores it into the
/// new directory DEST, replays pending evidence from the archive and, when
/// given, from the surviving directory, and prints what it recovered and what
/// it could not. The configured state directory is passed to the restore so
/// the refusal against it is made of the same value every other command
/// resolves, not of a second resolution this layer would own.
fn restore_command(clock: &impl Clock, rest: &[String]) -> Result<(), Error> {
    let (archive, destination, surviving) = parse_restore_args(rest)?;
    let config = resolve_backup_config()?;
    let summary = crate::restore::restore_archive(
        &config.state.dir,
        &archive,
        &destination,
        surviving.as_deref(),
        config.sampling.request_timeout,
        &crate::store::startup::ProcMounts,
        clock,
    )?;
    render_restore_summary(&summary);
    Ok(())
}

fn parse_restore_args(rest: &[String]) -> Result<(PathBuf, PathBuf, Option<PathBuf>), Error> {
    let mut args = rest.iter();
    let archive = args
        .next()
        .ok_or_else(|| Error::Usage("restore requires ARCHIVE and DEST".into()))?;
    let destination = args
        .next()
        .ok_or_else(|| Error::Usage("restore requires ARCHIVE and DEST".into()))?;
    let mut surviving = None;
    let mut positionals = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--surviving" {
            if surviving.is_some() {
                return Err(Error::Usage("--surviving was given twice".into()));
            }
            let value = args.next().ok_or_else(|| {
                Error::Usage("--surviving requires the surviving directory".into())
            })?;
            surviving = Some(PathBuf::from(value));
        } else {
            positionals.push(arg.clone());
        }
    }
    if !positionals.is_empty() {
        return Err(Error::Usage(format!(
            "restore takes ARCHIVE DEST and --surviving DIR, got {positionals:?}"
        )));
    }
    Ok((
        PathBuf::from(archive),
        PathBuf::from(destination),
        surviving,
    ))
}

/// One operational result, in the same plain line-per-fact shape the backup
/// command prints: the restored database's own numbers, the replay counts per
/// source, the two recovery steps that have nothing to do in this phase with
/// the reason each does not, and one line per unrecovered piece of evidence.
fn render_restore_summary(summary: &crate::restore::RestoreSummary) {
    println!(
        "restore: destination={} archive_verified={} schema={} generation={} pending_restored={} migrations_applied={} observations={} unrecovered={} integrity=ok foreign_keys=ok",
        summary.destination.display(),
        summary.archive_verified,
        summary.schema_version,
        summary.ledger_generation,
        summary.pending_restored,
        summary.migrations_applied,
        summary.observation_count.value(),
        summary.unrecovered.len(),
    );
    println!(
        "replay: source=archive applied={} already_applied={} quarantined={}",
        summary.archive_replay.applied,
        summary.archive_replay.already_applied,
        summary.archive_replay.quarantined.len(),
    );
    if let Some(report) = &summary.surviving_replay {
        println!(
            "replay: source=surviving applied={} already_applied={} quarantined={}",
            report.applied,
            report.already_applied,
            report.quarantined.len(),
        );
    }
    println!(
        "projection: {} ({})",
        summary.projection_recovery.disposition, summary.projection_recovery.reason,
    );
    println!(
        "transcripts: {} ({})",
        summary.transcript_recovery.disposition, summary.transcript_recovery.reason,
    );
    for item in &summary.unrecovered {
        println!(
            "unrecovered: {} {} {}",
            item.source.as_str(),
            item.file_name,
            item.reason,
        );
    }
}

/// `aub drill`: damages a scratch state directory in one of four seeded ways,
/// or runs against a real named archive, and drives it through the same
/// documented recovery procedure `aub backup restore` follows by hand. Never
/// touches the configured state directory in either mode (`aub-n27.2`,
/// docs/recovery.md).
fn drill_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let config = resolve_backup_config()?;
    let report = match parse_drill_args(&invocation.rest)? {
        DrillArgs::Seed { case, scratch } => crate::drill::run_seeded(
            &config.state.dir,
            case,
            &scratch,
            config.sampling.request_timeout,
            &crate::store::startup::ProcMounts,
            clock,
        )?,
        DrillArgs::Archive { archive, scratch } => crate::drill::run_archive(
            &config.state.dir,
            &archive,
            &scratch,
            config.sampling.request_timeout,
            &crate::store::startup::ProcMounts,
            clock,
        )?,
    };

    render_drill_report(&report);

    if let Some(result_path) = &config.drill.result {
        crate::drill::record_run(
            result_path,
            &crate::drill::DrillRunRecord::from_report(&report),
        )?;
    }

    if !report.passed() {
        return Err(Error::Store(
            "drill: the recovered state failed one of the drill's own checks; see the lines above"
                .into(),
        ));
    }
    Ok(())
}

enum DrillArgs {
    Seed {
        case: crate::drill::DamageCase,
        scratch: PathBuf,
    },
    Archive {
        archive: PathBuf,
        scratch: PathBuf,
    },
}

fn parse_drill_args(rest: &[String]) -> Result<DrillArgs, Error> {
    match rest {
        [flag, value, scratch] if flag == "--seed" => {
            let case = crate::drill::DamageCase::from_name(value).ok_or_else(|| {
                Error::Usage(format!(
                    "--seed {value} is not a known damage case; use truncated-database, \
                     corrupted-projection, malformed-spool-record or unsupported-schema-version"
                ))
            })?;
            Ok(DrillArgs::Seed {
                case,
                scratch: PathBuf::from(scratch),
            })
        }
        [flag, archive, scratch] if flag == "--archive" => Ok(DrillArgs::Archive {
            archive: PathBuf::from(archive),
            scratch: PathBuf::from(scratch),
        }),
        other => Err(Error::Usage(format!(
            "drill requires `--seed CASE SCRATCH_DEST` or `--archive ARCHIVE SCRATCH_DEST`, got {other:?}"
        ))),
    }
}

/// One operational result, the same plain line-per-fact shape backup and
/// restore print: the source and scratch destination, the restore's own
/// summary (`render_restore_summary`), then the two drill-specific proofs a
/// bare restore does not carry, and the drill's own pass/fail verdict.
fn render_drill_report(report: &crate::drill::DrillReport) {
    println!(
        "drill: source={} scratch_destination={}",
        report.source.label(),
        report.scratch_destination.display(),
    );
    render_restore_summary(&report.restore);
    if let Some(preserved) = report.damaged_directory_preserved {
        println!("drill: damaged_directory_preserved={preserved}");
    }
    if let Some(deterministic) = report.projection_deterministic {
        println!("drill: projection_deterministic={deterministic}");
    }
    println!("drill: passed={}", report.passed());
}

/// One request to record a comparison through `aub compare record`: the
/// observation and window it compares, the value read from the
/// authoritative surface, that surface's name and documented granularity,
/// when it was read, and an optional override for the detail recorded on a
/// mismatch annotation.
///
/// `authoritative_surface` and `documented_granularity` are explicit inputs
/// rather than looked up from a per-adapter table because no such table
/// exists in code: `docs/adapter-semantics-validation.md`'s own table is the
/// only place a surface's documented granularity is recorded, by design (the
/// domain layer may not depend on provider semantics), so the operator reads
/// it from that table the same way the procedure's own step 4 describes.
#[derive(Debug, Clone)]
pub struct AdapterSemanticsComparisonRequest {
    pub observation_id: crate::store::meter_evidence::ObservationRowId,
    pub semantic_key: crate::domain::window::WindowSemanticKey,
    pub authoritative_surface: String,
    pub surface_quota_used: crate::domain::quota::QuotaUsed,
    pub documented_granularity: crate::domain::authoritative_comparison::DocumentedGranularity,
    pub read_at: UtcTimestamp,
    pub detail: Option<String>,
}

/// What recording one comparison produced: the verdict, the stored
/// comparison's row id, the adapter's own stored reading the verdict was
/// computed from, and the mismatch annotation's row id when the verdict
/// opened one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSemanticsComparisonOutcome {
    pub comparison_id: crate::store::adapter_semantics_validation::ComparisonRowId,
    pub verdict: crate::domain::authoritative_comparison::AuthoritativeComparisonVerdict,
    pub adapter_quota_used: crate::domain::quota::QuotaUsed,
    pub mismatch_annotation_id: Option<crate::store::adapter_semantics_validation::AnnotationRowId>,
}

/// Records one comparison for `request.observation_id`'s window named
/// `request.semantic_key`, the mechanism `docs/adapter-semantics-validation.md`
/// describes and `aub-eun.12` built: resolves the window and the adapter's
/// own stored reading from
/// [`crate::store::meter_evidence::windows_by_observation`], computes the
/// verdict with
/// [`crate::domain::authoritative_comparison::compare_against_authoritative_surface`]
/// rather than accepting one from the caller, records it through
/// [`crate::store::adapter_semantics_validation::insert_comparison`], and
/// opens a mismatch annotation through
/// [`crate::store::adapter_semantics_validation::insert_annotation`] when the
/// verdict is
/// [`crate::domain::authoritative_comparison::AuthoritativeComparisonVerdict::UnresolvedMismatch`].
///
/// Refuses a window that already carries a comparison for this observation,
/// naming the existing one: a comparison is corrected by recording another
/// one, never by writing a second record over the first.
pub fn record_adapter_semantics_comparison(
    conn: &rusqlite::Connection,
    request: &AdapterSemanticsComparisonRequest,
) -> Result<AdapterSemanticsComparisonOutcome, Error> {
    use crate::domain::authoritative_comparison::{
        AuthoritativeComparisonVerdict, compare_against_authoritative_surface,
    };
    use crate::store::adapter_semantics_validation::{
        AnnotationKind, NewAdapterSemanticsAnnotation, NewAuthoritativeSurfaceComparison,
        comparisons_for_observation, insert_annotation, insert_comparison,
    };
    use crate::store::meter_evidence::windows_by_observation;

    let windows = windows_by_observation(conn, request.observation_id)?;
    let window = windows
        .iter()
        .find(|w| w.semantic_key == request.semantic_key)
        .ok_or_else(|| {
            Error::Usage(format!(
                "observation {} has no window named {:?}; see `aub compare uncompared {}`",
                request.observation_id.value(),
                request.semantic_key.as_str(),
                request.observation_id.value(),
            ))
        })?;

    let existing = comparisons_for_observation(conn, request.observation_id)?;
    if let Some(prior) = existing.iter().find(|c| c.window_id == window.row_id) {
        return Err(Error::Usage(format!(
            "observation {} window {:?} already carries comparison #{} (verdict {}); a wrong \
             comparison is corrected by recording another one, never by overwriting it",
            request.observation_id.value(),
            request.semantic_key.as_str(),
            prior.row_id.value(),
            prior.verdict.as_str(),
        )));
    }

    let verdict = compare_against_authoritative_surface(
        window.quota_used,
        request.surface_quota_used,
        request.documented_granularity,
    );

    let comparison_id = insert_comparison(
        conn,
        &NewAuthoritativeSurfaceComparison {
            observation_id: request.observation_id,
            window_id: window.row_id,
            semantic_key: request.semantic_key.clone(),
            authoritative_surface: request.authoritative_surface.clone(),
            documented_granularity: request.documented_granularity,
            adapter_quota_used: window.quota_used,
            authoritative_quota_used: request.surface_quota_used,
            read_at: request.read_at,
            verdict,
        },
    )?;

    let mismatch_annotation_id = if verdict == AuthoritativeComparisonVerdict::UnresolvedMismatch {
        let detail = request.detail.clone().unwrap_or_else(|| {
            format!(
                "recorded through `aub compare`: adapter read {} ppm, {} read {} ppm for window \
                 {:?} of observation {}",
                window.quota_used.as_ppm().get(),
                request.authoritative_surface,
                request.surface_quota_used.as_ppm().get(),
                request.semantic_key.as_str(),
                request.observation_id.value(),
            )
        });
        Some(insert_annotation(
            conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Mismatch,
                comparison_id,
                observation_id: request.observation_id,
                semantic_key: request.semantic_key.clone(),
                adapter_quota_used: window.quota_used,
                authoritative_quota_used: request.surface_quota_used,
                corrects: None,
                detail,
                created_at: request.read_at,
            },
        )?)
    } else {
        None
    };

    Ok(AdapterSemanticsComparisonOutcome {
        comparison_id,
        verdict,
        adapter_quota_used: window.quota_used,
        mismatch_annotation_id,
    })
}

/// `aub compare`: record an adapter-semantics comparison through the release
/// binary, or list the windows of an observation that still need one.
/// docs/adapter-semantics-validation.md is the procedure this command
/// automates the bookkeeping half of.
fn compare_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let subcommand = invocation.rest.first().map(String::as_str);
    match subcommand {
        Some("record") => compare_record_command(clock, invocation),
        Some("uncompared") => compare_uncompared_command(clock, invocation),
        other => Err(Error::Usage(format!(
            "compare requires a subcommand (record | uncompared), got {other:?}"
        ))),
    }
}

fn compare_next_arg(args: &mut std::slice::Iter<String>, flag: &str) -> Result<String, Error> {
    args.next()
        .cloned()
        .ok_or_else(|| Error::Usage(format!("{flag} requires a value")))
}

/// A percentage typed by the operator, as the surface displays it (`21`, `21.0`, `0.5`),
/// converted to the parts-per-million the domain stores. The record this feeds is immutable
/// and read by a human off a web page in whole points, so the command takes the number the
/// page shows rather than a six-digit ppm figure one misplaced zero away from a wrong
/// comparison that looks computed.
fn compare_parse_percent_arg(
    args: &mut std::slice::Iter<String>,
    flag: &str,
) -> Result<crate::domain::quota::QuotaFractionPpm, Error> {
    let raw = compare_next_arg(args, flag)?;
    let percent: f64 = raw
        .trim_end_matches('%')
        .parse()
        .map_err(|_| Error::Usage(format!("{flag} must be a percentage, got {raw:?}")))?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(Error::Usage(format!(
            "{flag} must be a percentage between 0 and 100, got {raw:?}"
        )));
    }
    let ppm = (percent * 10_000.0).round() as i32;
    crate::domain::quota::QuotaFractionPpm::new(ppm).ok_or_else(|| {
        Error::Usage(format!(
            "{flag} must be a percentage between 0 and 100, got {raw:?}"
        ))
    })
}

/// One whole percentage point: what the Anthropic Console displays, and therefore the
/// granularity `docs/adapter-semantics-validation.md` records for it. The default rather
/// than a required flag because the table has one row and the procedure names the number.
const DEFAULT_GRANULARITY_PPM: i32 = 10_000;

/// `aub compare record OBSERVATION_ID WINDOW --surface NAME --surface-percent N
/// [--granularity-percent N] [--read-at RFC3339] [--detail TEXT]`.
fn compare_record_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let rest = &invocation.rest;
    let usage = "compare record requires OBSERVATION_ID WINDOW --surface NAME --surface-percent \
                 N [--granularity-percent N] [--read-at RFC3339] [--detail TEXT]";
    let observation_arg = rest.get(1).ok_or_else(|| Error::Usage(usage.into()))?;
    let window_arg = rest.get(2).ok_or_else(|| Error::Usage(usage.into()))?;
    let observation_id_value: i64 = observation_arg.parse().map_err(|_| {
        Error::Usage(format!(
            "compare record: OBSERVATION_ID must be an integer, got {observation_arg:?}"
        ))
    })?;
    let observation_id = crate::store::meter_evidence::ObservationRowId::new(observation_id_value);
    let semantic_key = crate::domain::window::WindowSemanticKey::new(window_arg.clone());

    let mut surface: Option<String> = None;
    let mut surface_used_ppm: Option<crate::domain::quota::QuotaFractionPpm> = None;
    let mut granularity_ppm: Option<crate::domain::quota::QuotaFractionPpm> = None;
    let mut read_at: Option<UtcTimestamp> = None;
    let mut detail: Option<String> = None;

    let mut args = rest[3..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--surface" => surface = Some(compare_next_arg(&mut args, "--surface")?),
            "--surface-percent" => {
                surface_used_ppm = Some(compare_parse_percent_arg(&mut args, "--surface-percent")?)
            }
            "--granularity-percent" => {
                granularity_ppm = Some(compare_parse_percent_arg(
                    &mut args,
                    "--granularity-percent",
                )?)
            }
            "--read-at" => {
                let raw = compare_next_arg(&mut args, "--read-at")?;
                read_at = Some(UtcTimestamp::parse_rfc3339(&raw).ok_or_else(|| {
                    Error::Usage(format!("--read-at must be RFC3339, got {raw:?}"))
                })?);
            }
            "--detail" => detail = Some(compare_next_arg(&mut args, "--detail")?),
            other => {
                return Err(Error::Usage(format!(
                    "compare record: unknown argument {other}"
                )));
            }
        }
    }

    let surface =
        surface.ok_or_else(|| Error::Usage("compare record requires --surface NAME".into()))?;
    let surface_used_ppm = surface_used_ppm
        .ok_or_else(|| Error::Usage("compare record requires --surface-percent N".into()))?;
    let granularity_ppm = granularity_ppm.unwrap_or_else(|| {
        crate::domain::quota::QuotaFractionPpm::new(DEFAULT_GRANULARITY_PPM)
            .expect("one whole percentage point is a valid non-zero granularity")
    });
    let read_at = read_at.unwrap_or_else(|| clock.now());

    let request = AdapterSemanticsComparisonRequest {
        observation_id,
        semantic_key: semantic_key.clone(),
        authoritative_surface: surface,
        surface_quota_used: crate::domain::quota::QuotaUsed::new(surface_used_ppm),
        documented_granularity: crate::domain::authoritative_comparison::DocumentedGranularity::new(
            granularity_ppm,
        ),
        read_at,
        detail,
    };

    let conn = open_ledger(clock)?;
    let outcome = record_adapter_semantics_comparison(&conn, &request)?;

    println!(
        "compare: recorded observation={} window={} comparison=#{} adapter_used_ppm={} \
         surface_used_ppm={} verdict={}",
        observation_id.value(),
        semantic_key.as_str(),
        outcome.comparison_id.value(),
        outcome.adapter_quota_used.as_ppm().get(),
        surface_used_ppm.get(),
        outcome.verdict.as_str(),
    );
    if let Some(annotation_id) = outcome.mismatch_annotation_id {
        println!(
            "compare: unresolved mismatch opened as finding annotation=#{}",
            annotation_id.value()
        );
    }
    Ok(())
}

/// `aub compare uncompared OBSERVATION_ID`: the windows of one observation
/// `store::adapter_semantics_validation::uncompared_window_ids` says still
/// lack a comparison, named by their semantic key rather than left as bare
/// row ids the operator cannot act on.
fn compare_uncompared_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let rest = &invocation.rest;
    let observation_arg = rest
        .get(1)
        .ok_or_else(|| Error::Usage("compare uncompared requires OBSERVATION_ID".into()))?;
    if let Some(extra) = rest.get(2) {
        return Err(Error::Usage(format!("unknown argument: {extra}")));
    }
    let observation_id_value: i64 = observation_arg.parse().map_err(|_| {
        Error::Usage(format!(
            "compare uncompared: OBSERVATION_ID must be an integer, got {observation_arg:?}"
        ))
    })?;
    let observation_id = crate::store::meter_evidence::ObservationRowId::new(observation_id_value);

    let conn = open_ledger(clock)?;
    let windows = crate::store::meter_evidence::windows_by_observation(&conn, observation_id)?;
    let uncompared_ids: std::collections::HashSet<_> =
        crate::store::adapter_semantics_validation::uncompared_window_ids(&conn, observation_id)?
            .into_iter()
            .collect();
    let uncompared: Vec<_> = windows
        .iter()
        .filter(|w| uncompared_ids.contains(&w.row_id))
        .collect();
    if uncompared.is_empty() {
        println!(
            "compare: observation {} has no uncompared windows",
            observation_id.value()
        );
    } else {
        for window in uncompared {
            println!(
                "compare: observation={} uncompared window={}",
                observation_id.value(),
                window.semantic_key.as_str()
            );
        }
    }
    Ok(())
}

/// The `aub ingest transcripts` progress line's exact wording (`aub-va6s`),
/// factored out so its format is a golden test target independent of
/// actually running a pass long enough to trigger one.
fn format_ingest_progress_line(progress: &crate::ingest::IngestProgress) -> String {
    format!(
        "ingest transcripts: progress files={}/{} sessions={} events={} elapsed={}s rate={:.1}/s",
        progress.files_done,
        progress.files_total,
        progress.sessions_written,
        progress.events_written,
        progress.elapsed.as_nanos() / 1_000_000_000,
        progress.rate_events_per_sec,
    )
}

/// `aub ingest transcripts`: explicit transcript ingestion as an operation in
/// its own right (aub-lqe.11, PLAN.md 6, 17.2, 27, 34.16). The window flags of
/// spend do not exist here: ingestion is not windowed, it lands everything the
/// configured sources currently hold, and reports what it read and the
/// generation it advanced.
fn ingest_command(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {
    let options = ingest_flags(&invocation.rest)?;
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("ingest");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let mut conn = open_ledger(clock)?;
    // Each landed batch is announced while the pass is still running, with the
    // batch's index, size, writer-slot hold and generation: the stable
    // identifiers a contention log is correlated by (`aub-lqe.18`).
    let mut batch_sink = |batch: &crate::ingest::LandedBatch| {
        logger
            .emit(
                clock.now(),
                DiagnosticEvent::IngestBatchLanded,
                &[
                    ("batch", &Quantity::new(batch.index, "index")),
                    ("events", &Quantity::new(batch.events, "events")),
                    (
                        "writer_slot",
                        &Quantity::new(batch.writer_slot.as_nanos(), "ns"),
                    ),
                    (
                        "generation",
                        &Quantity::new(batch.generation.value(), "generation"),
                    ),
                ],
            )
            .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))
    };
    // Printed unconditionally to stderr, not gated by `--verbose` like the
    // structured diagnostics above: a run that holds the writer lock for
    // half an hour and prints nothing at the default log level reads as
    // hung, which is the failure `aub-va6s` exists to close.
    let mut progress_sink = |progress: &crate::ingest::IngestProgress| -> Result<(), Error> {
        eprintln!("{}", format_ingest_progress_line(progress));
        Ok(())
    };
    let report = crate::ingest::run(
        &mut conn,
        &config,
        &options,
        clock,
        &mut batch_sink,
        &mut progress_sink,
    )?;
    println!(
        "ingest transcripts: sources={} scanned={} parsed={} skipped={} unreadable={} quarantined={} generation={} batches={}",
        report.sources.join(","),
        report.files_scanned,
        report.files_parsed,
        report.files_skipped,
        report.unreadable_files.len(),
        report.quarantined,
        report.generation.value(),
        report.batches.len(),
    );
    let outcome = &report.outcome;
    println!(
        "  events: written={} already-ingested={} · occurrences: written={} already-ingested={} · components={} sessions={} replaced={}",
        outcome.events_written.value(),
        outcome.events_already_ingested.value(),
        outcome.occurrences_written.value(),
        outcome.occurrences_already_ingested.value(),
        outcome.components_written.value(),
        outcome.sessions_upserted.value(),
        outcome.rows_replaced.value(),
    );
    if report.unreadable_files.is_empty() {
        Ok(())
    } else {
        Err(Error::IngestIncomplete(format!(
            "{} file(s) could not be read; the landed batch excludes them",
            report.unreadable_files.len()
        )))
    }
}

/// The ingest command's own flags: the `transcripts` subcommand word (required,
/// the only rebuildable source class with parsers today), then `--source NAME`
/// and `--changed-only`. Everything else is a usage error naming the argument.
fn ingest_flags(rest: &[String]) -> Result<crate::ingest::IngestOptions, Error> {
    let mut args = rest.iter();
    match args.next() {
        Some(word) if word == "transcripts" => {}
        Some(other) => {
            return Err(Error::Usage(format!(
                "unknown ingest target: {other}; ingest reads transcripts"
            )));
        }
        None => {
            return Err(Error::Usage(
                "ingest requires a target: ingest transcripts".into(),
            ));
        }
    }
    let mut options = crate::ingest::IngestOptions::default();
    while let Some(arg) = args.next() {
        let value = if let Some(inline) = arg.strip_prefix("--source=") {
            Some(inline.to_string())
        } else if arg == "--source" {
            Some(
                args.next()
                    .cloned()
                    .ok_or_else(|| Error::Usage("--source requires a source name".into()))?,
            )
        } else {
            None
        };
        match value {
            Some(name) => {
                if options.source.is_some() {
                    return Err(Error::Usage("--source was given more than once".into()));
                }
                options.source = Some(name);
            }
            None if arg == "--changed-only" => options.changed_only = true,
            None => return Err(Error::Usage(format!("unknown argument: {arg}"))),
        }
    }
    Ok(options)
}

/// `aub rebuild <target>`: explicit destructive rebuild of rebuildable
/// materializations (aub-lqe.11, PLAN.md 6, 27, 34.16). The target resolves
/// through the shared taxonomy's rebuild groups, so the command cannot name a
/// class the taxonomy does not classify rebuildable, and the sweep it runs is
/// the one [`crate::store::retention::delete_rebuildable`] derives from the
/// taxonomy rather than a list declared here.
fn rebuild_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let target_name = invocation.rest.first().cloned().ok_or_else(|| {
        Error::Usage(format!(
            "rebuild requires a target: {}",
            crate::store::retention::RebuildGroup::ALL
                .iter()
                .map(|group| group.name())
                .collect::<Vec<_>>()
                .join(" | ")
        ))
    })?;
    if invocation.rest.len() > 1 {
        return Err(Error::Usage(format!(
            "unknown argument: {}",
            invocation.rest[1]
        )));
    }
    let group =
        crate::store::retention::RebuildGroup::from_name(&target_name).ok_or_else(|| {
            Error::Usage(format!(
                "unknown rebuild target {target_name}; rebuildable targets are: {}",
                crate::store::retention::RebuildGroup::ALL
                    .iter()
                    .map(|group| group.name())
                    .collect::<Vec<_>>()
                    .join(" | ")
            ))
        })?;
    let mut conn = open_ledger(clock)?;
    let report = crate::store::retention::delete_rebuildable(&mut conn, group)?;
    println!(
        "rebuild {}: deleted {} rows across {} tables",
        report.group.name(),
        report.total().value(),
        report.deleted.len(),
    );
    for (class, count) in &report.deleted {
        let table = class
            .table_name()
            .unwrap_or_else(|| unreachable!("a sweep class is a table class by construction"));
        println!("  {table}: {} rows", count.value());
    }
    Ok(())
}

/// `aub clear-diagnostics`: clears retained diagnostic bodies, scoped per provider
/// or in total. Never touches quarantine rows.
fn clear_diagnostics_command(
    clock: &impl Clock,
    level: Level,
    invocation: &Invocation,
) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("clear-diagnostics");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let mut provider: Option<String> = None;
    let mut all = false;
    let mut iter = invocation.rest.iter().peekable();
    while let Some(arg) = iter.next() {
        if let Some(val) = arg.strip_prefix("--provider=") {
            if provider.is_some() {
                return Err(Error::Usage("--provider specified more than once".into()));
            }
            if val.is_empty() {
                return Err(Error::Usage("--provider requires a non-empty name".into()));
            }
            provider = Some(val.to_string());
        } else if arg == "--provider" {
            if provider.is_some() {
                return Err(Error::Usage("--provider specified more than once".into()));
            }
            let next_val = iter
                .next()
                .ok_or_else(|| Error::Usage("--provider requires a provider name".into()))?;
            if next_val.is_empty() {
                return Err(Error::Usage("--provider requires a non-empty name".into()));
            }
            provider = Some(next_val.clone());
        } else if arg == "--all" {
            all = true;
        } else if !arg.starts_with("--") {
            if provider.is_some() {
                return Err(Error::Usage(format!(
                    "unexpected positional argument: {arg}"
                )));
            }
            provider = Some(arg.clone());
        } else {
            return Err(Error::Usage(format!("unknown argument: {arg}")));
        }
    }

    if all && provider.is_some() {
        return Err(Error::Usage(
            "--all cannot be combined with --provider".into(),
        ));
    }

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    let report =
        crate::store::retention::clear_retained_bodies(&config.state.dir, provider.as_deref())
            .map_err(|error| Error::Store(format!("clear diagnostics: {error}")))?;

    match invocation.format {
        OutputFormat::Text => {
            println!(
                "{}",
                crate::presentation::render_clear_diagnostics(&clear_diagnostics_report(&report))
            )
        }
        OutputFormat::Json => println!(
            "{}",
            crate::presentation::clear_diagnostics_json(
                &clear_diagnostics_report(&report),
                run,
                timestamp,
            )
        ),
    }
    Ok(())
}

/// `aub task`: `ingest`, `report TASK-ID`, and `overhead`. Owns task-claim
/// ingestion and segmentation, never issue management (`aub-eu7.4`,
/// PLAN.md 27); none of the three subcommands segments usage itself, they
/// only call [`crate::report::task`], which is the shared assembly
/// `aub spend --group-by task` also reads.
fn task_command(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {
    let subcommand = invocation.rest.first().map(String::as_str);
    match subcommand {
        Some("ingest") => task_ingest_command(clock, level, invocation),
        Some("report") => task_report_command(clock, invocation),
        Some("overhead") => task_overhead_command(clock, invocation),
        other => Err(Error::Usage(format!(
            "task requires a subcommand (ingest | report | overhead), got {other:?}"
        ))),
    }
}

/// Carries the store's tracker-ingest summary across the presentation boundary as
/// a report model, which is the only shape a renderer is allowed to see.
fn task_ingest_report(
    summary: &crate::store::task_event::IngestSummary,
) -> crate::report::TaskIngestReport {
    crate::report::TaskIngestReport {
        events_inserted: summary.events_inserted,
        events_already_present: summary.events_already_present,
        quarantines_inserted: summary.quarantines_inserted,
        quarantines_already_present: summary.quarantines_already_present,
    }
}

/// `aub task ingest`: runs the Beads tracker adapter and reports events
/// ingested, quarantined and unchanged. The tracker database is opened
/// read-only and is never written to: `aub` reads task-claim history, it
/// never manages issues.
fn task_ingest_command(
    clock: &impl Clock,
    level: Level,
    invocation: &Invocation,
) -> Result<(), Error> {
    if invocation.rest.len() > 1 {
        return Err(Error::Usage(format!(
            "unknown argument: {}",
            invocation.rest[1]
        )));
    }
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("task");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run.clone());
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;

    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let tracker = config.tracker.as_ref().ok_or_else(|| {
        Error::Usage("no [tracker] source is configured; task ingest has nothing to read".into())
    })?;
    let tracker_conn =
        crate::store::task_event::open_tracker_database(&tracker.path.join("beads.db"))?;
    let reader = crate::store::task_event::BeadsEventReader::new(&tracker_conn);
    let ledger_conn = open_ledger(clock)?;
    let summary = crate::store::task_event::ingest(
        &ledger_conn,
        crate::domain::ids::SourceNamespace::new("beads"),
        &reader,
    )?;

    match invocation.format {
        OutputFormat::Text => println!(
            "task ingest: events_inserted={} events_already_present={} quarantines_inserted={} quarantines_already_present={}",
            summary.events_inserted,
            summary.events_already_present,
            summary.quarantines_inserted,
            summary.quarantines_already_present,
        ),
        OutputFormat::Json => {
            let metadata = ReportMetadata::new(
                timestamp,
                timestamp,
                LedgerGeneration::new(
                    crate::store::ledger_generation::current(&ledger_conn)?.value(),
                ),
                None,
            );
            println!(
                "{}",
                crate::presentation::json::task_ingest_json(
                    &task_ingest_report(&summary),
                    run,
                    metadata,
                )
            );
        }
    }
    Ok(())
}

/// `aub task report TASK-ID`: the task's total usage, resolved task-kind
/// identity, subscription credits where a complete cost model exists, and
/// the sessions that contributed to it.
fn task_report_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let task_id_arg = invocation
        .rest
        .get(1)
        .ok_or_else(|| Error::Usage("task report requires a TASK-ID".into()))?;
    if invocation.rest.len() > 2 {
        return Err(Error::Usage(format!(
            "unknown argument: {}",
            invocation.rest[2]
        )));
    }
    let task_id = parse_task_id(task_id_arg)?;
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let conn = open_ledger(clock)?;
    let report = crate::report::task::assemble_task_report(&conn, &task_id, timestamp)?;
    match invocation.format {
        OutputFormat::Text => println!(
            "{}",
            crate::presentation::render::render_task_report_with_explain(
                &report,
                invocation.explain
            )
        ),
        OutputFormat::Json => println!(
            "{}",
            crate::presentation::json::task_report_json_with_explain(
                &report,
                run,
                invocation.explain
            )
        ),
    }
    Ok(())
}

/// Parses a `TASK-ID` positional argument in `SOURCE:NATIVE` form, matching
/// the namespaced identifier every task-attribution table keys on.
fn parse_task_id(value: &str) -> Result<crate::domain::ids::TaskId, Error> {
    let (source, native) = value
        .split_once(':')
        .ok_or_else(|| Error::Usage(format!("TASK-ID must be SOURCE:NATIVE, got {value}")))?;
    if source.is_empty() || native.is_empty() {
        return Err(Error::Usage(format!(
            "TASK-ID must be SOURCE:NATIVE, got {value}"
        )));
    }
    Ok(crate::domain::ids::TaskId::new(
        crate::domain::ids::SourceNamespace::new(source),
        crate::domain::ids::NativeTaskId::new(native),
    ))
}

/// `aub task overhead --since`: every overhead bucket usage landed in over
/// the window, alongside the total task-attributed usage in the same window.
fn task_overhead_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let window = task_overhead_window(&invocation.rest[1..], timestamp)?;
    let conn = open_ledger(clock)?;
    let report = crate::report::task::assemble_task_overhead(&conn, window, timestamp)?;
    match invocation.format {
        OutputFormat::Text => println!(
            "{}",
            crate::presentation::render::render_task_overhead_report_with_explain(
                &report,
                invocation.explain
            )
        ),
        OutputFormat::Json => println!(
            "{}",
            crate::presentation::json::task_overhead_json_with_explain(
                &report,
                run,
                invocation.explain
            )
        ),
    }
    Ok(())
}

/// The window from `--today`, `--since YYYY-MM-DD` and `--days N`, the same
/// convention `aub spend` uses (see [`spend_options`]).
fn task_overhead_window(rest: &[String], now: UtcTimestamp) -> Result<SpendWindow, Error> {
    let mut since: Option<UtcDate> = None;
    let mut days: i64 = 1;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--today" => since = Some(now.utc_date()),
            "--since" => {
                let val = args
                    .next()
                    .ok_or_else(|| Error::Usage("--since requires YYYY-MM-DD".into()))?;
                since = Some(parse_date(val)?);
            }
            "--days" => {
                let val = args
                    .next()
                    .ok_or_else(|| Error::Usage("--days requires a number".into()))?;
                days = val
                    .parse()
                    .map_err(|_| Error::Usage(format!("--days must be a number, got {val}")))?;
            }
            other => match other.strip_prefix("--since=") {
                Some(val) => since = Some(parse_date(val)?),
                None => match other.strip_prefix("--days=") {
                    Some(val) => {
                        days = val.parse().map_err(|_| {
                            Error::Usage(format!("--days must be a number, got {val}"))
                        })?
                    }
                    None => return Err(Error::Usage(format!("unknown argument: {other}"))),
                },
            },
        }
    }
    SpendWindow::starting(since.unwrap_or_else(|| now.utc_date()), days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FakeEnv;

    /// `aub sample` opens its ledger with the configured `sampling.request_timeout`,
    /// not a hardcoded value (`aub-va6s`): a config naming a busy timeout
    /// wider than the old hardcoded 500ms must actually reach the pragma
    /// policy, which a hardcoded value, however large, could never do.
    #[test]
    fn sample_busy_policy_reads_the_configured_busy_timeout_not_the_request_timeout() {
        // request_timeout is set too, to a different value: the policy must follow the
        // lock-wait key and not the provider-request one it was first wired to.
        let toml = "[sampling]\nrequest_timeout = \"3s\"\nbusy_timeout = \"17s\"\n";
        let (config, _) = crate::config::resolve(
            &crate::config::Overrides::new(),
            &FakeEnv::new(),
            Some(toml),
            "/virtual/aub.toml",
        )
        .expect("config resolves");
        assert_eq!(
            sample_busy_policy(&config).busy_timeout,
            crate::domain::time::MonotonicDuration::from_seconds(17),
            "the sampler's busy timeout must track the configured value, not a fixed constant"
        );
    }

    /// The waited duration is named on the exact refusal `aub sample`
    /// reproduces (`aub-va6s`); every other store failure passes through with
    /// its message untouched, so a caller cannot mistake an unrelated store
    /// error for a busy-database refusal that happened to sit near one.
    #[test]
    fn name_busy_wait_names_the_duration_on_a_locked_database_refusal_and_leaves_others_alone() {
        let busy_timeout = crate::domain::time::MonotonicDuration::from_millis(5_000);
        let locked = Error::Store(
            "cannot start sample run: database is locked (code 5, SQLITE_BUSY)".to_string(),
        );
        let named = name_busy_wait(locked, busy_timeout);
        assert!(
            matches!(named, Error::Store(ref m) if m.contains("waited up to 5000ms")),
            "{named:?}"
        );

        let unrelated_text = "cannot read pragma journal_mode: disk I/O error";
        let unchanged = name_busy_wait(Error::Store(unrelated_text.to_string()), busy_timeout);
        assert_eq!(unchanged.to_string(), unrelated_text);

        let other_class_text = "account 'work': authentication required";
        let unchanged_class = name_busy_wait(
            Error::AuthRequired(other_class_text.to_string()),
            busy_timeout,
        );
        assert_eq!(unchanged_class.to_string(), other_class_text);
    }

    /// The progress line's exact wording is the golden target (`aub-va6s`,
    /// `aub-mh1c`): files done of total, sessions and events landed so far,
    /// elapsed, and the rate over the interval since the previous line.
    #[test]
    fn golden_ingest_progress_line_format() {
        let progress = crate::ingest::IngestProgress {
            files_done: 100,
            files_total: 3830,
            sessions_written: 12,
            events_written: 4_567,
            elapsed: crate::domain::time::MonotonicDuration::from_seconds(37),
            rate_events_per_sec: 123.456,
        };
        assert_eq!(
            format_ingest_progress_line(&progress),
            "ingest transcripts: progress files=100/3830 sessions=12 events=4567 elapsed=37s rate=123.5/s"
        );
    }

    fn parse_percent(raw: &str) -> Result<u32, Error> {
        let args = [raw.to_string()];
        let mut iter = args.iter();
        compare_parse_percent_arg(&mut iter, "--surface-percent").map(|ppm| ppm.get())
    }

    // The record this feeds is immutable, so the parser is checked in both directions:
    // the shapes an operator copies off the Console page land on the exact ppm, and
    // anything outside a percentage is refused rather than rounded into a plausible row.
    #[test]
    fn compare_percent_flag_takes_what_the_surface_displays_and_refuses_the_rest() {
        assert_eq!(parse_percent("21").unwrap(), 210_000);
        assert_eq!(parse_percent("21.0").unwrap(), 210_000);
        assert_eq!(parse_percent("21%").unwrap(), 210_000);
        assert_eq!(parse_percent("0.5").unwrap(), 5_000);
        assert_eq!(parse_percent("0").unwrap(), 0);
        assert_eq!(parse_percent("100").unwrap(), 1_000_000);
        for bad in ["101", "-1", "abc", "250000", "nan", "inf"] {
            let err = parse_percent(bad).expect_err(bad);
            assert!(
                matches!(err, Error::Usage(ref m) if m.contains("--surface-percent")),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn doctor_maps_an_attribution_floor_breach_to_threshold_not_met() {
        use crate::attribution::account_segment::AccountEvidenceClass;
        use crate::attribution::quality::{
            AttributionObservation, AttributionQualityAssessment, AttributionQualityFloor,
        };
        use crate::domain::time::UtcTimestamp;
        use crate::domain::tokens::{
            CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
        };

        let tokens = |input: u64| {
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            )
        };
        let observations = vec![
            AttributionObservation {
                evidence_class: AccountEvidenceClass::ExplicitLauncherOrHook,
                usage: tokens(10),
                observed_at: Some(UtcTimestamp::from_unix_nanos(10)),
            },
            AttributionObservation {
                evidence_class: AccountEvidenceClass::Unattributed,
                usage: tokens(90),
                observed_at: Some(UtcTimestamp::from_unix_nanos(10)),
            },
        ];

        // With a floor of 0.9, the 10% attributed fraction breaches it.
        let breaching = AttributionQualityAssessment::assess(
            observations.clone(),
            UtcTimestamp::from_unix_nanos(0),
            AttributionQualityFloor::new(0.9),
        );
        match attribution_quality_breach_error(&breaching) {
            Some(Error::ThresholdNotMet(message)) => {
                assert!(
                    message.contains("input"),
                    "{message:?} must name the token kind"
                );
            }
            other => panic!("expected ThresholdNotMet, got {other:?}"),
        }

        // With no floor configured, the same corpus produces no error: the
        // metric is reported, not judged.
        let unjudged = AttributionQualityAssessment::assess(
            observations,
            UtcTimestamp::from_unix_nanos(0),
            None,
        );
        assert!(attribution_quality_breach_error(&unjudged).is_none());
    }

    /// `Command::ALL` must name every variant the enum declares. `DECLARED_VARIANTS`
    /// is derived from the enum's own declaration by [`aub_command_enum`], so the
    /// two can only disagree when a variant joined the enum without joining `ALL`;
    /// the failure names it. The equality assert after it pins identity and order
    /// together, so a shortened or reordered `ALL` is loud, not silent.
    #[test]
    fn all_lists_every_declared_variant() {
        let missing: Vec<_> = Command::DECLARED_VARIANTS
            .iter()
            .filter(|variant| !Command::ALL.contains(variant))
            .collect();
        assert!(
            missing.is_empty(),
            "variant(s) missing from Command::ALL: {missing:?}"
        );
        assert_eq!(
            Command::ALL.to_vec(),
            Command::DECLARED_VARIANTS.to_vec(),
            "Command::ALL must keep the enum's declaration order"
        );
    }

    /// Every command declares a shared-flag policy, and the policy is well-formed:
    /// verbosity is accepted everywhere, and a rejected `--format` states why rather
    /// than rejecting silently.
    #[test]
    fn every_command_declares_a_shared_flag_policy() {
        for command in Command::ALL {
            let policy = command.flag_policy();
            assert_eq!(
                policy.verbosity,
                FlagSupport::Accepted,
                "{command:?} must accept verbosity"
            );
            for (flag, support) in [
                ("--format", policy.format),
                ("--explain", policy.explain),
                ("--account", policy.account),
                ("--model", policy.model),
                ("--no-color", policy.no_color),
            ] {
                if let FlagSupport::Rejected { reason } = support {
                    assert!(
                        !reason.is_empty(),
                        "{command:?} rejects {flag} without a reason"
                    );
                }
            }
        }
    }

    /// Every command declares whether it accepts or rejects `--explain`, and a
    /// rejection states why rather than rejecting silently. The acceptance arm
    /// pins the parser contract that the provenance work will enable.
    #[test]
    fn every_command_declares_an_explain_policy() {
        for command in Command::ALL {
            match command.flag_policy().explain {
                FlagSupport::Rejected { reason } => assert!(
                    !reason.is_empty(),
                    "{command:?} rejects --explain without a reason"
                ),
                FlagSupport::Accepted => {}
            }
        }
    }

    /// `--explain` is a parsed token for every command whose policy accepts it, and
    /// the parser honours the policy: a rejection emits the policy's reason, an
    /// acceptance lands as `ExplainMode::Summary` (or `Full` for `--explain=full`).
    /// A planted negative is a policy row that says `Accepted` for a command
    /// whose parser path ignores the flag, which this loop reports by name.
    #[test]
    fn the_parser_honours_the_explain_policy_for_every_command() {
        for command in Command::ALL {
            let result = parse_invocation(args(&[command.name(), "--explain"]));
            match command.flag_policy().explain {
                FlagSupport::Accepted => match result {
                    Ok(Request::Run(invocation)) => {
                        assert_eq!(
                            invocation.explain,
                            ExplainMode::Summary,
                            "{command:?} parsed --explain as something other than Summary"
                        );
                    }
                    other => panic!("{command:?} accepts --explain but parsed as {other:?}"),
                },
                FlagSupport::Rejected { reason } => match result {
                    Err(Error::Usage(message)) => {
                        assert!(message.contains(reason), "{command:?}: {message}")
                    }
                    other => panic!("{command:?} rejects --explain but parsed as {other:?}"),
                },
            }
        }
    }

    /// `--explain=full` and `--explain=garbage` are both refused for a command
    /// whose policy rejects the flag, with the policy's reason: the rejection
    /// takes precedence over value validation, so a rejecting command never
    /// accepts a value it cannot render.
    #[test]
    fn explain_values_are_refused_where_the_policy_rejects_the_flag() {
        for value in ["--explain=full", "--explain=garbage"] {
            let result = parse_invocation(args(&["config", value]));
            match result {
                Err(Error::Usage(message)) => {
                    assert!(
                        message.contains("does not accept --explain"),
                        "{value} must be refused with the policy's reason: {message}"
                    );
                    assert!(
                        message.contains("omit the flag"),
                        "{value} must name the next action: {message}"
                    );
                }
                other => panic!("{value} must be refused, got: {other:?}"),
            }
        }
    }

    /// For commands whose policy accepts `--explain`, `--explain` and `--explain=summary`
    /// yield `ExplainMode::Summary`, `--explain=full` yields `ExplainMode::Full`,
    /// and invalid values are rejected with actionable guidance.
    #[test]
    fn explain_values_are_validated_where_the_policy_accepts_the_flag() {
        for cmd in ["status", "spend"] {
            for val in ["--explain", "--explain=summary"] {
                let parsed = parse_invocation(args(&[cmd, val])).expect("valid explain summary");
                let Request::Run(inv) = parsed else {
                    panic!("expected Request::Run, got {parsed:?}")
                };
                assert_eq!(inv.explain, ExplainMode::Summary);
            }
            let parsed_full =
                parse_invocation(args(&[cmd, "--explain=full"])).expect("valid explain full");
            let Request::Run(inv) = parsed_full else {
                panic!("expected Request::Run, got {parsed_full:?}")
            };
            assert_eq!(inv.explain, ExplainMode::Full);
            let err = parse_invocation(args(&[cmd, "--explain=invalid"]))
                .expect_err("invalid explain value must error");
            let Error::Usage(msg) = err else {
                panic!("expected Error::Usage, got {err:?}")
            };
            assert!(msg.contains("is not one of summary or full"), "{msg}");
            assert!(msg.contains("use --explain or --explain=full"), "{msg}");
        }
    }

    /// `--account` is a parsed token for every command, and the parser honours the
    /// policy: a rejection emits the policy's reason, an acceptance lands as the
    /// invocation's account. Status is the one command that accepts it, and the
    /// selector is why.
    #[test]
    fn the_parser_honours_the_account_policy_for_every_command() {
        for command in Command::ALL {
            let result = parse_invocation(args(&[command.name(), "--account", "work-a"]));
            match command.flag_policy().account {
                FlagSupport::Accepted => match result {
                    Ok(Request::Run(invocation)) => {
                        assert_eq!(
                            invocation.account.as_deref(),
                            Some("work-a"),
                            "{command:?} parsed --account as something other than the value"
                        );
                    }
                    other => panic!("{command:?} accepts --account but parsed as {other:?}"),
                },
                FlagSupport::Rejected { reason } => match result {
                    Err(Error::Usage(message)) => {
                        assert!(message.contains(reason), "{command:?}: {message}")
                    }
                    other => panic!("{command:?} rejects --account but parsed as {other:?}"),
                },
            }
        }
    }

    #[test]
    fn coverage_selectors_accept_both_since_spellings_and_compose_with_account() {
        let parsed = parse_invocation(args(&[
            "coverage",
            "--account",
            "research",
            "--severe",
            "--since=30d",
        ]))
        .expect("coverage selectors must parse");
        let Request::Run(invocation) = parsed else {
            panic!("coverage must produce an invocation")
        };
        assert_eq!(invocation.account.as_deref(), Some("research"));
        let window = coverage_window(&invocation.rest).expect("selectors must be valid");
        assert_eq!(window.since, MonotonicDuration::from_seconds(30 * 86_400));
        assert_eq!(window.description, "30d");
        assert!(window.severe_only);

        let spaced = coverage_window(&["--since".to_string(), "2h".to_string()])
            .expect("the spaced since form must parse");
        assert_eq!(spaced.since, MonotonicDuration::from_seconds(2 * 3_600));
        assert_eq!(spaced.description, "2h");

        match coverage_window(&["--since=forever".to_string()]) {
            Err(Error::Usage(_)) => {}
            Err(error) => panic!("an invalid coverage interval must be a usage error: {error:?}"),
            Ok(_) => panic!("an invalid coverage interval must be refused"),
        }
    }

    /// `--model` is a parsed token for every command, both the `--model M` and
    /// `--model=M` spellings, and the parser honours the policy: status is the
    /// one command that accepts it, and the window selection is why.
    #[test]
    fn the_parser_honours_the_model_policy_for_every_command() {
        for spelling in [
            vec!["--model", "claude-model-x"],
            vec!["--model=claude-model-x"],
        ] {
            let mut argv = vec![Command::Status.name()];
            argv.extend(spelling);
            let result = parse_invocation(args(&argv));
            match result {
                Ok(Request::Run(invocation)) => assert_eq!(
                    invocation.model.as_deref(),
                    Some("claude-model-x"),
                    "status must parse --model as the value: {invocation:?}"
                ),
                other => panic!("status accepts --model but parsed as {other:?}"),
            }
        }

        for command in Command::ALL {
            if command == Command::Status {
                continue;
            }
            let result = parse_invocation(args(&[command.name(), "--model", "m"]));
            match command.flag_policy().model {
                FlagSupport::Rejected { reason } => match result {
                    Err(Error::Usage(message)) => {
                        assert!(message.contains(reason), "{command:?}: {message}")
                    }
                    other => panic!("{command:?} rejects --model but parsed as {other:?}"),
                },
                FlagSupport::Accepted => {
                    panic!("{command:?} declares --model accepted but status is the only selector")
                }
            }
        }
    }

    /// `--no-color` is a parsed token for every command, and the parser honours
    /// the policy: a rejection emits the policy's reason, an acceptance lands as
    /// the invocation's no_color. No command accepts it yet, so the rejection
    /// arm is the one that fires.
    #[test]
    fn the_parser_honours_the_no_color_policy_for_every_command() {
        for command in Command::ALL {
            let result = parse_invocation(args(&[command.name(), "--no-color"]));
            match command.flag_policy().no_color {
                FlagSupport::Accepted => match result {
                    Ok(Request::Run(invocation)) => {
                        assert!(
                            invocation.no_color,
                            "{command:?} parsed --no-color as false"
                        );
                    }
                    other => panic!("{command:?} accepts --no-color but parsed as {other:?}"),
                },
                FlagSupport::Rejected { reason } => match result {
                    Err(Error::Usage(message)) => {
                        assert!(message.contains(reason), "{command:?}: {message}")
                    }
                    other => panic!("{command:?} rejects --no-color but parsed as {other:?}"),
                },
            }
        }
    }

    /// Help states, for every shipping command, the question it answers, the
    /// shared flags it refuses and why, and the formats it accepts - the refusal
    /// lines derived from the same policy the parser enforces.
    #[test]
    fn help_states_question_refusal_and_format_for_every_command() {
        let help = help_text();
        for command in Command::ALL {
            let Some(_summary) = command.summary() else {
                assert!(
                    command.question().is_none(),
                    "{command:?} is hidden but exposes a help question"
                );
                continue;
            };
            let question = command
                .question()
                .unwrap_or_else(|| panic!("{command:?} has no help question"));
            assert!(
                help.contains(question),
                "{command:?} help must state its question"
            );
            let refused_flags = command.refused_flags();
            assert!(
                !refused_flags.is_empty(),
                "{command:?} help has no refusal boundary"
            );
            for refused in refused_flags {
                assert!(
                    help.contains(&refused),
                    "{command:?} help must state the refusal: {refused}"
                );
            }
            assert!(
                help.contains(&command.format_help()),
                "{command:?} help must state its format support"
            );
        }
    }

    /// `docs/commands.md` names every shipping command in a `## \`aub NAME\``
    /// heading, each with a `**Refuses:**` line stating the behavioural
    /// boundary `--help` does not carry (aub-n27.6). The documented set is
    /// compared against [`Command::ALL`] filtered to the shipping subset
    /// (`summary().is_some()`) rather than a hand-maintained list, so a
    /// command added without a section fails here instead of only being
    /// noticed by a human reading the file. The planted negative: a
    /// documented command with no `**Refuses:**` line would still pass a
    /// weaker check that only compared the name set.
    #[test]
    fn documented_command_list_matches_the_parser_and_states_a_refusal() {
        let docs =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/commands.md"))
                .expect("docs/commands.md must be readable");

        let shipping: std::collections::BTreeSet<&str> = Command::ALL
            .into_iter()
            .filter(|command| command.summary().is_some())
            .map(Command::name)
            .collect();

        let mut documented: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for line in docs.lines() {
            let Some(rest) = line.strip_prefix("## `aub ") else {
                continue;
            };
            let name = rest
                .strip_suffix('`')
                .unwrap_or_else(|| panic!("malformed command heading: {line:?}"));
            documented.insert(name);
        }

        assert_eq!(
            documented, shipping,
            "docs/commands.md must document exactly the shipping commands"
        );

        for name in &documented {
            let heading = format!("## `aub {name}`");
            let start = docs
                .find(&heading)
                .unwrap_or_else(|| panic!("lost {heading:?} on the second pass"));
            let section_end = docs[start..]
                .find("\n## ")
                .map(|offset| start + offset)
                .unwrap_or(docs.len());
            let section = &docs[start..section_end];
            assert!(
                section.contains("**Refuses:**"),
                "docs/commands.md section for {name:?} has no **Refuses:** line"
            );
        }
    }

    fn args(items: &[&str]) -> Vec<OsString> {
        std::iter::once("aub")
            .chain(items.iter().copied())
            .map(OsString::from)
            .collect()
    }

    /// The parser honours the flag policy for every command, checked against the
    /// parser rather than the table: `--format json` is accepted exactly where the
    /// policy says `Accepted` and refused with the policy's reason elsewhere. The
    /// planted negative is a policy row that says `Accepted` for a command whose
    /// parser path ignores the flag, which this loop would report by name.
    #[test]
    fn the_parser_honours_the_format_policy_for_every_command() {
        for command in Command::ALL {
            let result = parse_invocation(args(&[command.name(), "--format", "json"]));
            match command.flag_policy().format {
                FlagSupport::Accepted => match result {
                    Ok(Request::Run(invocation)) => {
                        assert_eq!(invocation.format, OutputFormat::Json, "{command:?}");
                        assert!(
                            invocation.rest.is_empty(),
                            "{command:?} left the flag unread"
                        );
                    }
                    other => panic!("{command:?} accepts --format but parsed as {other:?}"),
                },
                FlagSupport::Rejected { reason } => match result {
                    Err(Error::Usage(message)) => {
                        assert!(message.contains(reason), "{command:?}: {message}")
                    }
                    other => panic!("{command:?} rejects --format but parsed as {other:?}"),
                },
            }
        }
    }

    /// Verbosity has one spelling and one meaning on every command. The parser
    /// accepts `-v` before or after the command and each occurrence raises the
    /// same counter by one.
    #[test]
    fn verbosity_has_uniform_spelling_and_semantics() {
        for command in Command::ALL {
            assert_eq!(command.flag_policy().verbosity, FlagSupport::Accepted);
            let before = parse_invocation(args(&["-v", command.name()])).unwrap();
            let after = parse_invocation(args(&[command.name(), "-v", "-v"])).unwrap();
            match (before, after) {
                (Request::Run(before), Request::Run(after)) => {
                    assert_eq!(before.verbosity, 1, "{command:?}");
                    assert_eq!(after.verbosity, 2, "{command:?}");
                }
                other => panic!("{command:?} did not parse verbosity uniformly: {other:?}"),
            }
        }
    }

    #[test]
    fn help_and_version_are_recognised_and_unknown_arguments_are_named() {
        assert_eq!(parse_invocation(args(&["--help"])).unwrap(), Request::Help);
        assert_eq!(parse_invocation(args(&["-h"])).unwrap(), Request::Help);
        assert_eq!(
            parse_invocation(args(&["--version"])).unwrap(),
            Request::Version
        );
        assert_eq!(parse_invocation(args(&[])).unwrap(), Request::Version);
        match parse_invocation(args(&["--definitely-not-a-flag"])) {
            Err(Error::Usage(message)) => assert!(message.contains("--definitely-not-a-flag")),
            other => panic!("{other:?}"),
        }
        let help = help_text();
        for command in Command::ALL {
            match command.summary() {
                Some(_) => assert!(
                    help.contains(command.name()),
                    "{command:?} missing from help"
                ),
                None => assert!(!help.contains(command.name()), "{command:?} is a hook"),
            }
        }
    }

    /// The spend window: today by default, `--since` with `--days`, and a malformed
    /// date refused rather than guessed.
    #[test]
    fn spend_options_default_to_the_utc_day_and_read_grouping_and_refresh_flags() {
        let now = crate::domain::time::UtcTimestamp::parse_rfc3339("2026-08-30T23:30:00Z").unwrap();
        let today = spend_options(&[], now).unwrap();
        assert_eq!(today.window.since.iso(), "2026-08-30");
        assert_eq!(today.window.until.iso(), "2026-08-31");
        assert_eq!(today.grouping, vec![SpendGrouping::Day]);
        assert_eq!(today.refresh, RefreshPolicy::Auto);
        let explicit = spend_options(
            &[
                "--since".into(),
                "2026-08-25".into(),
                "--days".into(),
                "3".into(),
                "--group-by=session".into(),
                "--group-by".into(),
                "repository".into(),
                "--refresh=never".into(),
            ],
            now,
        )
        .unwrap();
        assert_eq!(explicit.window.since.iso(), "2026-08-25");
        assert_eq!(explicit.window.until.iso(), "2026-08-28");
        assert_eq!(
            explicit.grouping,
            vec![SpendGrouping::Session, SpendGrouping::Repository]
        );
        assert_eq!(explicit.refresh, RefreshPolicy::Never);
        assert!(spend_options(&["--since".into(), "25/08/2026".into()], now).is_err());
        assert!(spend_options(&["--days".into(), "0".into()], now).is_err());
        assert_eq!(
            spend_options(&["--group-by=account".into()], now)
                .unwrap()
                .grouping,
            vec![SpendGrouping::Account]
        );
        assert_eq!(
            spend_options(&["--group-by=task".into()], now)
                .unwrap()
                .grouping,
            vec![SpendGrouping::Task]
        );
        assert!(spend_options(&["--group-by=model".into()], now).is_err());
        assert!(spend_options(&["--bogus".into()], now).is_err());
    }

    #[test]
    fn apply_set_parses_a_well_formed_key_value_pair() {
        let overrides = apply_set(crate::config::Overrides::new(), "state.dir=/x").unwrap();
        // Overrides has no public getter (it is consumed directly by resolve()); the
        // round trip through resolve is what config's own tests exercise. This test
        // only proves apply_set itself does not reject a well-formed pair.
        let _ = overrides;
    }

    #[test]
    fn apply_set_rejects_a_pair_with_no_equals_sign() {
        assert!(apply_set(crate::config::Overrides::new(), "state.dir").is_err());
    }

    /// The export flags: both `--key` spellings, the logical-identifier flag
    /// defaulting off, and the near-identical negatives each naming what they
    /// refused: a missing key, an unknown key value, a repeated key, and an
    /// argument the command does not own.
    #[test]
    fn export_flags_read_the_key_and_the_logical_id_flag() {
        let (key, ids) = export_flags(&["--key".into(), "session-id".into()]).unwrap();
        assert_eq!(key, ExportKey::Session);
        assert!(!ids, "logical ids are opt-in, never default");

        let (key, ids) =
            export_flags(&["--key=run-id".into(), "--include-logical-ids".into()]).unwrap();
        assert_eq!(key, ExportKey::Run);
        assert!(ids);

        for (rest, expected) in [
            (vec![], "--key session-id"),
            (vec!["--key".to_string(), "bogus".to_string()], "bogus"),
            (vec!["--key".to_string()], "--key requires"),
            (
                vec![
                    "--key".to_string(),
                    "run-id".to_string(),
                    "--key".to_string(),
                    "session-id".to_string(),
                ],
                "more than once",
            ),
            (
                vec![
                    "--key".to_string(),
                    "run-id".to_string(),
                    "--bogus".to_string(),
                ],
                "--bogus",
            ),
        ] {
            match export_flags(&rest) {
                Err(Error::Usage(message)) => {
                    assert!(
                        message.contains(expected),
                        "{message:?} must name {expected:?}"
                    )
                }
                other => panic!("expected a usage error naming {expected:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolve_config_file_path_prefers_the_flag_over_the_environment_and_default() {
        let env = FakeEnv::new()
            .set("HOME", "/tmp/home")
            .set("AUB_CONFIG_FILE", "/from/env.toml");
        assert_eq!(
            resolve_config_file_path(Some("/from/flag.toml"), &env),
            "/from/flag.toml"
        );
    }

    #[test]
    fn resolve_config_file_path_falls_back_to_the_environment_variable() {
        let env = FakeEnv::new()
            .set("HOME", "/tmp/home")
            .set("AUB_CONFIG_FILE", "/from/env.toml");
        assert_eq!(resolve_config_file_path(None, &env), "/from/env.toml");
    }

    #[test]
    fn resolve_config_file_path_falls_back_to_the_non_identifying_default() {
        let env = FakeEnv::new().set("HOME", "/tmp/synthetic-home");
        assert_eq!(
            resolve_config_file_path(None, &env),
            "/tmp/synthetic-home/.config/aub/config.toml"
        );
    }

    /// `aub config`'s output never names a credential's file path or profile
    /// reference: an account's provenance is reported as the one bucket key
    /// `accounts`, never key-by-key, so the account's own `credential_detail`
    /// (whatever kind of secret-adjacent reference it holds) has no key of its own to
    /// be printed under.
    #[test]
    fn config_provenance_never_exposes_a_credential_detail_as_its_own_key() {
        let file = r#"
[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "file", path = "/secret/path/to/credential.json" }
"#;
        let env = FakeEnv::new().set("HOME", "/tmp/synthetic-home");
        let (config, provenance) = crate::config::resolve(
            &crate::config::Overrides::new(),
            &env,
            Some(file),
            "/test/aub.toml",
        )
        .unwrap();

        // The credential detail is genuinely present on the resolved Config (a later
        // bead's adapter needs it) ...
        assert_eq!(
            config.accounts[0].credential_detail,
            "/secret/path/to/credential.json"
        );
        // ... but no provenance key printed by `aub config` names it or carries it:
        // the only key covering accounts is the one bucket key "accounts" itself.
        for (key, _source) in provenance.entries() {
            assert!(
                !key.contains("credential"),
                "a provenance key names credential material: {key}"
            );
        }
        assert_eq!(
            provenance.get("accounts"),
            Some(crate::config::ConfigSource::File)
        );
    }

    /// The status contract's shape (PLAN.md sections 16.2 and 43 workflow 4):
    /// the status function's own source performs exactly configuration
    /// resolution sufficient to locate the projection, one bounded file read,
    /// freshness computation and formatting. Nothing else is referenced from
    /// it, so a store, transcript, calibration, rate-card or write call that
    /// joined the status path would fail here before it could block a status
    /// bar on another aub operation.
    #[test]
    fn the_status_function_performs_only_the_status_contract() {
        let source = include_str!("cli.rs");
        // The status path is fn status and the helpers it alone uses, so the
        // scan covers the bodies that carry its work, not just its own text.
        let status_body = [
            function_body(source, "fn status("),
            function_body(source, "fn projection_accounts("),
            function_body(source, "fn status_clock_skew_envelope("),
        ]
        .concat();

        for forbidden in [
            "rusqlite",
            "Connection",
            "store::connection",
            "store::migrate",
            "transcripts::",
            "calibration",
            "rate_book",
            "ureq",
            "reqwest",
            "http",
            "spool",
            "fs::write",
            "OpenOptions",
            "create_dir",
            "remove_file",
        ] {
            assert!(
                !status_body.contains(forbidden),
                "the status function's source must not reference {forbidden}: the status contract allows only configuration resolution, one bounded projection read, freshness computation and formatting"
            );
        }
    }

    /// The negative that keeps the scan above a test rather than a ritual: a
    /// function body that does name the store fails the same scan.
    #[test]
    fn the_status_contract_scan_catches_a_forbidden_reference() {
        // The poisoned sample spells the violation in this scanner's
        // vocabulary but not in the literal the store-connection boundary
        // rule greps for, so the negative never trips that rule on this file.
        let poisoned = "fn status() { let probe = crate::store::connection::open(); }";
        assert!(function_body(poisoned, "fn status(").contains("store::connection"));
    }

    /// `now` and `sample` both turn a persistence-failing disposition into a
    /// store-class error and render nothing after it. The positive: a batch
    /// whose every disposition recorded returns `Ok`. The planted negative,
    /// near-identical, differs only in the one forbidden dimension: a
    /// `PersistFailed` disposition must become `Error::Store`, never `Ok`, so a
    /// fetched value is never rendered as though it had been recorded.
    #[test]
    fn a_persistence_failing_disposition_becomes_a_store_error() {
        use crate::domain::attempt::{AttemptId, AttemptOutcome};
        use crate::meter::sampler::{AccountDisposition, AccountReport};
        use crate::store::sampling_lease::AccountName;

        let recorded = AccountReport {
            name: AccountName::new("work-a"),
            disposition: AccountDisposition::NotYet {
                next_due_at: crate::domain::time::UtcTimestamp::from_unix_nanos(1),
            },
        };
        assert!(sampling_disposition_error(std::slice::from_ref(&recorded)).is_ok());

        let persist_failed = AccountReport {
            name: AccountName::new("work-a"),
            disposition: AccountDisposition::PersistFailed {
                attempt_id: AttemptId::new(7),
                outcome: AttemptOutcome::Success,
                reason: "disk full".to_string(),
            },
        };
        match sampling_disposition_error(&[recorded, persist_failed]) {
            Err(Error::Store(message)) => {
                assert!(message.contains("durably preserved"), "{message:?}");
                assert!(message.contains("disk full"), "{message:?}");
            }
            other => panic!("PersistFailed must map to Error::Store, got {other:?}"),
        }
    }

    #[test]
    fn clear_diagnostics_alias_clear_captures_is_recognised() {
        let req1 = parse_invocation(args(&["clear-diagnostics"])).unwrap();
        let req2 = parse_invocation(args(&["clear-captures"])).unwrap();
        match (req1, req2) {
            (Request::Run(inv1), Request::Run(inv2)) => {
                assert_eq!(inv1.command, Command::ClearDiagnostics);
                assert_eq!(inv2.command, Command::ClearDiagnostics);
            }
            other => panic!("expected Request::Run for both aliases, got {other:?}"),
        }
    }

    #[test]
    fn clear_diagnostics_accepts_format_and_provider_and_all() {
        let req = parse_invocation(args(&[
            "clear-diagnostics",
            "--format",
            "json",
            "--provider",
            "anthropic",
        ]))
        .unwrap();
        match req {
            Request::Run(inv) => {
                assert_eq!(inv.format, OutputFormat::Json);
                assert_eq!(
                    inv.rest,
                    vec!["--provider".to_string(), "anthropic".to_string()]
                );
            }
            other @ (Request::Version | Request::Help) => {
                panic!("unexpected parse result: {other:?}")
            }
        }
    }

    /// The body of one function in this file: from its declaration to the
    /// next top-level `fn`, or to the end of the file.
    #[test]
    fn sample_options_help_does_not_mention_all() {
        let options = Command::Sample
            .options_help()
            .expect("sample has options help");
        assert!(
            !options.contains("--all"),
            "sample options must not mention --all: {options}"
        );
        let clear_options = Command::ClearDiagnostics
            .options_help()
            .expect("clear-diagnostics has options help");
        assert!(
            clear_options.contains("--all"),
            "clear-diagnostics must keep --all: {clear_options}"
        );
    }

    #[test]
    fn sample_parser_refuses_all_naming_bare_form() {
        let inv = Invocation {
            command: Command::Sample,
            format: OutputFormat::Text,
            verbosity: 0,
            explain: ExplainMode::Off,
            account: None,
            model: None,
            no_color: false,
            rest: vec!["--all".to_string()],
        };
        let result = sample_command(&RealClock::new(), Level::DEFAULT, &inv);
        match result {
            Err(Error::Usage(msg)) => {
                assert!(
                    msg.contains("unknown argument: --all"),
                    "expected unknown argument: --all, got {msg}"
                );
                assert!(
                    msg.contains("run aub sample alone"),
                    "expected bare form naming, got {msg}"
                );
            }
            other => panic!("expected Error::Usage, got {other:?}"),
        }
    }

    #[test]
    fn sample_parser_refuses_all_with_account() {
        let inv = Invocation {
            command: Command::Sample,
            format: OutputFormat::Text,
            verbosity: 0,
            explain: ExplainMode::Off,
            account: Some("work-primary".to_string()),
            model: None,
            no_color: false,
            rest: vec!["--all".to_string()],
        };
        let result = sample_command(&RealClock::new(), Level::DEFAULT, &inv);
        match result {
            Err(Error::Usage(msg)) => {
                assert!(
                    msg.contains("unknown argument: --all"),
                    "expected unknown argument: --all, got {msg}"
                );
                assert!(
                    msg.contains("run aub sample alone"),
                    "expected bare form naming, got {msg}"
                );
            }
            other => panic!("expected Error::Usage, got {other:?}"),
        }
    }

    fn function_body(source: &str, declaration: &str) -> String {
        let start = source
            .find(declaration)
            .unwrap_or_else(|| panic!("cli.rs must declare {declaration}"));
        let rest = &source[start..];
        let end = rest[declaration.len()..]
            .find("\nfn ")
            .map(|offset| offset + declaration.len() + 1)
            .unwrap_or(rest.len());
        rest[..end].to_string()
    }
}
