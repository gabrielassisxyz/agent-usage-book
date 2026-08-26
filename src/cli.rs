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
        Some("__logging-fixture") => logging_fixture(&RealClock::new(), level),
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
