//! CLI helpers shared by more than one command cluster.
//!
//! Every function here is called from several clusters of `src/cli.rs` but
//! belongs to none of them, so it lives here rather than inside one cluster's
//! future module. Bodies are unchanged from `src/cli.rs`.

use std::ffi::OsString;

use super::Invocation;
use crate::domain::time::{Clock, UtcDate};
use crate::error::Error;

pub(crate) fn reject_positionals(invocation: &Invocation) -> Result<(), Error> {
    match invocation.rest.first() {
        Some(extra) => Err(Error::Usage(format!(
            "unknown argument: {extra}; run aub --help for command usage"
        ))),
        None => Ok(()),
    }
}

pub(crate) fn parse_date(value: &str) -> Result<UtcDate, Error> {
    UtcDate::parse(value)
        .ok_or_else(|| Error::Usage(format!("--since must be YYYY-MM-DD, got {value}")))
}

pub(crate) fn next_arg(
    args: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> Result<String, Error> {
    args.next()
        .and_then(|s| s.to_str().map(str::to_string))
        .ok_or_else(|| Error::Usage(format!("{flag} requires an argument")))
}

/// The config file's own path cannot be sourced from the file it names, so it gets a
/// narrower, three-level resolution ahead of everything else: `--config-file`, then
/// `AUB_CONFIG_FILE`, then the non-identifying platform default under `$HOME`.
pub(crate) fn resolve_config_file_path(
    flag: Option<&str>,
    env: &dyn crate::config::EnvSource,
) -> String {
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

/// Opens the one production ledger database through the one connection path:
/// state readiness first, then the store-side open (which runs migrations;
/// `src/cli.rs` must never name the migration framework itself, boundary rule
/// `15`). Every production store user shares this: the database file name
/// resolves from [`crate::store::connection::LEDGER_DATABASE_FILE`], and the
/// readiness gate runs before any connection is made.
pub(crate) fn open_ledger(clock: &impl Clock) -> Result<rusqlite::Connection, Error> {
    open_ledger_with_config(clock).map(|(conn, _config)| conn)
}

/// The same open, handing back the configuration it had to resolve anyway. A
/// caller that also needs the model table takes this one rather than resolving
/// the file a second time and risking two readings of it in one command.
pub(crate) fn open_ledger_with_config(
    clock: &impl Clock,
) -> Result<(rusqlite::Connection, crate::config::Config), Error> {
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
    Ok((opened, config))
}

/// Resolves the configuration the backup family reads, the same way every
/// command in it does, so the state directory the restore's refusals compare
/// against is the one configuration actually names.
pub(crate) fn resolve_backup_config() -> Result<crate::config::Config, Error> {
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
