//! Argument parsing and orchestration.
//!
//! May not depend on:
//! - presentation (it orchestrates, it does not format)
//! - provider adapters directly

use std::ffi::OsString;
use std::io;

use crate::domain::time::{Clock, RealClock};
use crate::error::Error;
use crate::logging::{DiagnosticEvent, DiagnosticLogger, Level, LogicalName, RunId};
use crate::report::ReportEnvelope;

/// Every command the CLI exposes.
///
/// The exhaustive match in [`Command::flag_policy`] is what makes the shared flag
/// policy a compile-time obligation: adding a command means adding a variant, and
/// the match refuses to compile until that variant declares its policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Status,
    Config,
    LoggingFixture,
    StateCheck,
    ExitClass,
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
        }
    }
}

/// Parse the early command surface and route it to bounded workflows.
pub fn run<I: IntoIterator<Item = OsString>>(args: I) -> Result<(), Error> {
    let mut args = args.into_iter();
    let _program = args.next();
    let mut verbosity: u8 = 0;
    let mut first = args.next();
    while matches!(first.as_ref().and_then(|arg| arg.to_str()), Some("-v")) {
        verbosity += 1;
        first = args.next();
    }
    let level = std::env::var("AUB_LOG_LEVEL")
        .ok()
        .and_then(|value| Level::parse(&value))
        .unwrap_or(Level::DEFAULT)
        .raised_by(verbosity);
    let Some(first) = first else {
        println!(
            "aub {} ({})",
            crate::build_info::crate_version(),
            crate::build_info::source_revision(),
        );
        return Ok(());
    };
    match first.to_str() {
        Some("status") => status(&RealClock::new(), level),
        Some("config") => config_command(args),
        Some("__logging-fixture") => logging_fixture(&RealClock::new(), level),
        Some("__state-check") => state_check(&RealClock::new(), level),
        Some("__exit-class") => {
            let class = args
                .next()
                .and_then(|s| s.to_str().and_then(|s| s.parse::<u8>().ok()));
            match class {
                Some(n) => crate::error::representative_outcome(n),
                None => Err(Error::Usage("__exit-class requires a class 0..=8".into())),
            }
        }
        Some(other) => Err(Error::Usage(format!("unknown argument: {other}"))),
        None => Err(Error::Usage("argument is not valid UTF-8".into())),
    }
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

fn status(clock: &impl Clock, level: Level) -> Result<(), Error> {
    let timestamp = clock.now();
    let run = RunId::new(timestamp);
    let command = LogicalName::new("status");
    let mut logger = DiagnosticLogger::new(io::stderr(), level, run);
    logger
        .emit(
            timestamp,
            DiagnosticEvent::RunStarted,
            &[("command", &command)],
        )
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    println!("status unavailable");
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

    // The closure below stands in for a real command's first network-touching call.
    // `run_after_state_check` never invokes it unless the check above already
    // succeeded, which is what makes the ordering provable rather than assumed.
    let emit_request_attempted = crate::store::startup::run_after_state_check(
        &config.state.dir,
        &crate::store::startup::ProcMounts,
        || {
            logger.emit(
                clock.now(),
                DiagnosticEvent::RequestAttempted,
                &[("command", &command)],
            )
        },
    )?;
    emit_request_attempted
        .map_err(|error| Error::Internal(format!("write diagnostic: {error}")))?;
    println!("state directory ready");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FakeEnv;

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
