// Compile-fail: a secret wrapper must not be usable as a structured diagnostic
// field. SafeDiagnosticValue is sealed positively: a value becomes a field only
// when logging.rs explicitly gives it a sanitized form, and credential
// material never gets one.

use agent_usage_book::auth::Secret;
use agent_usage_book::logging::LogField;

fn secret() -> Secret<String> {
    unreachable!()
}

fn main() {
    let fields: &[(&str, &dyn LogField)] = &[("credential", &secret())];
    let _ = fields;
}
