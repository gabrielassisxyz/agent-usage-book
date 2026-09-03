//! Structured diagnostics with a sealed safe-field boundary.
//!
//! May not depend on:
//! - provider adapters, credential stores, or filesystem paths
//! - report rendering or terminal-formatting crates

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::time::UtcTimestamp;

static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Five ordered levels. The scheduler-facing default is [`Level::Warn`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    pub const DEFAULT: Self = Self::Warn;

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    pub fn raised_by(self, count: u8) -> Self {
        match count {
            0 => self,
            1 => Self::Info,
            2 => Self::Debug,
            _ => Self::Trace,
        }
    }
}

/// Invocation identifier shared by every diagnostic and report envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunId(String);

impl RunId {
    pub fn new(timestamp: UtcTimestamp) -> Self {
        let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self(format!(
            "run-{}-{}-{sequence}",
            std::process::id(),
            timestamp.unix_nanos()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }
}

impl std::str::FromStr for RunId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

/// Typed vocabulary for the events available before sampling workflows land.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticEvent {
    RunStarted,
    ReportRendered,
    /// A network request was about to be made. Emitted only after a state-directory
    /// readiness check has already passed (`crate::store::startup`), so its absence
    /// from a run's log is itself the proof that a refused run never reached the
    /// network.
    RequestAttempted,
    /// One bounded ingest batch committed (`aub-lqe.18`). `batch` and
    /// `generation` are the stable identifiers that correlate the batch with
    /// its rows and the report; `writer_slot` is the hold the budget judges.
    IngestBatchLanded,
    /// A meter terminal bundle committed through the repository boundary.
    /// `attempt` is the attempt the evidence belongs to and `busy_wait` is how
    /// long the commit waited for the writer slot before it could run.
    MeterAttemptCommitted,
    /// A meter terminal bundle was spooled durably instead of committed: the
    /// writer slot stayed held longer than the caller's bound, and the record
    /// remains discoverable by the next drain. `attempt` names the evidence.
    MeterEvidenceSpooled,
    /// One pending-spool drain pass finished. `applied`, `already_applied` and
    /// `quarantined` are the three dispositions, counted; the pass that applies
    /// nothing still reports itself so a log shows the drain ran.
    MeterSpoolDrained,
}

impl DiagnosticEvent {
    pub const ALL: [Self; 7] = [
        Self::RunStarted,
        Self::ReportRendered,
        Self::RequestAttempted,
        Self::IngestBatchLanded,
        Self::MeterAttemptCommitted,
        Self::MeterEvidenceSpooled,
        Self::MeterSpoolDrained,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::RunStarted => "run_started",
            Self::ReportRendered => "report_rendered",
            Self::RequestAttempted => "request_attempted",
            Self::IngestBatchLanded => "ingest_batch_landed",
            Self::MeterAttemptCommitted => "meter_attempt_committed",
            Self::MeterEvidenceSpooled => "meter_evidence_spooled",
            Self::MeterSpoolDrained => "meter_spool_drained",
        }
    }

    pub fn level(self) -> Level {
        Level::Info
    }

    pub fn documented_fields(self) -> &'static str {
        match self {
            Self::RunStarted => "command",
            Self::ReportRendered => "report_kind",
            Self::RequestAttempted => "command",
            Self::IngestBatchLanded => "batch, events, writer_slot, generation",
            Self::MeterAttemptCommitted => "attempt, busy_wait",
            Self::MeterEvidenceSpooled => "attempt",
            Self::MeterSpoolDrained => "applied, already_applied, quarantined",
        }
    }
}

mod private {
    pub trait Sealed {}
}

/// Values proved safe for diagnostic serialization.
///
/// This is sealed positively: credential, raw-body, and filesystem-path wrappers
/// cannot become fields until this module explicitly gives them a sanitized form.
pub trait SafeDiagnosticValue: private::Sealed {
    fn write_json(&self, output: &mut String);
}

/// A value accepted as a structured diagnostic field.
pub trait LogField: SafeDiagnosticValue {}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogicalName(String);

impl LogicalName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The underlying name, for rendering and diagnostics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Redacted {
    _raw: String,
}

impl Redacted {
    pub fn credential(raw: &str) -> Self {
        Self {
            _raw: raw.to_owned(),
        }
    }

    pub fn provider_body(raw: &str) -> Self {
        Self {
            _raw: raw.to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Quantity {
    value: u64,
    unit: &'static str,
}

impl Quantity {
    pub fn new(value: u64, unit: &'static str) -> Self {
        Self { value, unit }
    }
}

impl private::Sealed for LogicalName {}
impl SafeDiagnosticValue for LogicalName {
    fn write_json(&self, output: &mut String) {
        push_json_string(output, &self.0);
    }
}
impl LogField for LogicalName {}

impl private::Sealed for Redacted {}
impl SafeDiagnosticValue for Redacted {
    fn write_json(&self, output: &mut String) {
        push_json_string(output, "[REDACTED]");
    }
}
impl LogField for Redacted {}

impl private::Sealed for Quantity {}
impl SafeDiagnosticValue for Quantity {
    fn write_json(&self, output: &mut String) {
        output.push_str("{\"value\":");
        output.push_str(&self.value.to_string());
        output.push_str(",\"unit\":");
        push_json_string(output, self.unit);
        output.push('}');
    }
}
impl LogField for Quantity {}

/// JSON-lines diagnostics written exclusively to the caller-provided stderr sink.
pub struct DiagnosticLogger<W> {
    output: W,
    level: Level,
    run: RunId,
}

impl<W: Write> DiagnosticLogger<W> {
    pub fn new(output: W, level: Level, run: RunId) -> Self {
        Self { output, level, run }
    }

    pub fn emit(
        &mut self,
        timestamp: UtcTimestamp,
        event: DiagnosticEvent,
        fields: &[(&str, &dyn LogField)],
    ) -> io::Result<()> {
        if event.level() > self.level {
            return Ok(());
        }
        let mut line = format!(
            "{{\"ts\":{},\"level\":\"{}\",\"event\":\"{}\",\"run\":\"{}\"",
            timestamp.unix_nanos(),
            event.level().name(),
            event.name(),
            self.run.as_str(),
        );
        for (name, value) in fields {
            line.push(',');
            push_json_string(&mut line, name);
            line.push(':');
            value.write_json(&mut line);
        }
        line.push_str("}\n");
        self.output.write_all(line.as_bytes())
    }
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use fmt::Write as _;
                write!(output, "\\u{:04x}", character as u32)
                    .expect("writing to String cannot fail");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn each_level_suppresses_only_events_above_its_threshold() {
        let timestamp = UtcTimestamp::from_unix_nanos(1);
        let run = RunId::new(timestamp);
        let name = LogicalName::new("fixture");
        let mut quiet = Vec::new();
        DiagnosticLogger::new(&mut quiet, Level::Warn, run.clone())
            .emit(
                timestamp,
                DiagnosticEvent::RunStarted,
                &[("command", &name)],
            )
            .unwrap();
        assert!(quiet.is_empty());

        for level in [Level::Info, Level::Debug, Level::Trace] {
            let mut output = Vec::new();
            DiagnosticLogger::new(&mut output, level, run.clone())
                .emit(
                    timestamp,
                    DiagnosticEvent::RunStarted,
                    &[("command", &name)],
                )
                .unwrap();
            assert!(!output.is_empty(), "{level:?} must include info events");
        }
        assert_eq!(Level::DEFAULT, Level::Warn);
        assert_eq!(Level::DEFAULT.raised_by(1), Level::Info);
        assert_eq!(Level::DEFAULT.raised_by(2), Level::Debug);
        assert_eq!(Level::DEFAULT.raised_by(3), Level::Trace);
    }

    proptest::proptest! {
        #[test]
        fn prop_redacted_values_never_serialize_input_at_any_level(
            secret in "SECRET_[a-zA-Z0-9_-]{10,50}",
            path in "PATH_[a-zA-Z0-9_-]{10,50}",
            level_idx in 0usize..5,
        ) {
            let levels = [
                Level::Error,
                Level::Warn,
                Level::Info,
                Level::Debug,
                Level::Trace,
            ];
            let level = levels[level_idx % 5];
            let body = format!("{{\"token\":\"{secret}\",\"path\":\"/{path}\"}}");
            let timestamp = UtcTimestamp::from_unix_nanos(1);
            let mut output = Vec::new();
            let run = RunId::new(timestamp);
            let credential = Redacted::credential(&secret);
            let provider_body = Redacted::provider_body(&body);
            let logical_name = LogicalName::new("provider-main");
            let mut logger = DiagnosticLogger::new(&mut output, level, run);
            logger
                .emit(
                    timestamp,
                    DiagnosticEvent::RunStarted,
                    &[
                        ("credential", &credential),
                        ("body", &provider_body),
                        ("source", &logical_name),
                    ],
                )
                .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            prop_assert!(!rendered.contains(&secret));
            prop_assert!(!rendered.contains(&body));
            prop_assert!(!rendered.contains(&path));
        }
    }

    /// Retained hand-picked regression: fixed bearer token and body across all 5 levels.
    #[test]
    fn redacted_values_never_serialize_input_at_any_level_hand_picked() {
        let secret = "Bearer abc.credential-value";
        let body = format!("{{\"token\":\"{secret}\",\"path\":\"/home/private\"}}");
        let timestamp = UtcTimestamp::from_unix_nanos(1);
        for level in [
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ] {
            let mut output = Vec::new();
            let run = RunId::new(timestamp);
            let credential = Redacted::credential(secret);
            let provider_body = Redacted::provider_body(&body);
            let logical_name = LogicalName::new("provider-main");
            let mut logger = DiagnosticLogger::new(&mut output, level, run);
            logger
                .emit(
                    timestamp,
                    DiagnosticEvent::RunStarted,
                    &[
                        ("credential", &credential),
                        ("body", &provider_body),
                        ("source", &logical_name),
                    ],
                )
                .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(!rendered.contains(secret));
            assert!(!rendered.contains(&body));
            assert!(!rendered.contains("/home/private"));
        }
    }

    #[test]
    fn documented_event_vocabulary_matches_typed_enum() {
        let document =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/diagnostics.md"))
                .expect("diagnostic vocabulary must be readable");
        for event in DiagnosticEvent::ALL {
            let row = format!(
                "| {} | {} | {} |",
                event.name(),
                event.level().name(),
                event.documented_fields()
            );
            assert!(
                document.contains(&row),
                "diagnostic vocabulary has no row matching {row:?}"
            );
        }
    }
}
