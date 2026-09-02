//! How a failed command reaches the user.
//!
//! A failure is either the plain `aub: <message>` line on stderr, or the
//! versioned JSON error envelope on stdout when the command line resolved to a
//! command whose policy accepted `--format json`. This module sits above `cli`
//! and `presentation` so `main` stays a thin entry point (`tests/main_size.rs`)
//! and neither of those modules has to reason about the process's output
//! streams.

use std::ffi::OsString;

use crate::cli::{OutputFormat, Request, parse_invocation};
use crate::error::Error;
use crate::presentation::json::error_envelope_json;

/// A rendered failure and the stream it belongs on.
pub enum FailureOutput {
    /// The versioned JSON error envelope, for stdout.
    Json(String),
    /// The plain `aub: <message>` line, for stderr.
    Plain(String),
}

/// Renders a failed [`crate::cli::run`] for the user.
///
/// The envelope is chosen only when the line parsed to a command whose policy
/// accepted `--format json`. A parse that failed outright, or a command that
/// rejects `--format`, keeps the plain line so its reason stays readable on
/// stderr rather than becoming an error-shaped document on a stream a caller of
/// that command never asked to parse. The command name in the envelope comes
/// from the parsed [`crate::cli::Command`], not an argument scan.
pub fn render_failure(args: &[OsString], error: &Error) -> FailureOutput {
    match parse_invocation(args.iter().cloned()) {
        Ok(Request::Run(invocation)) if invocation.format == OutputFormat::Json => {
            FailureOutput::Json(error_envelope_json(error, Some(invocation.command.name())))
        }
        _ => FailureOutput::Plain(format!("aub: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<OsString> {
        std::iter::once("aub")
            .chain(items.iter().copied())
            .map(OsString::from)
            .collect()
    }

    /// The envelope follows the command's own `--format` policy, not the raw
    /// argument text. The planted negative is the `config` case: `config` rejects
    /// `--format`, so its rejection stays a plain line; a scan of the argument
    /// vector for `--format json` would wrongly route it to `Json` and move the
    /// reason off stderr, which is the regression this replaced.
    #[test]
    fn json_is_chosen_only_when_the_command_accepted_the_flag() {
        match render_failure(
            &argv(&["spend", "--format=json", "--since", "x"]),
            &Error::Usage("bad date".into()),
        ) {
            FailureOutput::Json(doc) => {
                assert!(doc.contains("\"error\"") && doc.contains("\"command\":\"spend\""))
            }
            FailureOutput::Plain(line) => panic!("spend accepts --format json: {line}"),
        }

        match render_failure(
            &argv(&["config", "--format", "json"]),
            &Error::Usage(
                "config does not accept --format: config prints provenance, not a report".into(),
            ),
        ) {
            FailureOutput::Plain(line) => assert!(line.contains("provenance")),
            FailureOutput::Json(doc) => {
                panic!("a rejected --format must not render as json: {doc}")
            }
        }

        match render_failure(
            &argv(&["spend", "--since", "x"]),
            &Error::Usage("bad date".into()),
        ) {
            FailureOutput::Plain(line) => assert!(line.starts_with("aub: ")),
            FailureOutput::Json(doc) => panic!("no --format json was asked for: {doc}"),
        }
    }
}
