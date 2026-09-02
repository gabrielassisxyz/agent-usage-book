use std::ffi::OsString;
use std::process::ExitCode;

use agent_usage_book::cli;
use agent_usage_book::error_output::{FailureOutput, render_failure};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    match cli::run(args.iter().cloned()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The exit class is the scripting contract; the message is for the
            // person. How the message is rendered and which stream it lands on
            // is `error_output`'s call, so this stays a thin entry point.
            match render_failure(&args, &error) {
                FailureOutput::Json(envelope) => println!("{envelope}"),
                FailureOutput::Plain(line) => eprintln!("{line}"),
            }
            ExitCode::from(error.exit_class().code())
        }
    }
}
