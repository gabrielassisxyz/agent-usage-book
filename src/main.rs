use std::ffi::OsString;
use std::process::ExitCode;

use agent_usage_book::presentation::json::error_envelope_json;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    match agent_usage_book::cli::run(args.iter().cloned()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The exit class is the scripting contract; the message is for the
            // person. Dropping it left every usage error silent. A caller that
            // asked for `--format json` gets the failure as the versioned error
            // envelope on stdout instead, so it reads a stable problem code
            // rather than parsing this prose or inferring the class from $?.
            if json_output_requested(&args) {
                println!("{}", error_envelope_json(&error, command_name(&args)));
            } else {
                eprintln!("aub: {error}");
            }
            ExitCode::from(error.exit_class().code())
        }
    }
}

/// Whether the command line explicitly asked for JSON output. This is read again
/// here, rather than carried out of `cli::run`, because the format has to be known
/// on the error path even when parsing itself is what failed.
fn json_output_requested(args: &[OsString]) -> bool {
    let mut args = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = args.next() {
        if arg == "--format=json" {
            return true;
        }
        if arg == "--format" && args.next().as_deref() == Some("json") {
            return true;
        }
    }
    false
}

/// The command token as typed, for the envelope's `command` field: the first
/// argument that is not the program name and not an option. `None` when the line
/// carried no command, which is itself a usage error.
fn command_name(args: &[OsString]) -> Option<&str> {
    args.iter()
        .skip(1)
        .filter_map(|arg| arg.to_str())
        .find(|arg| !arg.starts_with('-'))
}
