use std::process::ExitCode;

fn main() -> ExitCode {
    match agent_usage_book::cli::run(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The exit class is the scripting contract; the message is for the
            // person. Dropping it left every usage error silent.
            eprintln!("aub: {error}");
            ExitCode::from(error.exit_class().code())
        }
    }
}
