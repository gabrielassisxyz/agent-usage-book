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

use crate::domain::freshness::{FreshnessInput, compute_freshness};
use crate::domain::time::{Clock, ClockSkewEnvelope, MonotonicDuration, RealClock, UtcDate};
use crate::error::Error;
use crate::logging::{DiagnosticEvent, DiagnosticLogger, Level, LogicalName, RunId};
use crate::presentation::json::{spend_json, status_json};
use crate::presentation::render::{render_spend_report, render_status_report};
use crate::report::ReportEnvelope;
use crate::report::spend::{SpendWindow, assemble as assemble_spend};
use crate::report::{LedgerGeneration, MeterAccount, ReportMetadata, StatusReport};

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
    LoggingFixture,
    StateCheck,
    ExitClass,
    AttemptCrashHook,
    RateCard,
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
    pub no_color: FlagSupport,
    pub verbosity: FlagSupport,
}

impl Command {
    /// Every command, in one place: the enumeration test drives from this rather
    /// than keeping its own private list. `all_lists_every_declared_variant` pins
    /// this array against [`Command::DECLARED_VARIANTS`], which the enum's own
    /// declaration derives, so a variant that joins the enum without joining this
    /// array fails a test that names it.
    pub const ALL: [Self; 8] = [
        Self::Status,
        Self::Spend,
        Self::Config,
        Self::LoggingFixture,
        Self::StateCheck,
        Self::ExitClass,
        Self::AttemptCrashHook,
        Self::RateCard,
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
                explain: FlagSupport::Rejected {
                    reason: "status reports no derived quantity",
                },
                account: FlagSupport::Accepted,
                no_color: FlagSupport::Rejected {
                    reason: "status prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::Spend => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Rejected {
                    reason: "spend prints its ingest summary on every run; --explain arrives with the provenance bead",
                },
                account: FlagSupport::Rejected {
                    reason: "spend has no account dimension until account attribution lands",
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
                no_color: FlagSupport::Rejected {
                    reason: "config prints no color",
                },
                verbosity: FlagSupport::Accepted,
            },
            Command::LoggingFixture => FlagPolicy {
                format: FlagSupport::Accepted,
                explain: FlagSupport::Rejected {
                    reason: "logging-fixture derives no quantity",
                },
                account: FlagSupport::Rejected {
                    reason: "logging-fixture takes no account",
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
                no_color: FlagSupport::Rejected {
                    reason: "attempt-crash-hook prints plain counts",
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
                no_color: FlagSupport::Rejected {
                    reason: "rate-card prints plain rows",
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
            Command::LoggingFixture => "__logging-fixture",
            Command::StateCheck => "__state-check",
            Command::ExitClass => "__exit-class",
            Command::AttemptCrashHook => "__attempt-crash-hook",
            Command::RateCard => "rate-card",
        }
    }

    /// The one-line description `--help` prints. A test hook prints none: it is not
    /// part of the shipping surface and help does not list it.
    pub fn summary(self) -> Option<&'static str> {
        match self {
            Command::Status => Some("render the last known meter reading per configured account"),
            Command::Spend => Some(
                "token usage per UTC day and transcript source, read from the configured transcripts",
            ),
            Command::Config => {
                Some("print every resolved configuration key with the source that won")
            }
            Command::LoggingFixture | Command::StateCheck | Command::ExitClass => None,
            Command::AttemptCrashHook => None,
            Command::RateCard => {
                Some("import, show and history the immutable dated vendor rate cards")
            }
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
    let command = Command::from_name(name)
        .ok_or_else(|| Error::Usage(format!("unknown argument: {name}")))?;

    let mut format = OutputFormat::Text;
    let mut rest = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let arg = arg
            .to_str()
            .ok_or_else(|| Error::Usage("argument is not valid UTF-8".into()))?
            .to_string();
        let format_value = if let Some(value) = arg.strip_prefix("--format=") {
            Some(value.to_string())
        } else if arg == "--format" {
            Some(next_arg(&mut args, "--format")?)
        } else {
            None
        };
        match format_value {
            Some(value) => format = parse_format(command, &value)?,
            None if arg == "-v" => verbosity += 1,
            None => rest.push(arg),
        }
    }
    Ok(Request::Run(Invocation {
        command,
        format,
        verbosity,
        rest,
    }))
}

fn parse_format(command: Command, value: &str) -> Result<OutputFormat, Error> {
    match command.flag_policy().format {
        FlagSupport::Rejected { reason } => Err(Error::Usage(format!(
            "{} does not accept --format: {reason}",
            command.name()
        ))),
        FlagSupport::Accepted => match value {
            "json" => Ok(OutputFormat::Json),
            "text" => Ok(OutputFormat::Text),
            other => Err(Error::Usage(format!(
                "--format must be text or json, got {other}"
            ))),
        },
    }
}

/// The `--help` text: one line per shipping command, plus the shared flags.
pub fn help_text() -> String {
    let mut lines = vec![
        "aub: one ledger for LLM consumption".to_string(),
        String::new(),
        "usage: aub [-v...] <command> [--format text|json] [options]".to_string(),
        String::new(),
        "commands:".to_string(),
    ];
    for command in Command::ALL {
        if let Some(summary) = command.summary() {
            lines.push(format!("  {:<8} {summary}", command.name()));
        }
    }
    lines.extend([
        String::new(),
        "spend options: --today (default) | --since YYYY-MM-DD | --days N".to_string(),
        "config options: --set key=value (repeatable), --config-file PATH".to_string(),
        String::new(),
        "aub            prints the version; aub --help prints this".to_string(),
    ]);
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
            status(&RealClock::new(), level, invocation.format)
        }
        Command::Spend => spend(&RealClock::new(), level, &invocation),
        Command::Config => config_command(invocation.rest.into_iter().map(OsString::from)),
        Command::LoggingFixture => logging_fixture(&RealClock::new(), level),
        Command::StateCheck => state_check(&RealClock::new(), level),
        Command::ExitClass => {
            let class = invocation.rest.first().and_then(|s| s.parse::<u8>().ok());
            match class {
                Some(n) => crate::error::representative_outcome(n),
                None => Err(Error::Usage("__exit-class requires a class 0..=8".into())),
            }
        }
        Command::AttemptCrashHook => attempt_crash_hook(&RealClock::new(), &invocation),
        Command::RateCard => rate_card_command(&RealClock::new(), &invocation),
    }
}

fn reject_positionals(invocation: &Invocation) -> Result<(), Error> {
    match invocation.rest.first() {
        Some(extra) => Err(Error::Usage(format!("unknown argument: {extra}"))),
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

    let window = spend_window(&invocation.rest, timestamp)?;
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;
    let report = assemble_spend(&config, window, timestamp)?;
    match invocation.format {
        OutputFormat::Text => println!("{}", render_spend_report(&report)),
        OutputFormat::Json => println!("{}", spend_json(&report, run)),
    }
    if report.ingest.unreadable_files.is_empty() {
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
fn spend_window(
    rest: &[String],
    now: crate::domain::time::UtcTimestamp,
) -> Result<SpendWindow, Error> {
    let mut since: Option<UtcDate> = None;
    let mut days: i64 = 1;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--today" => since = Some(now.utc_date()),
            "--since" => {
                let value = args
                    .next()
                    .ok_or_else(|| Error::Usage("--since requires YYYY-MM-DD".into()))?;
                since = Some(parse_date(value)?);
            }
            "--days" => {
                let value = args
                    .next()
                    .ok_or_else(|| Error::Usage("--days requires a number".into()))?;
                days = value
                    .parse()
                    .map_err(|_| Error::Usage(format!("--days must be a number, got {value}")))?;
            }
            other => match other.strip_prefix("--since=") {
                Some(value) => since = Some(parse_date(value)?),
                None => match other.strip_prefix("--days=") {
                    Some(value) => {
                        days = value.parse().map_err(|_| {
                            Error::Usage(format!("--days must be a number, got {value}"))
                        })?
                    }
                    None => return Err(Error::Usage(format!("unknown argument: {other}"))),
                },
            },
        }
    }
    SpendWindow::starting(since.unwrap_or_else(|| now.utc_date()), days)
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

fn status(clock: &impl Clock, level: Level, format: OutputFormat) -> Result<(), Error> {
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

    // The report is built from the configured accounts; until the projection
    // exists (aub-me5.6) every account's reading is the never-observed state,
    // computed through the freshness machine rather than constructed here.
    let env = crate::config::RealEnv;
    let file_path = resolve_config_file_path(None, &env);
    let file_contents = std::fs::read_to_string(&file_path).ok();
    let (config, _provenance) = crate::config::resolve(
        &crate::config::Overrides::new(),
        &env,
        file_contents.as_deref(),
        &file_path,
    )?;

    let freshness = compute_freshness(
        &FreshnessInput::new(
            None,
            None,
            None,
            None,
            None,
            config.freshness.meter,
            config.sampling.command_budget,
            status_clock_skew_envelope(),
        ),
        clock,
    );
    let accounts = config
        .accounts
        .iter()
        .map(|account| MeterAccount::new(LogicalName::new(account.name.clone()), freshness.clone()))
        .collect();
    let metadata = ReportMetadata::new(timestamp, timestamp, LedgerGeneration::new(0), None);
    let report = StatusReport::new(metadata, accounts, vec![]);
    match format {
        OutputFormat::Text => println!(
            "{}",
            render_status_report(&report, timestamp, status_clock_skew_envelope())
        ),
        OutputFormat::Json => println!("{}", status_json(&report, run)),
    }
    Ok(())
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

/// Test-only surface: the crash-injection hook for the two-stage meter attempt
/// lifecycle (`aub-sth.6`, PLAN.md section 34.7). `__attempt-crash-hook start`
/// commits the attempt start through the real store APIs and then aborts the
/// process, so a test can prove exactly what survives a kill between the two
/// commits: the start with no result. `complete` is the adjacent positive
/// control, running start then result then a clean exit, and `read-back`
/// reports what the database actually holds.
///
/// The hook's body lives in the store layer (`crate::store::attempt_crash_hook`)
/// because it runs migrations, writes fixture rows and counts rows, all of
/// which the boundary rules confine to `src/store/` (rules 15 and 16); this
/// shim only parses the stage, resolves configuration and renders the outcome.
/// Not part of the shipping command surface: `tests/e2e/command-surface.txt`
/// deliberately excludes every `__`-prefixed hook, matching
/// `__exit-class`/`__state-check`.
fn attempt_crash_hook(clock: &impl Clock, invocation: &Invocation) -> Result<(), Error> {
    let stage = match invocation.rest.first().map(String::as_str) {
        Some("start") => crate::store::attempt_crash_hook::CrashHookStage::Start,
        Some("complete") => crate::store::attempt_crash_hook::CrashHookStage::Complete,
        Some("read-back") => crate::store::attempt_crash_hook::CrashHookStage::ReadBack,
        other => {
            return Err(Error::Usage(format!(
                "__attempt-crash-hook requires a stage (start | complete | read-back), got {other:?}"
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
            crate::store::attempt_crash_hook::run_stage(
                &config.state.dir.join("attempt-crash-hook.db"),
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
        crate::store::attempt_crash_hook::CrashHookOutcome::Counts { starts, results } => {
            println!("starts={starts} results={results}");
        }
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
/// `15`). The rate card is the first production store user, so this is where
/// the database file name resolves from
/// [`crate::store::connection::LEDGER_DATABASE_FILE`].
fn rate_card_open_ledger(clock: &impl Clock) -> Result<rusqlite::Connection, Error> {
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
    let conn = rate_card_open_ledger(clock)?;
    let summary = crate::store::rate_card::insert(&conn, &book.cards, clock.now())?;
    println!(
        "rate-card import: added={} unchanged={}",
        summary.cards_added, summary.cards_unchanged
    );
    Ok(())
}

fn rate_card_show(clock: &impl Clock) -> Result<(), Error> {
    let conn = rate_card_open_ledger(clock)?;
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
    let conn = rate_card_open_ledger(clock)?;
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
            if let FlagSupport::Rejected { reason } = policy.format {
                assert!(
                    !reason.is_empty(),
                    "{command:?} rejects --format without a reason"
                );
            }
        }
    }

    /// This bead ships no explain behaviour: every command keeps rejecting
    /// `--explain`, with a reason, and no explain token is parsed. The explicit
    /// pin makes that state a checked fact rather than an unstated one, so the
    /// explain bead flips it deliberately instead of silently.
    #[test]
    fn every_command_rejects_explain() {
        for command in Command::ALL {
            match command.flag_policy().explain {
                FlagSupport::Rejected { reason } => assert!(
                    !reason.is_empty(),
                    "{command:?} rejects --explain without a reason"
                ),
                FlagSupport::Accepted => {
                    panic!("{command:?} accepts --explain; no explain behaviour ships yet")
                }
            }
        }
    }

    /// `--explain` is not a parsed token: the argument surface recognises only
    /// `-v` and the command names, so an explain flag is an unknown argument.
    /// This is the parser half of "no explain token is parsed".
    #[test]
    fn explain_is_not_a_parsed_token() {
        let result = parse_invocation([
            OsString::from("aub"),
            OsString::from("--explain"),
            OsString::from("status"),
        ]);
        match result {
            Err(Error::Usage(message)) => assert!(
                message.contains("unknown argument"),
                "--explain must be rejected as an unknown argument, got: {message}"
            ),
            other => panic!("--explain must be rejected as an unknown argument, got: {other:?}"),
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
    fn spend_window_defaults_to_the_utc_day_and_reads_its_flags() {
        let now = crate::domain::time::UtcTimestamp::parse_rfc3339("2026-08-30T23:30:00Z").unwrap();
        let today = spend_window(&[], now).unwrap();
        assert_eq!(today.since.iso(), "2026-08-30");
        assert_eq!(today.until.iso(), "2026-08-31");
        let explicit = spend_window(
            &[
                "--since".into(),
                "2026-08-25".into(),
                "--days".into(),
                "3".into(),
            ],
            now,
        )
        .unwrap();
        assert_eq!(explicit.since.iso(), "2026-08-25");
        assert_eq!(explicit.until.iso(), "2026-08-28");
        assert!(spend_window(&["--since".into(), "25/08/2026".into()], now).is_err());
        assert!(spend_window(&["--days".into(), "0".into()], now).is_err());
        assert!(spend_window(&["--bogus".into()], now).is_err());
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
}
