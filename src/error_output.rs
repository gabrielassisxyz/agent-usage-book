//! How a failed command reaches the user.
//!
//! A failure is either the plain `aub: <message>` line on stderr, or the
//! versioned JSON error envelope on stdout when the command line resolved to a
//! command whose policy accepted `--format json`. This module sits above `cli`
//! and `presentation` so `main` stays a thin entry point (`tests/main_size.rs`)
//! and neither of those modules has to reason about the process's output
//! streams.

use std::ffi::OsString;

use crate::cli::{Command, OutputFormat, Request, parse_invocation};
use crate::error::Error;
use crate::presentation::json::error_envelope_json;
use crate::presentation::render::render_actionable_failure_message;

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
    render_failure_with_home(args, error, std::env::var("HOME").ok().as_deref())
}

fn render_failure_with_home(args: &[OsString], error: &Error, home: Option<&str>) -> FailureOutput {
    let command = args.iter().skip(1).find_map(|arg| {
        let name = arg.to_str()?;
        Command::ALL
            .into_iter()
            .find(|command| command.name() == name)
    });
    let message = render_actionable_failure_message(error, command.map(Command::name), home);
    let rendered_error = error_with_rendered_message(error, message.clone());
    match parse_invocation(args.iter().cloned()) {
        Ok(Request::Run(invocation)) if invocation.format == OutputFormat::Json => {
            FailureOutput::Json(error_envelope_json(
                &rendered_error,
                Some(invocation.command.name()),
            ))
        }
        _ => FailureOutput::Plain(format!("aub: {message}")),
    }
}

fn error_with_rendered_message(error: &Error, message: String) -> Error {
    match error {
        Error::Internal(_) => Error::Internal(message),
        Error::Usage(_) => Error::Usage(message),
        Error::AuthRequired(_) => Error::AuthRequired(message),
        Error::RemoteUnavailable(_) => Error::RemoteUnavailable(message),
        Error::Store(_) => Error::Store(message),
        Error::InsufficientEvidence(_) => Error::InsufficientEvidence(message),
        Error::ThresholdNotMet(_) => Error::ThresholdNotMet(message),
        Error::IngestIncomplete(_) => Error::IngestIncomplete(message),
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

    /// Every public error class reaches both output modes with a concrete next
    /// action, while a path under the current home is collapsed before either
    /// plain text or JSON can expose it.
    #[test]
    fn every_actionable_error_names_a_next_action_without_an_absolute_home_path() {
        let home = "/tmp/synthetic-home";
        let errors = [
            Error::Internal(format!("failure at {home}/state/aub.sqlite3")),
            Error::Usage(format!("invalid value in {home}/.config/aub/config.toml")),
            Error::AuthRequired(format!("credential missing at {home}/credentials.json")),
            Error::RemoteUnavailable(format!("source unavailable from {home}/source")),
            Error::Store(format!("cannot open {home}/state/aub.sqlite3")),
            Error::InsufficientEvidence(format!("evidence missing under {home}/evidence")),
            Error::ThresholdNotMet(format!("threshold file {home}/thresholds.toml")),
            Error::IngestIncomplete(format!("unreadable {home}/transcripts/run.jsonl")),
        ];
        for error in errors {
            for output in [
                render_failure_with_home(&argv(&["status"]), &error, Some(home)),
                render_failure_with_home(
                    &argv(&["status", "--format", "json"]),
                    &error,
                    Some(home),
                ),
            ] {
                let rendered = match output {
                    FailureOutput::Plain(line) => line,
                    FailureOutput::Json(document) => document,
                };
                assert!(
                    rendered.contains("next:"),
                    "missing next action: {rendered}"
                );
                assert!(
                    rendered.contains("run aub status")
                        || rendered.contains("run aub --help")
                        || rendered.contains("set accounts[].credential")
                        || rendered.contains("check the state.dir")
                        || rendered.contains("fix the named local prerequisite"),
                    "next action is not concrete: {rendered}"
                );
                assert!(
                    !rendered.contains(home),
                    "absolute home path reached output: {rendered}"
                );
                assert!(
                    rendered.contains("~/"),
                    "home path was not collapsed: {rendered}"
                );
            }
        }
    }
}
