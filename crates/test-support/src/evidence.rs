//! Evidence fixture builders: an attempt, a response evidence row, an observation with
//! windows, a usage event with components, and a marker.
//!
//! These are test-only stand-ins for the domain types that later beads define. Each
//! builder has sane fixed defaults and per-field overrides, so a test states only the
//! field it cares about and inherits a valid value for the rest. Defaults are fixed
//! rather than random, so a builder is deterministic by construction; the seeded
//! generators in [`crate::rng`] are the only source of randomness and always take an
//! explicit seed.

/// A collection attempt: durable before any network I/O, with a terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub attempt_id: String,
    pub account: String,
    pub started_at: u64,
    pub outcome: AttemptOutcome,
}

/// The terminal outcome of an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    Success { observation_id: String },
    AuthRequired { reason: String },
    Unreachable { class: String },
}

impl Attempt {
    pub fn builder() -> AttemptBuilder {
        AttemptBuilder::default()
    }
}

#[derive(Default)]
pub struct AttemptBuilder {
    attempt_id: Option<String>,
    account: Option<String>,
    started_at: Option<u64>,
    outcome: Option<AttemptOutcome>,
}

impl AttemptBuilder {
    pub fn attempt_id(mut self, v: impl Into<String>) -> Self {
        self.attempt_id = Some(v.into());
        self
    }
    pub fn account(mut self, v: impl Into<String>) -> Self {
        self.account = Some(v.into());
        self
    }
    pub fn started_at(mut self, v: u64) -> Self {
        self.started_at = Some(v);
        self
    }
    pub fn outcome(mut self, v: AttemptOutcome) -> Self {
        self.outcome = Some(v);
        self
    }
    pub fn build(self) -> Attempt {
        Attempt {
            attempt_id: self.attempt_id.unwrap_or_else(|| "attempt-1".into()),
            account: self.account.unwrap_or_else(|| "work-a".into()),
            started_at: self.started_at.unwrap_or(1_700_000_000),
            outcome: self.outcome.unwrap_or(AttemptOutcome::Success {
                observation_id: "obs-1".into(),
            }),
        }
    }
}

/// A sanitized provider response evidence row, persisted alongside the attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseEvidenceRow {
    pub attempt_id: String,
    pub received_at: u64,
    pub status: u16,
    pub body: String,
}

impl ResponseEvidenceRow {
    pub fn builder() -> ResponseEvidenceRowBuilder {
        ResponseEvidenceRowBuilder::default()
    }
}

#[derive(Default)]
pub struct ResponseEvidenceRowBuilder {
    attempt_id: Option<String>,
    received_at: Option<u64>,
    status: Option<u16>,
    body: Option<String>,
}

impl ResponseEvidenceRowBuilder {
    pub fn attempt_id(mut self, v: impl Into<String>) -> Self {
        self.attempt_id = Some(v.into());
        self
    }
    pub fn received_at(mut self, v: u64) -> Self {
        self.received_at = Some(v);
        self
    }
    pub fn status(mut self, v: u16) -> Self {
        self.status = Some(v);
        self
    }
    pub fn body(mut self, v: impl Into<String>) -> Self {
        self.body = Some(v.into());
        self
    }
    pub fn build(self) -> ResponseEvidenceRow {
        ResponseEvidenceRow {
            attempt_id: self.attempt_id.unwrap_or_else(|| "attempt-1".into()),
            received_at: self.received_at.unwrap_or(1_700_000_000),
            status: self.status.unwrap_or(200),
            body: self.body.unwrap_or_else(|| "{}".into()),
        }
    }
}

/// A normalized observation carrying one or more quota windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub observation_id: String,
    pub account: String,
    pub received_at: u64,
    pub windows: Vec<Window>,
}

/// One quota window inside an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub start: u64,
    pub end: u64,
    pub credits: u64,
}

impl Observation {
    pub fn builder() -> ObservationBuilder {
        ObservationBuilder::default()
    }
}

#[derive(Default)]
pub struct ObservationBuilder {
    observation_id: Option<String>,
    account: Option<String>,
    received_at: Option<u64>,
    windows: Option<Vec<Window>>,
}

impl ObservationBuilder {
    pub fn observation_id(mut self, v: impl Into<String>) -> Self {
        self.observation_id = Some(v.into());
        self
    }
    pub fn account(mut self, v: impl Into<String>) -> Self {
        self.account = Some(v.into());
        self
    }
    pub fn received_at(mut self, v: u64) -> Self {
        self.received_at = Some(v);
        self
    }
    pub fn windows(mut self, v: Vec<Window>) -> Self {
        self.windows = Some(v);
        self
    }
    pub fn build(self) -> Observation {
        Observation {
            observation_id: self.observation_id.unwrap_or_else(|| "obs-1".into()),
            account: self.account.unwrap_or_else(|| "work-a".into()),
            received_at: self.received_at.unwrap_or(1_700_000_000),
            windows: self.windows.unwrap_or_else(|| {
                vec![Window {
                    start: 1_699_000_000,
                    end: 1_700_000_000,
                    credits: 100,
                }]
            }),
        }
    }
}

/// A usage event carrying per-kind token components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub event_id: String,
    pub account: String,
    pub at: u64,
    pub components: Vec<Component>,
}

/// One token component of a usage event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub kind: String,
    pub tokens: u64,
}

impl UsageEvent {
    pub fn builder() -> UsageEventBuilder {
        UsageEventBuilder::default()
    }
}

#[derive(Default)]
pub struct UsageEventBuilder {
    event_id: Option<String>,
    account: Option<String>,
    at: Option<u64>,
    components: Option<Vec<Component>>,
}

impl UsageEventBuilder {
    pub fn event_id(mut self, v: impl Into<String>) -> Self {
        self.event_id = Some(v.into());
        self
    }
    pub fn account(mut self, v: impl Into<String>) -> Self {
        self.account = Some(v.into());
        self
    }
    pub fn at(mut self, v: u64) -> Self {
        self.at = Some(v);
        self
    }
    pub fn components(mut self, v: Vec<Component>) -> Self {
        self.components = Some(v);
        self
    }
    pub fn build(self) -> UsageEvent {
        UsageEvent {
            event_id: self.event_id.unwrap_or_else(|| "event-1".into()),
            account: self.account.unwrap_or_else(|| "work-a".into()),
            at: self.at.unwrap_or(1_700_000_000),
            components: self.components.unwrap_or_else(|| {
                vec![Component {
                    kind: "input".into(),
                    tokens: 1_000,
                }]
            }),
        }
    }
}

/// An account or session marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub marker_id: String,
    pub account: String,
    pub at: u64,
    pub kind: MarkerKind,
}

/// The kind of a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    Account,
    Session,
}

impl Marker {
    pub fn builder() -> MarkerBuilder {
        MarkerBuilder::default()
    }
}

#[derive(Default)]
pub struct MarkerBuilder {
    marker_id: Option<String>,
    account: Option<String>,
    at: Option<u64>,
    kind: Option<MarkerKind>,
}

impl MarkerBuilder {
    pub fn marker_id(mut self, v: impl Into<String>) -> Self {
        self.marker_id = Some(v.into());
        self
    }
    pub fn account(mut self, v: impl Into<String>) -> Self {
        self.account = Some(v.into());
        self
    }
    pub fn at(mut self, v: u64) -> Self {
        self.at = Some(v);
        self
    }
    pub fn kind(mut self, v: MarkerKind) -> Self {
        self.kind = Some(v);
        self
    }
    pub fn build(self) -> Marker {
        Marker {
            marker_id: self.marker_id.unwrap_or_else(|| "marker-1".into()),
            account: self.account.unwrap_or_else(|| "work-a".into()),
            at: self.at.unwrap_or(1_700_000_000),
            kind: self.kind.unwrap_or(MarkerKind::Account),
        }
    }
}
