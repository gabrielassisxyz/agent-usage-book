// Compile-fail: a resolved credential (the handle a provider adapter receives)
// must not be usable as a structured diagnostic field. It carries the secret
// material, so it can never become a field through the sealed
// SafeDiagnosticValue path.

use agent_usage_book::auth::ResolvedCredential;
use agent_usage_book::logging::LogField;

fn resolved() -> ResolvedCredential {
    unreachable!()
}

fn main() {
    let fields: &[(&str, &dyn LogField)] = &[("resolved", &resolved())];
    let _ = fields;
}
