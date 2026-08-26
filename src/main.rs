use std::ffi::OsString;
use std::process::ExitCode;

use agent_usage_book::error::Error;

fn main() -> ExitCode {
    match run(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => ExitCode::from(error.exit_class().code()),
    }
}

fn run<I: IntoIterator<Item = OsString>>(args: I) -> Result<(), Error> {
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(first) = args.next() else {
        println!(
            "{} {} ({})",
            env!("CARGO_BIN_NAME"),
            agent_usage_book::build_info::crate_version(),
            agent_usage_book::build_info::source_revision(),
        );
        return Ok(());
    };
    match first.to_str() {
        Some("__exit-class") => {
            let class = args
                .next()
                .and_then(|s| s.to_str().and_then(|s| s.parse::<u8>().ok()));
            match class {
                Some(n) => agent_usage_book::error::representative_outcome(n),
                None => Err(Error::Usage("__exit-class requires a class 0..=8".into())),
            }
        }
        Some(other) => Err(Error::Usage(format!("unknown argument: {other}"))),
        None => Err(Error::Usage("argument is not valid UTF-8".into())),
    }
}
