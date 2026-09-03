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

use crate::domain::time::{
    Clock, ClockSkewEnvelope, MonotonicDuration, RealClock, UtcDate, UtcTimestamp,
};
use crate::error::Error;
use crate::logging::{DiagnosticEvent, DiagnosticLogger, Level, LogicalName, Quantity, RunId};
pub use crate::presentation::ExplainMode;
use crate::presentation::json::{coverage_json, spend_json_with_explain, status_json_with_explain};
use crate::presentation::render::{
    render_coverage_report, render_coverage_threshold_message, render_spend_report_with_explain,
    render_status_report_with_explain,
};
use crate::report::ReportEnvelope;
use crate::report::coverage::{CoverageFloors, CoverageSelector, assemble as assemble_coverage};
use crate::report::export::assemble as assemble_export;
use crate::report::spend::{SpendWindow, assemble_canonical as assemble_spend};
use crate::report::{LedgerGeneration, MeterAccount, ReportMetadata, SpendGrouping, StatusReport};
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
    RateCard,
    Backup,
    Ingest,
    Rebuild,
    Doctor,
    Coverage,
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
    pub const ALL: [Self; 15] = [
        Self::Status,
        Self::Spend,
        Self::Config,
        Self::Export,
        Self::LoggingFixture,
        Self::StateCheck,
        Self::ExitClass,
        Self::AttemptCrashHook,
        Self::ProjectionCrashHook,
        Self::RateCard,
        Self::Backup,
        Self::Ingest,
        Self::Rebuild,
        Self::Doctor,
        Self::Coverage,
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
                    reason: "spend has no account dimension until account attribution lands",
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
            Command::RateCard => "rate-card",
            Command::Backup => "backup",
            Command::Ingest => "ingest",
            Command::Rebuild => "rebuild",
            Command::Doctor => "doctor",
            Command::Coverage => "coverage",
        }
    }

    /// The one-line description `--help` prints. A test hook prints none: it is not
    /// part of the shipping surface and help does not list it.
    pub fn summary(self) -> Option<&'static str> {
        match self {
            Command::Status => Some("render the last known meter reading per configured account"),
            Command::Spend => {
                Some("canonical token usage grouped by day, session, project or repository")
            }
            Command::Config => {
                Some("print every resolved configuration key with the source that won")
            }
            Command::Export => Some(
                "emit versioned JSONL usage rows keyed by session-id or run-id for external joins",
            ),
            Command::LoggingFixture | Command::StateCheck | Command::ExitClass => None,
            Command::AttemptCrashHook => None,
            Command::ProjectionCrashHook => None,
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
            Command::Export => Some(
                "which usage did each session or run consume, as a versioned JSONL ledger for an external join?",
            ),
            Command::LoggingFixture | Command::StateCheck | Command::ExitClass => None,
            Command::AttemptCrashHook => None,
            Command::ProjectionCrashHook => None,
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
                "--today (default) | --since YYYY-MM-DD | --days N | --group-by day|session|project|repository (repeatable) | --refresh auto|never|force | --value api-list",
            ),
            Command::Config => Some("--set key=value (repeatable), --config-file PATH"),
            Command::Backup => Some("DESTINATION | verify DESTINATION"),
            Command::Ingest => Some("transcripts [--source NAME] [--changed-only]"),
            Command::Rebuild => Some("transcripts | attribution"),
            Command::Export => Some("--key session-id|run-id (required), --include-logical-ids"),
            Command::Doctor => Some("--transcript-format-drift"),
            Command::Coverage => {
                Some("--since DURATION (default 24h), --severe; --account is shared")
            }
            Command::Status
            | Command::LoggingFixture
            | Command::StateCheck
            | Command::ExitClass
            | Command::AttemptCrashHook
            | Command::ProjectionCrashHook
            | Command::RateCard => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
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
        Command::RateCard => rate_card_command(&RealClock::new(), &invocation),
        Command::Backup => backup_command(&RealClock::new(), &invocation),
        Command::Ingest => ingest_command(&RealClock::new(), level, &invocation),
        Command::Rebuild => rebuild_command(&RealClock::new(), &invocation),
        Command::Doctor => doctor_command(&RealClock::new(), level, &invocation),
        Command::Coverage => coverage_command(&RealClock::new(), level, &invocation),
    }
}

/// `aub doctor`: operational health, drift and integrity diagnostics.
/// Supports `--transcript-format-drift` (aub-lqe.17).
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

    for arg in &invocation.rest {
        match arg.as_str() {
            "--transcript-format-drift" | "--rate-card-staleness" => {}
            other => return Err(Error::Usage(format!("unknown argument: {other}"))),
        }
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

    let mut db_quarantine = None;
    let mut stale_cards = Vec::new();
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
        }
    }

    let report =
        crate::transcripts::detect_drift(&config, None, timestamp, db_quarantine.as_deref())?;

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

    Ok(())
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
        // The spend refresh lands batches without observing them; the batch sink is the
        // ingest command's diagnostic surface, not the spend command's.
        match crate::ingest::run(&mut conn, &config, &ingest_options, clock, &mut |_| Ok(())) {
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
    let mut report = assemble_spend(
        &conn,
        options.window,
        timestamp,
        options.grouping,
        options.refresh != RefreshPolicy::Never,
        refresh_failure.clone(),
        rate_book.as_ref(),
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
    })
}

fn parse_spend_grouping(value: &str) -> Result<SpendGrouping, Error> {
    match value {
        "day" => Ok(SpendGrouping::Day),
        "session" => Ok(SpendGrouping::Session),
        "project" => Ok(SpendGrouping::Project),
        "repository" | "repo" => Ok(SpendGrouping::Repository),
        _ => Err(Error::Usage(format!(
            "--group-by must be day, session, project or repository, got {value}"
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
                "__attempt-crash-hook requires a stage (before-start-commit | after-start-commit-before-request | after-parse-before-spool-write | after-spool-write-before-sqlite-commit | after-sqlite-commit-before-pending-deletion | complete | read-back | drain | freshness | sample | sample-crash), got {other:?}"
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

    // The sample stages run the meter evidence cycle against the one ledger
    // database, so they contend with a concurrent ingest the way the two real
    // workloads do; the lifecycle stages keep their own fixture database.
    let ledger = matches!(
        stage,
        crate::store::attempt_crash_hook::CrashHookStage::Sample { .. }
            | crate::store::attempt_crash_hook::CrashHookStage::SampleCrash
    );
    let database = if ledger {
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
/// and recomputes its verification result. The archive module owns the cut and
/// verification protocol, while this layer only resolves configuration and
/// renders the typed summary.
fn backup_command(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let (verify, destination) = match invocation.rest.as_slice() {
        [destination] => (false, destination),
        [subcommand, destination] if subcommand == "verify" => (true, destination),
        rest => {
            return Err(Error::Usage(format!(
                "backup requires DEST or `verify DEST`, got {rest:?}"
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
    let destination = std::path::Path::new(destination);
    let summary = if verify {
        crate::backup::verify_archive(destination, config.sampling.request_timeout, clock)?
    } else {
        crate::store::startup::run_after_state_check(
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
        )??
    };
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
    let report = crate::ingest::run(&mut conn, &config, &options, clock, &mut batch_sink)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FakeEnv;

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
        assert!(spend_options(&["--group-by=account".into()], now).is_err());
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

    /// The body of one function in this file: from its declaration to the
    /// next top-level `fn`, or to the end of the file.
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
