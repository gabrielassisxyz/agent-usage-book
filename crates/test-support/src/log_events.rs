//! Assertion helpers over structured log events.

use std::collections::BTreeMap;

/// A structured log event: a name and a set of string fields, mirroring the JSON-lines
/// shape the production logging contract emits (`ts`, `level`, `event`, `run`, plus
/// per-event fields). Tests build these directly or parse them from captured stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    pub name: String,
    pub fields: BTreeMap<String, String>,
}

impl LogEvent {
    /// An event with the given name and no fields.
    pub fn new(name: impl Into<String>) -> Self {
        LogEvent {
            name: name.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Adds one field, returning the event for chaining.
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }
}

/// Asserts that `events` contains an event with the given name whose `field` equals
/// `expected_value`. On failure it panics with a readable diff: the expected event,
/// then every actual event, so the reader can see what was there instead.
pub fn assert_event<'a>(
    events: impl IntoIterator<Item = &'a LogEvent>,
    name: &str,
    field: &str,
    expected_value: &str,
) {
    let events: Vec<&LogEvent> = events.into_iter().collect();
    let found = events.iter().any(|event| {
        event.name == name && event.fields.get(field).map(String::as_str) == Some(expected_value)
    });
    if found {
        return;
    }

    let mut message = format!(
        "expected log event not found:\n  name: {name:?}, field {field:?} == {expected_value:?}\nactual events ({}):",
        events.len()
    );
    for event in &events {
        message.push_str(&format!("\n  - {} {:?}", event.name, event.fields));
    }
    panic!("{message}");
}
