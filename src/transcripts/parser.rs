//! Parser adapters: one trait, explicit evidence classification, and quarantine.
//!
//! A transcript parser translates source records into normalized usage events. It
//! must not know subscription window capacity, API pricing, task history, or meter
//! percentages: a parser that learns any of those grows a second opinion about a
//! physical quantity, which is the failure mode this project exists to prevent.
//!
//! May not depend on:
//! - calibration, cost models, rate cards, task history, or meter observations

use std::collections::BTreeMap;

use crate::domain::ids::SessionId;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::UsageVector;
use crate::evidence::{EstimatorId, Provenance};

/// The parser implementation version, carried on every event it emits.
///
/// A distinct type from [`InputFormatVersion`] and [`EstimatorVersion`]: the parser
/// is the code, the input format is the shape it reads, and the estimator is the
/// algorithm that reconstructed a value. Bumping one never silently bumps another.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParserVersion(String);

impl ParserVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The input-format version assumptions a parser makes.
///
/// A parser declares which version of its source format it understands, so a format
/// change that breaks the parser is a versioned fact rather than a silent misparse.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputFormatVersion(String);

impl InputFormatVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The version of the estimator that reconstructed a value.
///
/// Carried on a reconstructed classification so the estimate's algorithm version
/// survives normalization and is never mistaken for a measured count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EstimatorVersion(String);

impl EstimatorVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How the parser obtained the usage values in an event.
///
/// The classification travels with the event forever and must not be erased during
/// normalization: downstream it decides whether the event may enter calibration or
/// the `can-run` reference set. A reconstructed value is never a measured token count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceClassification {
    /// The source reported the values directly (provider or CLI reported).
    Reported,
    /// The values were reconstructed by an estimator, never measured. Carries the
    /// estimator identifier and its version.
    Reconstructed {
        estimator: EstimatorId,
        version: EstimatorVersion,
    },
    /// The values were derived from other reported values in the same record.
    Derived,
}

impl EvidenceClassification {
    /// True when the values were reconstructed rather than reported or derived.
    pub fn is_reconstructed(&self) -> bool {
        matches!(self, Self::Reconstructed { .. })
    }

    /// The estimator and its version, when this classification is reconstructed.
    pub fn reconstructed(&self) -> Option<(&EstimatorId, &EstimatorVersion)> {
        match self {
            Self::Reconstructed { estimator, version } => Some((estimator, version)),
            Self::Reported | Self::Derived => None,
        }
    }
}

/// Where one source record came from: the file identity and the line it starts on.
///
/// The line is a location, not a quantity: it names where a record was found so a
/// quarantine can point the operator at the offending input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    file: String,
    line: u64,
}

impl SourceLocation {
    pub fn new(file: impl Into<String>, line: u64) -> Self {
        Self {
            file: file.into(),
            line,
        }
    }

    pub fn file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> u64 {
        self.line
    }
}

/// Why a source record could not be normalized.
///
/// The parser's own vocabulary, distinct from the transport failure classification:
/// these are the shapes a malformed transcript record takes, not the ways a network
/// request can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineClass {
    /// A required field is absent from the record.
    MissingRequiredField,
    /// A field is present but has the wrong type (e.g. a count that is not a number).
    WrongFieldType,
    /// The record is truncated or otherwise structurally incomplete.
    TruncatedStructure,
    /// The input format version is one this parser does not understand.
    UnsupportedInputFormat,
}

impl QuarantineClass {
    /// The stable name a report prints for this class.
    pub fn name(self) -> &'static str {
        match self {
            QuarantineClass::MissingRequiredField => "missing_required_field",
            QuarantineClass::WrongFieldType => "wrong_field_type",
            QuarantineClass::TruncatedStructure => "truncated_structure",
            QuarantineClass::UnsupportedInputFormat => "unsupported_input_format",
        }
    }
}

/// A source record that could not be normalized.
///
/// Emitted rather than dropped: a silently skipped record is an undercount, and an
/// undercount looks exactly like a correct answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineRecord {
    location: SourceLocation,
    parser_version: ParserVersion,
    class: QuarantineClass,
}

impl QuarantineRecord {
    pub fn new(
        location: SourceLocation,
        parser_version: ParserVersion,
        class: QuarantineClass,
    ) -> Self {
        Self {
            location,
            parser_version,
            class,
        }
    }

    pub fn location(&self) -> &SourceLocation {
        &self.location
    }

    pub fn parser_version(&self) -> &ParserVersion {
        &self.parser_version
    }

    pub fn class(&self) -> QuarantineClass {
        self.class
    }
}

/// The provenance entry prefix that marks a stable, source-provided event identifier.
///
/// The dedup framework treats an entry with this prefix as strong identity rather
/// than a heuristic fingerprint, so the prefix is defined once, here, and every
/// parser that passes a native identifier through writes it under this prefix.
pub const STRONG_IDENTITY_PREFIX: &str = "event-id:";

/// A normalized usage event: a usage vector, its evidence classification, the source
/// provenance, and the parser version that produced it, plus the record timestamp
/// and the session the source attributes it to where the source writes them.
///
/// The timestamp and the session are optional because a source may omit them, and
/// an absent value must stay absent: a report that cannot place an event in a day
/// counts it as undated rather than inventing a day for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedUsageEvent {
    usage: UsageVector,
    classification: EvidenceClassification,
    provenance: Provenance,
    parser_version: ParserVersion,
    occurred_at: Option<UtcTimestamp>,
    session: Option<SessionId>,
    /// A source-provided sequence number for the record, when the source
    /// writes one. The strongest available ordering discriminator for a
    /// cumulative series: a sequence survives clock skew and identical
    /// timestamps where a timestamp alone cannot order two records. `None`
    /// means the source writes no sequence and ordering falls back to the
    /// timestamp path with a documented tiebreak.
    sequence: Option<u64>,
}

impl NormalizedUsageEvent {
    pub fn new(
        usage: UsageVector,
        classification: EvidenceClassification,
        provenance: Provenance,
        parser_version: ParserVersion,
    ) -> Self {
        Self {
            usage,
            classification,
            provenance,
            parser_version,
            occurred_at: None,
            session: None,
            sequence: None,
        }
    }

    /// The same event carrying the record's own timestamp.
    pub fn with_occurred_at(mut self, occurred_at: UtcTimestamp) -> Self {
        self.occurred_at = Some(occurred_at);
        self
    }

    /// The same event attributed to the session its source names.
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session = Some(session);
        self
    }

    /// The same event carrying the source's sequence number for the record.
    pub fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = Some(sequence);
        self
    }

    pub fn occurred_at(&self) -> Option<UtcTimestamp> {
        self.occurred_at
    }

    /// The source-provided sequence number, when the source wrote one.
    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    pub fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }

    /// The source-provided stable identifier, when the source wrote one.
    pub fn strong_identity(&self) -> Option<&str> {
        self.provenance
            .sources()
            .iter()
            .find_map(|source| source.strip_prefix(STRONG_IDENTITY_PREFIX))
    }

    pub fn usage(&self) -> &UsageVector {
        &self.usage
    }

    pub fn classification(&self) -> &EvidenceClassification {
        &self.classification
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub fn parser_version(&self) -> &ParserVersion {
        &self.parser_version
    }
}

/// What one parse produced: normalized events and quarantined records.
///
/// A malformed record quarantines and does not abort the rest of the input, so one
/// broken record never suppresses the records around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOutput {
    events: Vec<NormalizedUsageEvent>,
    quarantined: Vec<QuarantineRecord>,
}

impl ParseOutput {
    pub fn new(events: Vec<NormalizedUsageEvent>, quarantined: Vec<QuarantineRecord>) -> Self {
        Self {
            events,
            quarantined,
        }
    }

    pub fn events(&self) -> &[NormalizedUsageEvent] {
        &self.events
    }

    pub fn quarantined(&self) -> &[QuarantineRecord] {
        &self.quarantined
    }
}

/// The parser adapter trait: one parser per source format.
///
/// The trait's surface is deliberately narrow. A parser declares its own version and
/// its input-format assumptions, and turns one source into normalized events plus
/// quarantine records. It exposes no access to cost models, calibrations, rate cards,
/// task history, or meter observations: the output is a usage vector with an evidence
/// classification, never a priced or calibrated quantity.
pub trait ParserAdapter {
    /// The parser's own version, carried on every event it emits.
    fn parser_version(&self) -> ParserVersion;

    /// The input-format version assumptions this parser makes.
    fn input_format_version(&self) -> InputFormatVersion;

    /// Parses one source into normalized events and quarantine records.
    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput;

    /// Whether this source reports cumulative totals rather than per-record
    /// consumption. A cumulative parser's event carries a total-so-far value,
    /// so summing its events as if each were independent overcounts any series
    /// longer than one record. The dedup module's cumulative pipeline
    /// (`crate::dedup::cumulative`) orders a cumulative source's surviving
    /// events and differences them into deltas; a parser that reports false
    /// has its events counted as they are. Declared once per parser, here, so
    /// the pipeline reads the declaration instead of a per-caller list.
    fn reports_cumulative(&self) -> bool {
        false
    }
}

/// One shape every parser's fixture set must cover.
///
/// The list lives here, once, so a parser bead and the corpus audit cannot hold
/// different ideas of what complete means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FixtureShape {
    SimpleSession,
    NestedSubagentPaths,
    TruncatedFile,
    PartiallyWrittenFinalRecord,
    FileRotation,
    MalformedRecords,
    ModelChangeMidSession,
    CacheReadsAndWrites,
    NoNativeUsageField,
}

/// The fixture catalog every parser owes. Enumerated once, here.
pub const FIXTURE_CATALOG: [FixtureShape; 9] = [
    FixtureShape::SimpleSession,
    FixtureShape::NestedSubagentPaths,
    FixtureShape::TruncatedFile,
    FixtureShape::PartiallyWrittenFinalRecord,
    FixtureShape::FileRotation,
    FixtureShape::MalformedRecords,
    FixtureShape::ModelChangeMidSession,
    FixtureShape::CacheReadsAndWrites,
    FixtureShape::NoNativeUsageField,
];

/// A parser's answer for one catalog shape: an applicable fixture, or a
/// machine-readable not-applicable rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureCoverage {
    Applicable { fixture: String },
    NotApplicable { rationale: String },
}

/// Verifies a parser's declared coverage is complete: every catalog shape is covered,
/// with neither omission accepted. Returns the shapes that are missing.
pub fn verify_fixture_coverage(
    coverage: &BTreeMap<FixtureShape, FixtureCoverage>,
) -> Vec<FixtureShape> {
    FIXTURE_CATALOG
        .iter()
        .copied()
        .filter(|shape| !coverage.contains_key(shape))
        .collect()
}

/// One mutation kind every parser must survive with a defined outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    MissingRequiredField,
    WrongFieldType,
    TruncatedStructure,
    UnknownNonUsageField,
    UnknownUsageComponent,
}

/// The expected outcome of applying one mutation to a parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationExpectation {
    /// The record parses successfully (an unknown non-usage field is preserved).
    Parses,
    /// The record quarantines with a specific class.
    Quarantines(QuarantineClass),
    /// The record parses and an unknown usage component survives in the unknown map.
    PreservesUnknownComponent { key: String },
}

/// Applies one mutated input to a parser and asserts the exact parse, preserve, or
/// quarantine outcome. The shared contract suite every parser bead runs.
pub fn assert_mutation_outcome(
    parser: &dyn ParserAdapter,
    mutated: &str,
    location: &SourceLocation,
    expected: MutationExpectation,
) {
    let output = parser.parse(mutated, location);
    match expected {
        MutationExpectation::Parses => {
            assert!(
                !output.events().is_empty(),
                "expected a parsed event, got none"
            );
            assert!(
                output.quarantined().is_empty(),
                "expected no quarantine, got {:?}",
                output.quarantined()
            );
        }
        MutationExpectation::Quarantines(class) => {
            assert!(
                output.events().is_empty(),
                "expected no events, got {:?}",
                output.events()
            );
            assert_eq!(
                output.quarantined().len(),
                1,
                "expected exactly one quarantine record, got {:?}",
                output.quarantined()
            );
            assert_eq!(
                output.quarantined()[0].class(),
                class,
                "expected quarantine class {class:?}, got {:?}",
                output.quarantined()[0].class()
            );
        }
        MutationExpectation::PreservesUnknownComponent { key } => {
            assert!(
                !output.events().is_empty(),
                "expected a parsed event, got none"
            );
            assert!(
                output
                    .events()
                    .iter()
                    .any(|event| event.usage().unknown().contains_key(&key)),
                "expected unknown component {key:?} to survive in the unknown map"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    };
    use crate::evidence::{CoverageCompleteness, EvidenceQuality};

    fn location() -> SourceLocation {
        SourceLocation::new("fixture.transcript", 1)
    }

    fn measured_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageVector {
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(cache_read),
                CacheWriteTokens::new(cache_write),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        )
    }

    /// A minimal reference parser for the contract tests. One record per line,
    /// `key=value` pairs separated by spaces. Known usage keys are `input`, `output`,
    /// `cache_read` and `cache_write`; an unknown key ending in `_tokens` is an unknown
    /// usage component; any other unknown key is a non-usage field that is ignored.
    struct TestParser;

    impl ParserAdapter for TestParser {
        fn parser_version(&self) -> ParserVersion {
            ParserVersion::new("test-1")
        }

        fn input_format_version(&self) -> InputFormatVersion {
            InputFormatVersion::new("test-format-v1")
        }

        fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
            let mut events = Vec::new();
            let mut quarantined = Vec::new();
            for (index, line) in input.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let record_location = SourceLocation::new(
                    location.file().to_string(),
                    location.line() + index as u64,
                );
                match parse_record(line) {
                    Ok(event) => events.push(event),
                    Err(class) => quarantined.push(QuarantineRecord::new(
                        record_location,
                        self.parser_version(),
                        class,
                    )),
                }
            }
            ParseOutput::new(events, quarantined)
        }
    }

    fn parse_record(line: &str) -> Result<NormalizedUsageEvent, QuarantineClass> {
        let mut input = None;
        let mut output = None;
        let mut cache_read = None;
        let mut cache_write = None;
        let mut unknown = BTreeMap::new();

        for pair in line.split_whitespace() {
            let (key, value) = pair
                .split_once('=')
                .ok_or(QuarantineClass::TruncatedStructure)?;
            match key {
                "input" => input = Some(parse_count(value)?),
                "output" => output = Some(parse_count(value)?),
                "cache_read" => cache_read = Some(parse_count(value)?),
                "cache_write" => cache_write = Some(parse_count(value)?),
                other if other.ends_with("_tokens") => {
                    unknown.insert(other.to_string(), TokenCount::new(parse_count(value)?));
                }
                _ => {
                    // An unknown non-usage field: preserved by ignoring it, never
                    // treated as a usage component.
                }
            }
        }

        let input = input.ok_or(QuarantineClass::MissingRequiredField)?;
        let known = KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output.unwrap_or(0)),
            CacheReadTokens::new(cache_read.unwrap_or(0)),
            CacheWriteTokens::new(cache_write.unwrap_or(0)),
        );
        let usage = UsageVector::new(
            known,
            unknown,
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        Ok(NormalizedUsageEvent::new(
            usage,
            EvidenceClassification::Reported,
            Provenance::new(["fixture.transcript".to_string()]),
            ParserVersion::new("test-1"),
        ))
    }

    fn parse_count(value: &str) -> Result<u64, QuarantineClass> {
        value
            .parse::<u64>()
            .map_err(|_| QuarantineClass::WrongFieldType)
    }

    #[test]
    fn an_unrecognized_usage_field_survives_into_the_unknown_component_map() {
        let parser = TestParser;
        let output = parser.parse("input=100 output=50 reasoning_tokens=99", &location());

        assert_eq!(output.quarantined().len(), 0);
        assert_eq!(output.events().len(), 1);
        let event = &output.events()[0];
        assert_eq!(
            event
                .usage()
                .unknown()
                .get("reasoning_tokens")
                .map(|c| c.value()),
            Some(99),
            "the unrecognized usage field must survive under its reported key"
        );
        assert_eq!(event.usage().known().input().value(), 100);
    }

    #[test]
    fn input_that_cannot_be_normalized_is_quarantined_never_dropped() {
        let parser = TestParser;
        let output = parser.parse("input=100\nnot-a-record\ninput=200", &location());

        assert_eq!(output.events().len(), 2, "the good records must survive");
        assert_eq!(
            output.quarantined().len(),
            1,
            "the bad record must quarantine"
        );
        assert_eq!(
            output.quarantined()[0].class(),
            QuarantineClass::TruncatedStructure
        );
        assert_eq!(output.quarantined()[0].location().line(), 2);
    }

    #[test]
    fn a_reconstructed_event_carries_its_estimator_identifier_and_version() {
        let estimator = EstimatorId::new("characters");
        let version = EstimatorVersion::new("v3");
        let classification = EvidenceClassification::Reconstructed {
            estimator: estimator.clone(),
            version: version.clone(),
        };
        let event = NormalizedUsageEvent::new(
            measured_usage(1, 2, 3, 4),
            classification,
            Provenance::new(["fixture.transcript".to_string()]),
            ParserVersion::new("test-1"),
        );

        assert!(event.classification().is_reconstructed());
        assert_eq!(
            event.classification().reconstructed(),
            Some((&estimator, &version)),
            "the estimator identifier and version must survive normalization"
        );
    }

    #[test]
    fn the_classification_distinguishes_the_three_cases() {
        let reported = EvidenceClassification::Reported;
        let derived = EvidenceClassification::Derived;
        let reconstructed = EvidenceClassification::Reconstructed {
            estimator: EstimatorId::new("characters"),
            version: EstimatorVersion::new("v3"),
        };

        assert!(!reported.is_reconstructed());
        assert!(!derived.is_reconstructed());
        assert!(reconstructed.is_reconstructed());
        assert_eq!(reported.reconstructed(), None);
        assert_eq!(derived.reconstructed(), None);
        assert!(reconstructed.reconstructed().is_some());
    }

    #[test]
    fn the_fixture_catalog_is_enumerated_once_with_nine_shapes() {
        assert_eq!(FIXTURE_CATALOG.len(), 9);
        // The list lives here, once: a shape removed from the catalog is caught by
        // this count, and a shape added without a coverage answer is caught by
        // verify_fixture_coverage.
        let mut seen = std::collections::BTreeSet::new();
        for shape in FIXTURE_CATALOG {
            assert!(seen.insert(shape), "catalog shape {shape:?} is duplicated");
        }
    }

    #[test]
    fn fixture_coverage_rejects_an_omitted_shape() {
        let mut coverage = BTreeMap::new();
        for shape in FIXTURE_CATALOG {
            if shape == FixtureShape::FileRotation {
                continue;
            }
            coverage.insert(
                shape,
                FixtureCoverage::Applicable {
                    fixture: format!("{shape:?}.fixture"),
                },
            );
        }

        let missing = verify_fixture_coverage(&coverage);
        assert_eq!(missing, vec![FixtureShape::FileRotation]);
    }

    #[test]
    fn fixture_coverage_accepts_a_not_applicable_rationale() {
        let mut coverage = BTreeMap::new();
        for shape in FIXTURE_CATALOG {
            coverage.insert(
                shape,
                FixtureCoverage::NotApplicable {
                    rationale: "this format has no such shape".to_string(),
                },
            );
        }

        assert!(verify_fixture_coverage(&coverage).is_empty());
    }

    #[test]
    fn the_mutation_catalog_asserts_each_outcome() {
        let parser = TestParser;
        let loc = location();

        // Missing required field quarantines.
        assert_mutation_outcome(
            &parser,
            "output=50",
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::MissingRequiredField),
        );
        // Wrong field type quarantines.
        assert_mutation_outcome(
            &parser,
            "input=abc",
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::WrongFieldType),
        );
        // Truncated structure quarantines.
        assert_mutation_outcome(
            &parser,
            "input",
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::TruncatedStructure),
        );
        // Unknown non-usage field preserves a supported record.
        assert_mutation_outcome(
            &parser,
            "input=10 model=opus",
            &loc,
            MutationExpectation::Parses,
        );
        // Unknown usage component survives in the unknown map.
        assert_mutation_outcome(
            &parser,
            "input=10 reasoning_tokens=99",
            &loc,
            MutationExpectation::PreservesUnknownComponent {
                key: "reasoning_tokens".to_string(),
            },
        );
    }

    #[test]
    fn the_event_carries_all_four_required_parts() {
        let usage = measured_usage(1, 2, 3, 4);
        let classification = EvidenceClassification::Reported;
        let provenance = Provenance::new(["fixture.transcript".to_string()]);
        let parser_version = ParserVersion::new("test-1");

        let event = NormalizedUsageEvent::new(
            usage.clone(),
            classification.clone(),
            provenance.clone(),
            parser_version.clone(),
        );

        assert_eq!(event.usage(), &usage);
        assert_eq!(event.classification(), &classification);
        assert_eq!(event.provenance(), &provenance);
        assert_eq!(event.parser_version(), &parser_version);
    }
}
