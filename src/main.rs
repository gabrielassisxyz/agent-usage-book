use std::ffi::OsString;
use std::process::ExitCode;

use agent_usage_book::cli::{self, OutputFormat, Request};
use agent_usage_book::presentation::json::error_envelope_json;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    match cli::run(args.iter().cloned()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The exit class is the scripting contract; the message is for the
            // person. Dropping it left every usage error silent. When the line
            // resolved to a command that accepted `--format json`, the failure
            // is the versioned error envelope on stdout so a caller reads a
            // stable problem code instead of parsing this prose or inferring the
            // class from $?. A command that *rejects* `--format` still fails as a
            // plain usage line, so its reason stays readable on stderr.
            match json_error_command(&args) {
                Some(command) => {
                    println!("{}", error_envelope_json(&error, Some(command)));
                }
                None => eprintln!("aub: {error}"),
            }
            ExitCode::from(error.exit_class().code())
        }
    }
}

/// The command name when the line resolved to a command whose policy accepted
/// `--format json`, otherwise `None`. This is parsed again here, rather than
/// carried out of `cli::run`, because the format has to be known on the error
/// path; a parse that failed outright (an unknown flag, a rejected `--format`)
/// yields `None` and the failure stays a plain stderr line.
fn json_error_command(args: &[OsString]) -> Option<&'static str> {
    match cli::parse_invocation(args.iter().cloned()) {
        Ok(Request::Run(invocation)) if invocation.format == OutputFormat::Json => {
            Some(invocation.command.name())
        }
        _ => None,
    }
}
