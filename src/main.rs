use std::process::ExitCode;

fn main() -> ExitCode {
    match agent_usage_book::cli::run(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => ExitCode::from(error.exit_class().code()),
    }
}
