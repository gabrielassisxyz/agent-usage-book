use agent_usage_book::logging::{LogField, SafeDiagnosticValue};

struct RawProviderBody;

impl SafeDiagnosticValue for RawProviderBody {
    fn write_json(&self, _: &mut String) {}
}

impl LogField for RawProviderBody {}

fn main() {}
