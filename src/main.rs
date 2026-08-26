fn main() {
    println!(
        "{} {} ({})",
        env!("CARGO_BIN_NAME"),
        agent_usage_book::build_info::crate_version(),
        agent_usage_book::build_info::source_revision(),
    );
}
