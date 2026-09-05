//! Assembly of canonical and legacy spend reports.
//!
//! The command path reads canonical events from the ledger, groups them by the
//! requested dimensions, and records the evidence that qualifies every subtotal.
//! The older in-memory assembler remains for its focused parser and grouping
//! tests while ingest is a separate operation.
//!
//! Two things are never done here. A count is never printed without the ingest
//! summary that qualifies it, because a total with quarantined records behind it is
//! not a total. And an event whose record carried no timestamp is never placed in a
//! day: it is counted as undated and left out of every group.
//!
//! May not depend on:
//! - presentation
//! - calibration, rate cards or meter observations

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::attribution::account_segment::{
    self, AccountEvidenceClass, AccountSegmentTarget, AccountSegmentationInputs, AccountUsageEvent,
};
use crate::attribution::report::{AttributableEvent, attribute_events};
use crate::attribution::segment::SegmentTarget;
use crate::config::Config;
use crate::dedup::deduplicate;
use crate::domain::credits::Credits;
use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::money::Usd;
use crate::domain::provenance::{DerivationId, EvidenceId, QuerySemantics, WitnessId};
use crate::domain::time::{UtcDate, UtcTimestamp, unix_nanos};
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};
use crate::error::Error;
use crate::evidence::{
    ComponentKind, CoverageCompleteness, Derivation, EstimatorId, EvidenceQuality, Provenance,
    RequiredFact,
};
use crate::logging::LogicalName;
use crate::report::models::{
    AccountGroupExplain, AccountMarkerReference, IngestSummary, IngestionGeneration,
    LedgerGeneration, ReportMetadata, SpendDiagnostic, SpendDiagnosticProvenance, SpendGroup,
    SpendGroupCreditsProvenance, SpendGroupProvenance, SpendGroupWindowEquivalentProvenance,
    SpendGrouping, SpendReport, UNKNOWN_ACCOUNT_LABEL, WindowEquivalentDerivation,
};
use crate::report::provenance::{ProvenanceNode, Unit, ValueArithmetic};
use crate::store::cost_model::CostModel;
use crate::store::spend::CanonicalSpendEvent;
use crate::transcripts::{
    DiscoveryError, DiscoveryOptions, NormalizedUsageEvent, ParserAdapter, ParserVersion,
    SourceLocation, discover, parser_for_format,
};
use crate::valuation::{RateBook, ValuationOutcome};

/// The UTC day range a spend report covers: `since` inclusive, `until` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendWindow {
    pub since: UtcDate,
    pub until: UtcDate,
}

/// Whether an `aub spend` caller requested the optional credit dimension and, when
/// it did, the immutable model witness resolved from the repository.
#[derive(Debug, Clone, Copy)]
pub enum CreditReporting<'model> {
    NotRequested,
    Active(&'model CostModel),
    NoActiveModel,
}

/// Resolves the optional calibrated window-equivalent dimension for one spend
/// group. The report layer supplies only the group stratum and qualified credits;
/// resolving a calibration remains outside this module so report assembly cannot
/// reach into calibration persistence or invent a coefficient.
pub trait WindowEquivalentResolver {
    fn window_semantic_key(&self) -> &str;

    fn resolve(
        &self,
        account: Option<&str>,
        provider: Option<&str>,
        credits: Option<&Derivation<Credits>>,
    ) -> Result<WindowEquivalentDerivation, Error>;
}

impl SpendWindow {
    /// `days` whole UTC days starting at `since`. Zero days is a usage error: an
    /// empty window would report zero events and a zero must come from evidence.
    pub fn starting(since: UtcDate, days: i64) -> Result<Self, Error> {
        if days < 1 {
            return Err(Error::Usage("--days must be at least 1".into()));
        }
        Ok(Self {
            since,
            until: since.plus_days(days),
        })
    }

    fn contains(&self, at: UtcTimestamp) -> bool {
        at >= self.since.start() && at < self.until.start()
    }
}

/// Reads every configured transcript source and assembles the spend report.
///
/// A source root that does not exist or a source without a known `format` is a
/// usage error, because both are configuration the operator wrote. A file that
/// cannot be read is named in the summary and the remaining files still count; the
/// caller decides the exit class from `unreadable_files`.
pub fn assemble(
    config: &Config,
    window: SpendWindow,
    generated_at: UtcTimestamp,
) -> Result<SpendReport, Error> {
    if config.transcripts.is_empty() {
        return Err(Error::Usage(
            "no [[transcripts]] sources are configured; spend has nothing to read".into(),
        ));
    }
    let parsers = parsers_for(config)?;
    let cumulative_parsers: BTreeSet<ParserVersion> = parsers
        .values()
        .filter(|parser| parser.reports_cumulative())
        .map(|parser| parser.parser_version())
        .collect();
    let discovered =
        discover(&config.transcripts, &DiscoveryOptions::default()).map_err(discovery_error)?;

    let mut summary = IngestSummary::default();
    let mut events: Vec<(String, NormalizedUsageEvent)> = Vec::new();
    for source in &discovered {
        let parser = parsers
            .get(source.source.as_str())
            .expect("every discovered source was configured");
        for file in &source.files {
            if modified_before(file, window.since.start()) {
                summary.files_skipped_before_window += 1;
                continue;
            }
            // A database source is parsed whole from its file, the same seam
            // the ingest pass reads through, so the report never parses a
            // SQLite file as text.
            let output = if parser.is_database_source() {
                parser
                    .parse_database_file(file, &SourceLocation::new(file.display().to_string(), 1))
            } else {
                let Ok(contents) = std::fs::read_to_string(file) else {
                    summary.unreadable_files.push(file.display().to_string());
                    continue;
                };
                parser.parse(
                    &contents,
                    &SourceLocation::new(file.display().to_string(), 1),
                )
            };
            summary.files_read += 1;
            for record in output.quarantined() {
                *summary
                    .quarantined_by_class
                    .entry(record.class().name().to_string())
                    .or_insert(0) += 1;
            }
            events.extend(
                output
                    .events()
                    .iter()
                    .cloned()
                    .map(|event| (source.source.clone(), event)),
            );
        }
    }

    let groups = group_events(events, window, &mut summary, &cumulative_parsers);
    let metadata = ReportMetadata::new(generated_at, generated_at, LedgerGeneration::new(0), None);
    let (groups, provenance): (Vec<SpendGroup>, Vec<SpendGroupProvenance>) =
        groups.into_iter().unzip();
    Ok(SpendReport::new(
        metadata,
        window.since,
        window.until,
        groups,
        provenance,
        summary,
    ))
}

fn parsers_for(config: &Config) -> Result<BTreeMap<&str, Box<dyn ParserAdapter>>, Error> {
    let mut parsers = BTreeMap::new();
    for source in &config.transcripts {
        let format = source.format.as_deref().ok_or_else(|| {
            Error::Usage(format!(
                "transcript source {} declares no format; set format to one of {}",
                source.name,
                crate::transcripts::KNOWN_FORMATS.join(", ")
            ))
        })?;
        let parser = parser_for_format(format).ok_or_else(|| {
            Error::Usage(format!(
                "transcript source {} declares unknown format {format}; known formats are {}",
                source.name,
                crate::transcripts::KNOWN_FORMATS.join(", ")
            ))
        })?;
        parsers.insert(source.name.as_str(), parser);
    }
    Ok(parsers)
}

fn discovery_error(error: DiscoveryError) -> Error {
    match error {
        DiscoveryError::RootMissing { source, path } => Error::Usage(format!(
            "transcript source {source}: root {} is not a directory",
            path.display()
        )),
        DiscoveryError::UnsupportedPattern { source, pattern } => Error::Usage(format!(
            "transcript source {source}: pattern {pattern} is not supported"
        )),
        DiscoveryError::UnreadableDirectory { path } => Error::IngestIncomplete(format!(
            "transcript directory {} could not be read",
            path.display()
        )),
    }
}

/// True when the file's modification time is known and precedes `instant`. A file
/// whose mtime cannot be read is not skipped: skipping is an optimisation and an
/// unknown mtime must not become a silent omission.
fn modified_before(file: &Path, instant: UtcTimestamp) -> bool {
    let Ok(metadata) = std::fs::metadata(file) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let nanos = unix_nanos(modified);
    i64::try_from(nanos).is_ok_and(|nanos| nanos < instant.unix_nanos())
}

/// A key that sorts by day then source, so the rendering reads chronologically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    day: UtcDate,
    source: String,
}

struct GroupAccumulator {
    usage: Option<UsageVector>,
    members: BTreeSet<EvidenceId>,
    files: BTreeSet<String>,
    event_count: u64,
}

fn group_events(
    events: Vec<(String, NormalizedUsageEvent)>,
    window: SpendWindow,
    summary: &mut IngestSummary,
    cumulative_parsers: &BTreeSet<ParserVersion>,
) -> Vec<(SpendGroup, SpendGroupProvenance)> {
    let (sources, events): (Vec<String>, Vec<NormalizedUsageEvent>) = events.into_iter().unzip();
    // Deduplication runs over the whole batch, not per source, because one
    // message replayed across two configured roots is still one message. The
    // source labels are re-attached from the file each canonical event names.
    let source_of_file = source_by_file(&sources, &events);
    let deduplicated = deduplicate(events);
    summary.replayed_occurrences = deduplicated.replayed_occurrences;
    summary.collisions = deduplicated.collisions;
    summary.without_identity = deduplicated.without_identity;

    // Cumulative sources report totals so far, so their surviving events are
    // points of one series per session, not independent consumption: the
    // pipeline orders each series and differences it into deltas, rejecting
    // counter resets with the affected interval named (aub-lqe.9). The deltas
    // replace the source's events before grouping sums anything, and each
    // reset marks the group it would have joined partially covered.
    let (canonical, cumulative) = crate::dedup::cumulative::derive_cumulative_deltas(
        deduplicated.canonical,
        &|version: &ParserVersion| cumulative_parsers.contains(version),
    );
    let mut reset_exclusions: BTreeMap<GroupKey, BTreeSet<ComponentKind>> = BTreeMap::new();
    for reset in &cumulative.resets {
        if let Some(occurred_at) = reset.rejected.occurred_at() {
            let file = reset.rejected.source_file().to_string();
            let source = source_of_file
                .get(&file)
                .cloned()
                .unwrap_or_else(|| "unknown-source".to_string());
            let key = GroupKey {
                day: occurred_at.utc_date(),
                source,
            };
            reset_exclusions
                .entry(key)
                .or_default()
                .extend(reset.missing.iter().cloned());
        }
    }

    // A heuristic-key collision quarantined a pair, so the groups the pair's
    // occurrences would have joined are missing everything the pair would have
    // carried; the groups say so rather than reading as complete (aub-lqe.10).
    // An occurrence with no timestamp joins no group, like any undated event.
    let mut collision_exclusions: BTreeMap<GroupKey, BTreeSet<ComponentKind>> = BTreeMap::new();
    for collision in &deduplicated.heuristic_collisions {
        for occurrence in collision.occurrences() {
            let Some(occurred_at) = occurrence.occurred_at() else {
                continue;
            };
            let file = occurrence.source_file().to_string();
            let source = source_of_file
                .get(&file)
                .cloned()
                .unwrap_or_else(|| "unknown-source".to_string());
            collision_exclusions
                .entry(GroupKey {
                    day: occurred_at.utc_date(),
                    source,
                })
                .or_default()
                .extend(collision.missing_components());
        }
    }

    let mut groups: BTreeMap<GroupKey, GroupAccumulator> = BTreeMap::new();
    for event in canonical {
        let Some(occurred_at) = event.occurred_at() else {
            summary.undated_events += 1;
            continue;
        };
        if !window.contains(occurred_at) {
            summary.events_outside_window += 1;
            continue;
        }
        summary.events_in_window += 1;
        let file = event.source_file().to_string();
        let source = source_of_file
            .get(&file)
            .cloned()
            .unwrap_or_else(|| "unknown-source".to_string());
        let key = GroupKey {
            day: occurred_at.utc_date(),
            source,
        };
        let group = groups.entry(key).or_insert_with(|| GroupAccumulator {
            usage: None,
            members: BTreeSet::new(),
            files: BTreeSet::new(),
            event_count: 0,
        });
        group.usage = Some(match group.usage.take() {
            None => event.usage().clone(),
            Some(current) => add_usage(&current, event.usage()),
        });
        group
            .members
            .insert(EvidenceId::new(evidence_id(&event, &file)));
        group.files.insert(file);
        group.event_count += 1;
    }

    groups
        .into_iter()
        .map(|(key, group)| {
            finish_group(key, group, window, &reset_exclusions, &collision_exclusions)
        })
        .collect()
}

fn source_by_file(sources: &[String], events: &[NormalizedUsageEvent]) -> BTreeMap<String, String> {
    sources
        .iter()
        .zip(events)
        .map(|(source, event)| (event.source_file().to_string(), source.clone()))
        .collect()
}

fn evidence_id(event: &NormalizedUsageEvent, file: &str) -> String {
    match event.strong_identity() {
        Some(identity) => identity.to_string(),
        None => format!(
            "{file}#{}",
            event.occurred_at().map_or(0, UtcTimestamp::unix_nanos)
        ),
    }
}

fn add_usage(left: &UsageVector, right: &UsageVector) -> UsageVector {
    let mut unknown: BTreeMap<String, TokenCount> = left.unknown().clone();
    for (key, count) in right.unknown() {
        let entry = unknown.entry(key.clone()).or_insert(TokenCount::new(0));
        *entry = *entry + *count;
    }
    UsageVector::new(
        left.known() + right.known(),
        unknown,
        left.coverage().combine(right.coverage()),
        left.quality().combine(right.quality()),
    )
}

fn finish_group(
    key: GroupKey,
    group: GroupAccumulator,
    window: SpendWindow,
    reset_exclusions: &BTreeMap<GroupKey, BTreeSet<ComponentKind>>,
    collision_exclusions: &BTreeMap<GroupKey, BTreeSet<ComponentKind>>,
) -> (SpendGroup, SpendGroupProvenance) {
    let name = LogicalName::new(format!("{} {}", key.day.iso(), key.source));
    let node = ProvenanceNode::new(
        group.members.iter().cloned(),
        [],
        QuerySemantics::new(
            "day,source",
            format!("{}..{}", window.since.iso(), window.until.iso()),
        ),
        group.files.len() as u64,
        group.event_count,
        ValueArithmetic::Sum,
    );
    let derivation_id = DerivationId::from_manifest(node.manifest());
    // A group exists only because at least one event landed in it, so the usage
    // is always present; no quantity type has a Default to fall back on, by design.
    let usage = group
        .usage
        .expect("a spend group is created by the first event that lands in it");
    // Excluded material this group would have carried: a counter reset's
    // rejected delta and a quarantined heuristic-collision pair both leave
    // components missing from the total, and the group's coverage names them
    // rather than reading as complete (aub-lqe.9, aub-lqe.10).
    let mut excluded: BTreeSet<ComponentKind> = BTreeSet::new();
    if let Some(kinds) = reset_exclusions.get(&key) {
        excluded.extend(kinds.iter().cloned());
    }
    if let Some(kinds) = collision_exclusions.get(&key) {
        excluded.extend(kinds.iter().cloned());
    }
    let usage = if excluded.is_empty() {
        usage
    } else {
        let merged = usage
            .coverage()
            .combine(&CoverageCompleteness::Partial { missing: excluded });
        UsageVector::new(
            usage.known(),
            usage.unknown().clone(),
            merged,
            usage.quality().clone(),
        )
    };
    let spend_group = SpendGroup::new(
        name.clone(),
        usage,
        Provenance::new(group.files.iter().cloned()),
        derivation_id,
    );
    (spend_group, SpendGroupProvenance::new(name, node))
}

/// Assembles `aub spend` from the durable canonical ledger. The optional refresh
/// is performed by the CLI before this read; a failed refresh is carried as a
/// qualification while this function still reports the last committed subtotal.
///
/// The optional dimensions arrive as separate arguments rather than a bundle: each
/// is resolved by the CLI from a different repository and passed as its own witness,
/// which is the property the fail-closed rule rests on.
#[allow(clippy::too_many_arguments)]
pub fn assemble_canonical(
    conn: &rusqlite::Connection,
    window: SpendWindow,
    generated_at: UtcTimestamp,
    grouping: Vec<SpendGrouping>,
    refresh_attempted: bool,
    refresh_failure: Option<String>,
    rate_book: Option<&RateBook>,
    credit_reporting: CreditReporting<'_>,
) -> Result<SpendReport, Error> {
    assemble_canonical_with_window_equivalent(
        conn,
        window,
        generated_at,
        grouping,
        refresh_attempted,
        refresh_failure,
        rate_book,
        credit_reporting,
        None,
    )
}

/// Assembles a canonical spend report with an optional calibrated
/// window-equivalent conversion. The legacy entry point above deliberately
/// remains unchanged for callers that did not request the new dimension.
#[allow(clippy::too_many_arguments)]
pub fn assemble_canonical_with_window_equivalent(
    conn: &rusqlite::Connection,
    window: SpendWindow,
    generated_at: UtcTimestamp,
    grouping: Vec<SpendGrouping>,
    refresh_attempted: bool,
    refresh_failure: Option<String>,
    rate_book: Option<&RateBook>,
    credit_reporting: CreditReporting<'_>,
    window_resolver: Option<&dyn WindowEquivalentResolver>,
) -> Result<SpendReport, Error> {
    let grouping = if grouping.is_empty() {
        vec![SpendGrouping::Day]
    } else {
        grouping
    };
    let events =
        crate::store::spend::canonical_events(conn, window.since.start(), window.until.start())?;
    let diagnostics = crate::store::spend::diagnostics(conn)?;
    let replayed_occurrences = diagnostics.replayed_occurrences;
    let heuristic_identities = diagnostics.heuristic_identities;
    let partial = refresh_failure.is_some() || !diagnostics.quarantined_by_class.is_empty();
    let task_labels = if grouping.contains(&SpendGrouping::Task) {
        task_label_map(conn, &events)?
    } else {
        BTreeMap::new()
    };
    let mut provenance = Vec::new();
    let mut credit_provenance = Vec::new();
    let mut window_provenance = Vec::new();
    let mut account_explain = Vec::new();
    let account_of = if grouping.contains(&SpendGrouping::Account) || window_resolver.is_some() {
        let (map, explain) = account_attribution(conn, &events)?;
        account_explain = explain;
        map
    } else {
        BTreeMap::new()
    };
    let groups = canonical_groups(
        &events,
        &grouping,
        0,
        &mut Vec::new(),
        &window,
        partial,
        &account_of,
        &mut provenance,
        rate_book,
        &mut credit_provenance,
        credit_reporting,
        &task_labels,
        window_resolver,
        &mut window_provenance,
    )?;
    let mut metadata = ReportMetadata::new(
        generated_at,
        generated_at,
        LedgerGeneration::new(crate::store::ledger_generation::current(conn)?.value()),
        Some(IngestionGeneration::new(
            crate::store::ingestion_generation::current(conn)?.value(),
        )),
    );
    let stale_note = if let Some(book) = rate_book {
        metadata = metadata.with_rate_card_version(book.version());
        let stale = book.stale_cards(generated_at.utc_date());
        if !stale.is_empty() {
            let first = &stale[0];
            let due_str = match &first.draft.review_due {
                crate::domain::rate_card::ReviewDuePolicy::On(d) => d.iso(),
                crate::domain::rate_card::ReviewDuePolicy::None => String::new(),
            };
            Some(format!(
                "rate card review is due (configured review-due date {due_str} has passed)"
            ))
        } else {
            None
        }
    } else {
        None
    };
    let ingest = IngestSummary {
        refresh_attempted,
        refresh_failure,
        files_read: 0,
        files_skipped_before_window: 0,
        unreadable_files: Vec::new(),
        quarantined_by_class: diagnostics.quarantined_by_class,
        replayed_occurrences,
        collisions: 0,
        without_identity: 0,
        heuristic_identities,
        undated_events: 0,
        events_outside_window: 0,
        events_in_window: events.len() as u64,
    };
    let diagnostic_members = events
        .iter()
        .map(|event| EvidenceId::new(event.canonical_id.clone()))
        .collect::<Vec<_>>();
    let diagnostic_source_count = events
        .iter()
        .flat_map(|event| event.sources.iter())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let diagnostic_node = |grouping: &str, count: u64| {
        ProvenanceNode::new(
            diagnostic_members.clone(),
            [],
            QuerySemantics::new(
                grouping,
                format!("{}..{}", window.since.iso(), window.until.iso()),
            ),
            diagnostic_source_count,
            count,
            ValueArithmetic::Count,
        )
    };
    Ok(SpendReport::new(
        metadata,
        window.since,
        window.until,
        groups,
        provenance,
        ingest,
    )
    .with_stale_rate_card_note(stale_note)
    .with_grouping(grouping)
    .with_account_explain(account_explain)
    .with_window_equivalent_window(
        window_resolver.map(|resolver| resolver.window_semantic_key().to_string()),
    )
    .with_credit_model(match credit_reporting {
        CreditReporting::Active(model) => Some(model.id().clone()),
        CreditReporting::NotRequested | CreditReporting::NoActiveModel => None,
    })
    .with_credit_provenance(credit_provenance)
    .with_window_equivalent_provenance(window_provenance)
    .with_diagnostics(vec![
        SpendDiagnosticProvenance {
            diagnostic: SpendDiagnostic::CanonicalRecords,
            node: diagnostic_node("canonical_records", events.len() as u64),
        },
        SpendDiagnosticProvenance {
            diagnostic: SpendDiagnostic::ReplayedOccurrences,
            node: diagnostic_node("replayed_occurrences", replayed_occurrences),
        },
        SpendDiagnosticProvenance {
            diagnostic: SpendDiagnostic::HeuristicIdentities,
            node: diagnostic_node("heuristic_identities", heuristic_identities),
        },
    ]))
}

#[allow(clippy::too_many_arguments)]
fn canonical_groups(
    events: &[CanonicalSpendEvent],
    grouping: &[SpendGrouping],
    depth: usize,
    path: &mut Vec<String>,
    window: &SpendWindow,
    partial: bool,
    account_of: &BTreeMap<String, String>,
    provenance: &mut Vec<SpendGroupProvenance>,
    rate_book: Option<&RateBook>,
    credit_provenance: &mut Vec<SpendGroupCreditsProvenance>,
    credit_reporting: CreditReporting<'_>,
    task_labels: &BTreeMap<String, String>,
    window_resolver: Option<&dyn WindowEquivalentResolver>,
    window_provenance: &mut Vec<SpendGroupWindowEquivalentProvenance>,
) -> Result<Vec<SpendGroup>, Error> {
    let Some(dimension) = grouping.get(depth).copied() else {
        return Ok(Vec::new());
    };
    let mut by_key: BTreeMap<String, Vec<&CanonicalSpendEvent>> = BTreeMap::new();
    for event in events {
        by_key
            .entry(group_value(event, dimension, task_labels, account_of))
            .or_default()
            .push(event);
    }
    by_key
        .into_iter()
        .map(|(value, members)| -> Result<SpendGroup, Error> {
            path.push(format!("{}={value}", dimension.as_str()));
            let key = LogicalName::new(path.join(" / "));
            let usage = canonical_usage(&members, partial);
            let sources = members
                .iter()
                .flat_map(|event| event.sources.iter().cloned())
                .collect::<BTreeSet<_>>();
            let valuation = rate_book.map(|book| value_events(&members, book));
            let mut witnesses = Vec::new();
            if let Some(book) = rate_book
                && let Some(rc_id) = book.version()
            {
                witnesses.push(WitnessId::RateCard(rc_id));
            }
            let node = ProvenanceNode::new(
                members
                    .iter()
                    .map(|event| EvidenceId::new(event.canonical_id.clone())),
                witnesses,
                QuerySemantics::new(
                    grouping[..=depth]
                        .iter()
                        .map(|dimension| dimension.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    format!("{}..{}", window.since.iso(), window.until.iso()),
                ),
                sources.len() as u64,
                members.len() as u64,
                ValueArithmetic::Sum,
            );
            let derivation_id = DerivationId::from_manifest(node.manifest());
            provenance.push(SpendGroupProvenance::new(key.clone(), node));
            let credits = credit_derivation(credit_reporting, &usage);
            if let CreditReporting::Active(model) = credit_reporting {
                let credit_node = ProvenanceNode::new(
                    members
                        .iter()
                        .map(|event| EvidenceId::new(event.canonical_id.clone())),
                    [WitnessId::CostModel(model.id().clone())],
                    QuerySemantics::new(
                        grouping[..=depth]
                            .iter()
                            .map(|dimension| dimension.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        format!("{}..{}", window.since.iso(), window.until.iso()),
                    ),
                    sources.len() as u64,
                    members.len() as u64,
                    ValueArithmetic::Converted {
                        from: Unit::Tokens,
                        to: Unit::Credits,
                    },
                );
                credit_provenance.push(SpendGroupCreditsProvenance::new(key.clone(), credit_node));
            }
            let account = uniform_account(&members, account_of);
            let provider = uniform_provider(&members);
            let window_equivalent = window_resolver
                .map(|resolver| resolver.resolve(account, provider.as_deref(), credits.as_ref()))
                .transpose()?;
            if let Some(result) = &window_equivalent {
                let mut window_witnesses = Vec::new();
                if let CreditReporting::Active(model) = credit_reporting {
                    window_witnesses.push(WitnessId::CostModel(model.id().clone()));
                }
                if let Some(calibration_id) = result.calibration_id() {
                    window_witnesses.push(WitnessId::WindowCalibration(calibration_id.clone()));
                }
                let window_node = ProvenanceNode::new(
                    members
                        .iter()
                        .map(|event| EvidenceId::new(event.canonical_id.clone())),
                    window_witnesses,
                    QuerySemantics::new(
                        grouping[..=depth]
                            .iter()
                            .map(|dimension| dimension.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        format!("{}..{}", window.since.iso(), window.until.iso()),
                    ),
                    sources.len() as u64,
                    members.len() as u64,
                    ValueArithmetic::Converted {
                        from: Unit::Credits,
                        to: Unit::PercentagePoints,
                    },
                );
                window_provenance.push(SpendGroupWindowEquivalentProvenance::new(
                    key.clone(),
                    window_node,
                ));
            }
            let children = canonical_groups(
                &members.into_iter().cloned().collect::<Vec<_>>(),
                grouping,
                depth + 1,
                path,
                window,
                partial,
                account_of,
                provenance,
                rate_book,
                credit_provenance,
                credit_reporting,
                task_labels,
                window_resolver,
                window_provenance,
            )?;
            path.pop();
            let group = SpendGroup::new(key, usage, Provenance::new(sources), derivation_id)
                .with_valuation(valuation)
                .with_children(children);
            let group = match credits {
                Some(credits) => group.with_credits(credits),
                None => group,
            };
            let group = match window_equivalent {
                Some(window_equivalent) => group.with_window_equivalent(window_equivalent),
                None => group,
            };
            // The unknown-account group is usage no marker could justify: its
            // coverage says so on every subtotal it holds, rather than reading
            // as a complete account (aub-mgv.4, PLAN.md 19.2).
            if dimension == SpendGrouping::Account && value == UNKNOWN_ACCOUNT_LABEL {
                Ok(qualify_tree_unattributed(group))
            } else {
                Ok(group)
            }
        })
        .collect()
}

/// Marks a group and every descendant partial on the account dimension: the
/// account attribution is a required input the unknown-account bucket does not
/// have. This never touches the token quantities, only the coverage they
/// carry.
fn qualify_tree_unattributed(mut group: SpendGroup) -> SpendGroup {
    let merged = group
        .usage
        .coverage()
        .combine(&CoverageCompleteness::partial([ComponentKind::new(
            "account",
        )]));
    group.usage = UsageVector::new(
        group.usage.known(),
        group.usage.unknown().clone(),
        merged,
        group.usage.quality().clone(),
    );
    group.children = group
        .children
        .into_iter()
        .map(qualify_tree_unattributed)
        .collect();
    group
}

/// Resolves every in-window event to an account through the marker-interval
/// segmentation, returning the per-event account label and, per distinct
/// account, the marker evidence that placed it. The report never inspects a
/// marker itself: [`account_segment::assign`] owns the decision (aub-mgv.4).
fn account_attribution(
    conn: &rusqlite::Connection,
    events: &[CanonicalSpendEvent],
) -> Result<(BTreeMap<String, String>, Vec<AccountGroupExplain>), Error> {
    // Bucket event indices by the session that owns them, keeping the order
    // canonical_events returned so assign()'s answer maps back by position. An
    // event with no session identity has no marker timeline and goes straight
    // to the unknown-account bucket.
    let mut by_session: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let mut sessionless: Vec<usize> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match (&event.session_source, &event.session_native) {
            (Some(source), Some(native)) => by_session
                .entry((source.clone(), native.clone()))
                .or_default()
                .push(index),
            _ => sessionless.push(index),
        }
    }

    let mut account_of: BTreeMap<String, String> = BTreeMap::new();
    let mut per_account: BTreeMap<
        String,
        (AccountEvidenceClass, BTreeSet<AccountMarkerReference>),
    > = BTreeMap::new();

    for &index in &sessionless {
        account_of.insert(
            events[index].canonical_id.clone(),
            UNKNOWN_ACCOUNT_LABEL.to_string(),
        );
        per_account
            .entry(UNKNOWN_ACCOUNT_LABEL.to_string())
            .or_insert_with(|| (AccountEvidenceClass::Unattributed, BTreeSet::new()));
    }

    for ((source, native), indices) in &by_session {
        let session_id = SessionId::new(
            SourceNamespace::new(source.clone()),
            NativeSessionId::new(native.clone()),
        );
        let markers = crate::store::session_account_marker::markers_for_session(conn, &session_id)?;
        let usage: Vec<AccountUsageEvent> = indices
            .iter()
            .map(|&i| AccountUsageEvent {
                occurred_at: events[i].occurred_at,
                usage: known_vector(&events[i].components),
            })
            .collect();
        let assigned = account_segment::assign(&AccountSegmentationInputs {
            markers: markers.iter().map(|marker| marker.boundary()).collect(),
            usage,
        });
        for (&event_index, (target, class)) in indices.iter().zip(&assigned) {
            let label = match target {
                AccountSegmentTarget::Account(account) => account.clone(),
                AccountSegmentTarget::UnknownAccount => UNKNOWN_ACCOUNT_LABEL.to_string(),
            };
            account_of.insert(events[event_index].canonical_id.clone(), label.clone());
            let entry = per_account
                .entry(label)
                .or_insert_with(|| (*class, BTreeSet::new()));
            if class.takes_precedence_over(entry.0) {
                entry.0 = *class;
            }
            if let AccountSegmentTarget::Account(account) = target {
                for marker in &markers {
                    let boundary_class = marker.boundary().effective_evidence_class();
                    if marker.logical_account() == account && boundary_class == *class {
                        entry.1.insert(AccountMarkerReference {
                            reference: format!("session_account_marker:{}", marker.id().value()),
                            logical_account: account.clone(),
                            evidence_class: boundary_class,
                        });
                    }
                }
            }
        }
    }

    let account_explain = per_account
        .into_iter()
        .map(|(label, (evidence_class, markers))| AccountGroupExplain {
            key: LogicalName::new(format!("account={label}")),
            evidence_class,
            markers: markers.into_iter().collect(),
        })
        .collect();
    Ok((account_of, account_explain))
}

/// The four known token kinds of a canonical event as a [`KnownTokenVector`],
/// the shape [`account_segment`] segments over.
fn known_vector(components: &BTreeMap<String, u64>) -> KnownTokenVector {
    KnownTokenVector::new(
        InputTokens::new(components.get("input").copied().unwrap_or(0)),
        OutputTokens::new(components.get("output").copied().unwrap_or(0)),
        CacheReadTokens::new(components.get("cache_read").copied().unwrap_or(0)),
        CacheWriteTokens::new(components.get("cache_write").copied().unwrap_or(0)),
    )
}

fn value_events(events: &[&CanonicalSpendEvent], book: &RateBook) -> ValuationOutcome<Usd> {
    let mut outcome: Option<ValuationOutcome<Usd>> = None;
    for event in events {
        let usage = canonical_usage(&[event], false);
        let vendor = event.vendor.as_deref().unwrap_or("unknown");
        let model = event.model.as_deref().unwrap_or("unknown");
        let event_val = crate::valuation::value_usage_vector::<Usd>(
            book,
            vendor,
            model,
            event.occurred_at.utc_date(),
            &usage,
        );
        outcome = match outcome {
            Some(prev) => Some(prev.combine(event_val)),
            None => Some(event_val),
        };
    }
    outcome.unwrap_or_else(|| {
        ValuationOutcome::Complete(crate::valuation::ApiListPriceEquivalent::new(
            crate::domain::money::Money::<Usd>::from_micros(0),
        ))
    })
}

/// The credit derivation for one spend group, or `None` when the caller did not ask
/// for credits at all. A request that finds no active model still produces a
/// derivation, so the refusal names the missing fact instead of reading as "not
/// requested".
fn credit_derivation(
    reporting: CreditReporting<'_>,
    usage: &UsageVector,
) -> Option<Derivation<Credits>> {
    match reporting {
        CreditReporting::NotRequested => None,
        CreditReporting::Active(model) => Some(crate::cost_model::convert(model, usage)),
        CreditReporting::NoActiveModel => Some(
            Derivation::unavailable(
                [RequiredFact::new("active cost model")],
                Provenance::new(["cost-model:unavailable".to_string()]),
            )
            .expect("the active cost model is a named missing fact"),
        ),
    }
}

fn group_value(
    event: &CanonicalSpendEvent,
    grouping: SpendGrouping,
    task_labels: &BTreeMap<String, String>,
    account_of: &BTreeMap<String, String>,
) -> String {
    match grouping {
        SpendGrouping::Day => event.occurred_at.utc_date().iso(),
        SpendGrouping::Session => event.session.clone(),
        SpendGrouping::Project => event.project.clone(),
        SpendGrouping::Repository => event.repository.clone(),
        SpendGrouping::Task => task_labels
            .get(&event.canonical_id)
            .cloned()
            .unwrap_or_else(|| "overhead:missing_timestamp".to_string()),
        // Resolved before grouping by account_attribution(); an event missing
        // from the map never received an account and is unattributed.
        SpendGrouping::Account => account_of
            .get(&event.canonical_id)
            .cloned()
            .unwrap_or_else(|| UNKNOWN_ACCOUNT_LABEL.to_string()),
    }
}

fn uniform_account<'a>(
    members: &[&CanonicalSpendEvent],
    account_of: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    let mut accounts = members.iter().map(|event| {
        account_of
            .get(&event.canonical_id)
            .map(String::as_str)
            .unwrap_or(UNKNOWN_ACCOUNT_LABEL)
    });
    let first = accounts.next()?;
    accounts.all(|account| account == first).then_some(first)
}

fn uniform_provider(members: &[&CanonicalSpendEvent]) -> Option<String> {
    let first = members.first()?.vendor.as_deref()?;
    members
        .iter()
        .all(|event| event.vendor.as_deref() == Some(first))
        .then(|| first.to_string())
}

/// The task-or-overhead label for every event, keyed by its own
/// `canonical_id`, computed by the shared segmentation engine
/// ([`crate::attribution::segment`]) rather than by any grouping logic of
/// this command's own: `--group-by task` reuses exactly the attribution
/// `aub task report` and `aub task overhead` read back (`aub-eu7.4`).
fn task_label_map(
    conn: &rusqlite::Connection,
    events: &[CanonicalSpendEvent],
) -> Result<BTreeMap<String, String>, Error> {
    let boundaries = crate::store::task_event::read_boundaries(conn)?;
    let attributable: Vec<AttributableEvent> = events
        .iter()
        .map(|event| AttributableEvent {
            canonical_id: event.canonical_id.clone(),
            occurred_at: event.occurred_at,
            session_is_mapped: event.session != crate::store::spend::UNKNOWN_SESSION,
            usage: known_vector(&event.components),
        })
        .collect();
    let attributed = attribute_events(boundaries, true, &attributable);
    Ok(attributed
        .into_iter()
        .map(|attribution| {
            (
                attribution.canonical_id,
                task_target_label(&attribution.target),
            )
        })
        .collect())
}

fn task_target_label(target: &SegmentTarget) -> String {
    match target {
        SegmentTarget::Task(task_id) => {
            format!(
                "{}:{}",
                task_id.source().as_str(),
                task_id.native().as_str()
            )
        }
        SegmentTarget::Overhead(reason) => format!("overhead:{}", reason.as_str()),
    }
}

/// Aggregates known and unknown token components, coverage and evidence
/// quality across a set of canonical events. `pub(crate)` so `report::task`
/// reuses the exact same aggregation `aub spend` reports, rather than a
/// second copy that could silently drift from it.
pub(crate) fn canonical_usage(events: &[&CanonicalSpendEvent], partial: bool) -> UsageVector {
    let mut components = BTreeMap::<String, u64>::new();
    let mut quality = EvidenceQuality::Measured;
    for event in events {
        for (kind, count) in &event.components {
            *components.entry(kind.clone()).or_insert(0) += count;
        }
        let event_quality = if event.evidence_kind == "reported" {
            EvidenceQuality::Measured
        } else {
            EvidenceQuality::estimated([EstimatorId::new(event.evidence_kind.clone())], None)
        };
        quality = quality.combine(&event_quality);
    }
    let known = KnownTokenVector::new(
        InputTokens::new(components.remove("input").unwrap_or(0)),
        OutputTokens::new(components.remove("output").unwrap_or(0)),
        CacheReadTokens::new(components.remove("cache_read").unwrap_or(0)),
        CacheWriteTokens::new(components.remove("cache_write").unwrap_or(0)),
    );
    let coverage = if partial {
        CoverageCompleteness::partial([
            ComponentKind::new("input"),
            ComponentKind::new("output"),
            ComponentKind::new("cache_read"),
            ComponentKind::new("cache_write"),
        ])
    } else {
        CoverageCompleteness::Complete
    };
    UsageVector::new(
        known,
        components
            .into_iter()
            .map(|(kind, count)| (kind, TokenCount::new(count)))
            .collect(),
        coverage,
        quality,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TranscriptConfig;
    use crate::domain::ids::SourceNamespace;
    use crate::domain::interval::Interval;
    use crate::domain::provenance::WindowCalibrationId;
    use crate::domain::quota::PercentagePoints;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::sessions::{ProjectKey, RepositoryKey};
    use crate::store::connection::{AccessMode, PragmaPolicy};
    use crate::store::session::{NewSession, insert_session};
    use crate::store::usage_component::insert_components;
    use crate::store::usage_event::{NewUsageEvent, insert_event};
    use crate::store::usage_occurrence::{NewUsageOccurrence, insert_occurrence};
    use crate::transcripts::ParserVersion;
    use std::fs;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aub-spend-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn source(name: &str, root: &Path, format: &str) -> TranscriptConfig {
        TranscriptConfig {
            name: name.to_string(),
            root: root.to_path_buf(),
            pattern: "**/*.jsonl".to_string(),
            format: Some(format.to_string()),
            usage_evidence: None,
        }
    }

    fn config_with(transcripts: Vec<TranscriptConfig>) -> Config {
        let (mut config, _) = crate::config::resolve(
            &crate::config::Overrides::new(),
            &crate::config::FakeEnv::new().set("HOME", "/tmp/synthetic-home"),
            None,
            "/test/aub.toml",
        )
        .unwrap();
        config.transcripts = transcripts;
        config
    }

    fn claude_line(id: &str, timestamp: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{timestamp}","sessionId":"s1","message":{{"id":"{id}","usage":{{"input_tokens":10,"output_tokens":{output},"cache_read_input_tokens":5,"cache_creation_input_tokens":1,"service_tier":"standard"}}}}}}"#
        )
    }

    fn window(since: &str, days: i64) -> SpendWindow {
        SpendWindow::starting(UtcDate::parse(since).unwrap(), days).unwrap()
    }

    fn now() -> UtcTimestamp {
        UtcTimestamp::parse_rfc3339("2026-08-30T12:00:00Z").unwrap()
    }

    fn canonical_conn(tag: &str) -> (PathBuf, rusqlite::Connection) {
        let root = scratch(tag);
        let mut conn = crate::store::connection::open(
            &root.join("ledger.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(100),
            },
        )
        .unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (root, conn)
    }

    fn seed_canonical(
        conn: &rusqlite::Connection,
        id: &str,
        timestamp: i64,
        session: &str,
        evidence_kind: &str,
        components: &[(&str, u64)],
    ) {
        let event = insert_event(
            conn,
            &NewUsageEvent {
                canonical_event_id: id,
                session_id: Some(session),
                event_timestamp: Some(UtcTimestamp::from_unix_nanos(timestamp)),
                model_id: None,
                evidence_kind,
                source_provenance: "fixture.jsonl",
                parser_version: "fixture-v1",
                created_at: UtcTimestamp::from_unix_nanos(timestamp),
            },
        )
        .unwrap();
        insert_components(conn, event, components).unwrap();
        let namespace = SourceNamespace::new("fixture");
        let version = ParserVersion::new("fixture-v1");
        insert_occurrence(
            conn,
            &NewUsageOccurrence {
                source_namespace: &namespace,
                native_event_id: Some(id),
                parser_version: &version,
                heuristic_key: None,
                source_file: "fixture.jsonl",
                occurred_at_nanos: Some(timestamp),
                event_id: Some(event),
                transcript_file_id: None,
                source_location: None,
                canonical_fingerprint: None,
                identity_strength: None,
                heuristic_algorithm_version: None,
                canonical_payload_digest: None,
            },
        )
        .unwrap();
    }

    fn seed_session(conn: &rusqlite::Connection, name: &str) {
        insert_session(
            conn,
            &NewSession {
                source: SourceNamespace::new("fixture"),
                native_session_id: crate::domain::ids::NativeSessionId::new(name),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: ProjectKey::new("project-a"),
                repository_key: RepositoryKey::new("repository-a"),
                run_id: None,
            },
        )
        .unwrap();
    }

    fn seed_marker(
        conn: &rusqlite::Connection,
        native: &str,
        account: &str,
        observed_nanos: i64,
        designation: crate::store::session_account_marker::EvidenceDesignation,
    ) {
        crate::store::session_account_marker::insert_marker(
            conn,
            &crate::store::session_account_marker::NewSessionAccountMarker {
                session_id: SessionId::new(
                    SourceNamespace::new("fixture"),
                    crate::domain::ids::NativeSessionId::new(native),
                ),
                observed_at: UtcTimestamp::from_unix_nanos(observed_nanos),
                source_ordering_key: None,
                logical_account: account.to_owned(),
                resolved_account_id: None,
                marker_source: crate::store::session_account_marker::MarkerSource::new("hook"),
                run_id: None,
                evidence_designation: designation,
            },
        )
        .unwrap();
    }

    /// `--group-by account` sums per account, keeps the unknown-account bucket
    /// as its own group, carries the marker evidence class per group, and does
    /// not decide attribution itself: its buckets equal a direct
    /// `account_segment::segment` over the same markers and usage.
    #[test]
    fn account_grouping_delegates_to_segmentation_and_keeps_the_unknown_bucket() {
        use crate::store::session_account_marker::EvidenceDesignation;

        let (_root, conn) = canonical_conn("account-grouping");
        seed_session(&conn, "s1");
        seed_session(&conn, "s2");
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        // s1: marker at day+50 names "work"; one event before it (unknown), one after (work).
        seed_marker(
            &conn,
            "s1",
            "work",
            day + 50,
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        seed_canonical(&conn, "e1", day + 10, "s1", "reported", &[("input", 3)]);
        seed_canonical(&conn, "e2", day + 90, "s1", "reported", &[("input", 7)]);
        // s2: marker from the start names "research".
        seed_marker(
            &conn,
            "s2",
            "research",
            day + 1,
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        seed_canonical(&conn, "e3", day + 20, "s2", "reported", &[("input", 5)]);
        crate::store::ingestion_generation::advance(&conn).unwrap();

        let report = assemble_canonical(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![SpendGrouping::Account],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();

        let by_key: BTreeMap<&str, &SpendGroup> =
            report.groups.iter().map(|g| (g.key.as_str(), g)).collect();
        assert_eq!(by_key["account=work"].usage.known().input().value(), 7);
        assert_eq!(by_key["account=research"].usage.known().input().value(), 5);
        assert_eq!(
            by_key["account=unknown-account"]
                .usage
                .known()
                .input()
                .value(),
            3,
            "the pre-marker event is its own group, never merged into work"
        );

        // One group partial while others are complete: the unknown-account
        // bucket is partial on the account dimension, the attributed ones are not.
        assert!(
            by_key["account=unknown-account"]
                .usage
                .coverage()
                .missing()
                .is_some()
        );
        assert!(by_key["account=work"].usage.coverage().missing().is_none());
        assert!(
            by_key["account=research"]
                .usage
                .coverage()
                .missing()
                .is_none()
        );

        // Evidence class travels per group.
        let explain: BTreeMap<&str, &AccountGroupExplain> = report
            .account_explain
            .iter()
            .map(|group| (group.key.as_str(), group))
            .collect();
        assert_eq!(
            explain["account=work"].evidence_class,
            AccountEvidenceClass::ExplicitLauncherOrHook
        );
        assert_eq!(
            explain["account=unknown-account"].evidence_class,
            AccountEvidenceClass::Unattributed
        );
        assert!(
            explain["account=unknown-account"].markers.is_empty(),
            "the unknown bucket names no marker"
        );
        assert!(
            explain["account=work"].markers[0]
                .reference
                .starts_with("session_account_marker:"),
            "an attributed account names the marker that placed it"
        );

        // No attribution logic of its own: the report's account buckets equal a
        // direct segmentation over the same inputs.
        let direct_s1 = account_segment::segment(&AccountSegmentationInputs {
            markers: vec![account_segment::AccountMarkerBoundary::explicit(
                "work",
                UtcTimestamp::from_unix_nanos(day + 50),
                None,
            )],
            usage: vec![
                AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(day + 10),
                    usage: known_vector(&[("input".to_string(), 3)].into_iter().collect()),
                },
                AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(day + 90),
                    usage: known_vector(&[("input".to_string(), 7)].into_iter().collect()),
                },
            ],
        });
        assert_eq!(
            direct_s1.account_usage("work").unwrap().input().value(),
            by_key["account=work"].usage.known().input().value()
        );
        assert_eq!(
            direct_s1.unknown_account_usage().unwrap().input().value(),
            by_key["account=unknown-account"]
                .usage
                .known()
                .input()
                .value()
        );

        // Human and JSON explain carry identical marker references and classes.
        let human = crate::presentation::render_spend_report_with_explain(
            &report,
            crate::presentation::ExplainMode::Summary,
        );
        let json = crate::presentation::spend_json_with_explain(
            &report,
            crate::logging::RunId::new(now()),
            crate::presentation::ExplainMode::Summary,
        );
        for group in &report.account_explain {
            assert!(human.contains(group.evidence_class.as_str()));
            assert!(json.contains(group.evidence_class.as_str()));
            for marker in &group.markers {
                assert!(
                    human.contains(&marker.reference),
                    "human explain names {}",
                    marker.reference
                );
                assert!(
                    json.contains(&marker.reference),
                    "json explain names {}",
                    marker.reference
                );
            }
        }
        crate::presentation::validate_spend_report_json(&json).unwrap();
    }

    /// `--group-by account --group-by day` reconciles: each account's day
    /// children sum to its total, and the account totals sum to a plain
    /// `--group-by day` report over the same corpus.
    #[test]
    fn account_grouping_composes_with_day_and_the_totals_reconcile() {
        use crate::store::session_account_marker::EvidenceDesignation;

        let (_root, conn) = canonical_conn("account-compose");
        seed_session(&conn, "s1");
        let day25 = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        let day26 = UtcDate::parse("2026-08-26").unwrap().start().unix_nanos();
        seed_marker(
            &conn,
            "s1",
            "work",
            day25,
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        seed_canonical(
            &conn,
            "e1",
            day25 + 10,
            "s1",
            "reported",
            &[("input", 4), ("output", 1)],
        );
        seed_canonical(
            &conn,
            "e2",
            day26 + 10,
            "s1",
            "reported",
            &[("input", 6), ("output", 2)],
        );
        crate::store::ingestion_generation::advance(&conn).unwrap();

        let composed = assemble_canonical(
            &conn,
            window("2026-08-25", 2),
            now(),
            vec![SpendGrouping::Account, SpendGrouping::Day],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();
        let work = composed
            .groups
            .iter()
            .find(|g| g.key.as_str() == "account=work")
            .expect("work account group");
        let child_input: u64 = work
            .children
            .iter()
            .map(|child| child.usage.known().input().value())
            .sum();
        assert_eq!(child_input, work.usage.known().input().value());

        let account_total: u64 = composed
            .groups
            .iter()
            .map(|g| g.usage.known().input().value())
            .sum();
        let by_day = assemble_canonical(
            &conn,
            window("2026-08-25", 2),
            now(),
            vec![SpendGrouping::Day],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();
        let day_total: u64 = by_day
            .groups
            .iter()
            .map(|g| g.usage.known().input().value())
            .sum();
        assert_eq!(account_total, day_total, "account grouping loses no tokens");
        assert_eq!(day_total, 10);

        // Composed the other way, day then account: a mid-session switch to a
        // second account on day 26 must land under day 26's node, not day 25's.
        // This is sensitive to the recursion carrying the account resolution
        // into the inner dimension.
        seed_marker(
            &conn,
            "s1",
            "research",
            day26,
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        let day_then_account = assemble_canonical(
            &conn,
            window("2026-08-25", 2),
            now(),
            vec![SpendGrouping::Day, SpendGrouping::Account],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();
        let cell = |day: &str, account: &str| -> u64 {
            day_then_account
                .groups
                .iter()
                .find(|g| g.key.as_str() == format!("day={day}"))
                .and_then(|g| {
                    g.children
                        .iter()
                        .find(|c| c.key.as_str() == format!("day={day} / account={account}"))
                })
                .map_or(0, |c| c.usage.known().input().value())
        };
        assert_eq!(cell("2026-08-25", "work"), 4);
        assert_eq!(cell("2026-08-26", "research"), 6);
        assert_eq!(cell("2026-08-26", "work"), 0, "day 26 switched to research");
    }

    #[test]
    fn window_equivalent_splits_mixed_accounts_without_combining_percentage_points() {
        let (_root, conn) = canonical_conn("window-equivalent-strata");
        seed_session(&conn, "s1");
        seed_session(&conn, "s2");
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        use crate::store::session_account_marker::EvidenceDesignation;
        seed_marker(
            &conn,
            "s1",
            "work",
            day,
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        seed_marker(
            &conn,
            "s2",
            "research",
            day,
            EvidenceDesignation::ExplicitLauncherOrHook,
        );
        seed_canonical(&conn, "e1", day + 10, "s1", "reported", &[("input", 100)]);
        seed_canonical(&conn, "e2", day + 20, "s2", "reported", &[("input", 200)]);
        crate::store::ingestion_generation::advance(&conn).unwrap();

        struct FixtureResolver;
        impl WindowEquivalentResolver for FixtureResolver {
            fn window_semantic_key(&self) -> &str {
                "five_hour"
            }

            fn resolve(
                &self,
                account: Option<&str>,
                provider: Option<&str>,
                _credits: Option<&Derivation<Credits>>,
            ) -> Result<WindowEquivalentDerivation, Error> {
                let Some(account) = account else {
                    return Ok(WindowEquivalentDerivation::unavailable(
                        [RequiredFact::new("account attribution")],
                        Provenance::new(["fixture-window:unavailable".to_string()]),
                    )
                    .unwrap());
                };
                assert_eq!(provider, Some("fixture"));
                let (calibration_id, lower, upper) = match account {
                    "work" => ("cal-work-v1", 10, 20),
                    "research" => ("cal-research-v1", 30, 40),
                    other => panic!("unexpected account {other}"),
                };
                Ok(WindowEquivalentDerivation::Available(
                    crate::report::WindowEquivalentValue {
                        interval: Interval::new(
                            PercentagePoints::new(lower).unwrap(),
                            PercentagePoints::new(upper).unwrap(),
                        )
                        .unwrap(),
                        calibration_id: WindowCalibrationId::new(calibration_id),
                        coverage: CoverageCompleteness::Complete,
                        quality: EvidenceQuality::Estimated {
                            methods: [EstimatorId::new(calibration_id.to_string())]
                                .into_iter()
                                .collect(),
                            uncertainty: None,
                        },
                        provenance: Provenance::new([format!(
                            "window-calibration:{calibration_id}"
                        )]),
                    },
                ))
            }
        }

        let model = crate::store::cost_model::anthropic_claude_messages_v1(now());
        let report = assemble_canonical_with_window_equivalent(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![SpendGrouping::Day, SpendGrouping::Account],
            false,
            None,
            None,
            CreditReporting::Active(&model),
            Some(&FixtureResolver),
        )
        .unwrap();

        let day_group = &report.groups[0];
        assert!(matches!(
            day_group.window_equivalent,
            Some(WindowEquivalentDerivation::Unavailable { .. })
        ));
        assert_eq!(day_group.children.len(), 2);
        let child_calibrations: BTreeSet<&str> = day_group
            .children
            .iter()
            .filter_map(|child| child.window_equivalent.as_ref()?.calibration_id())
            .map(WindowCalibrationId::as_str)
            .collect();
        assert_eq!(
            child_calibrations,
            BTreeSet::from(["cal-research-v1", "cal-work-v1"])
        );
    }

    #[test]
    fn canonical_groups_nest_and_reconcile_every_token_kind() {
        let (_root, conn) = canonical_conn("canonical-groups");
        seed_session(&conn, "s1");
        seed_session(&conn, "s2");
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        seed_canonical(
            &conn,
            "e1",
            day + 1,
            "s1",
            "reported",
            &[("input", 2), ("output", 3)],
        );
        seed_canonical(
            &conn,
            "e2",
            day + 2,
            "s2",
            "derived",
            &[("input", 5), ("output", 7)],
        );
        crate::store::ingestion_generation::advance(&conn).unwrap();

        let report = assemble_canonical(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![
                SpendGrouping::Day,
                SpendGrouping::Session,
                SpendGrouping::Project,
                SpendGrouping::Repository,
            ],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();

        assert_eq!(report.metadata.ingestion_generation.unwrap().get(), 1);
        assert_eq!(report.groups.len(), 1);
        let day_group = &report.groups[0];
        assert_eq!(day_group.key.as_str(), "day=2026-08-25");
        assert_eq!(day_group.usage.known().input().value(), 7);
        assert_eq!(day_group.usage.known().output().value(), 10);
        assert_eq!(day_group.children.len(), 2);
        let session_s1 = &day_group.children[0];
        assert_eq!(
            session_s1.key.as_str(),
            "day=2026-08-25 / session=fixture:s1"
        );
        let project_group = &session_s1.children[0];
        assert_eq!(
            project_group.key.as_str(),
            "day=2026-08-25 / session=fixture:s1 / project=project-a"
        );
        let repo_group = &project_group.children[0];
        assert_eq!(
            repo_group.key.as_str(),
            "day=2026-08-25 / session=fixture:s1 / project=project-a / repository=repository-a"
        );
        let children_input: u64 = day_group
            .children
            .iter()
            .map(|group| group.usage.known().input().value())
            .sum();
        let children_output: u64 = day_group
            .children
            .iter()
            .map(|group| group.usage.known().output().value())
            .sum();
        assert_eq!(children_input, day_group.usage.known().input().value());
        assert_eq!(children_output, day_group.usage.known().output().value());
        assert!(matches!(
            day_group.usage.quality(),
            EvidenceQuality::Mixed { .. }
        ));
        let explain = crate::presentation::render_spend_report_with_explain(
            &report,
            crate::presentation::ExplainMode::Summary,
        );
        assert!(explain.contains("spend_canonical_records"));
        assert!(explain.contains("spend_replayed_occurrences"));
        assert!(explain.contains("spend_heuristic_identities"));
    }

    /// `--group-by task` reads the tracker's claim/release timeline and
    /// attributes each canonical event to the task claimed at its own
    /// timestamp, or to a named overhead bucket, without any grouping logic
    /// of this test's own: the assertion is that the two group totals
    /// reconcile exactly to the ungrouped canonical total, which is the
    /// conservation invariant `aub-eu7.2` owns, exercised here through the
    /// spend command's own grouping path.
    #[test]
    fn group_by_task_reconciles_to_canonical_totals_and_labels_by_task_and_overhead() {
        use crate::attribution::{TrackerEventReader, TrackerEventRecord};

        let (_root, conn) = canonical_conn("group-by-task");
        seed_session(&conn, "s1");
        let day_start = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        // Before any claim: lands in the before_first_claim overhead bucket.
        seed_canonical(
            &conn,
            "e1",
            day_start + 1,
            "s1",
            "reported",
            &[("input", 2)],
        );
        // After the claim to T1: lands in task T1.
        let one_hour = 3_600_000_000_000;
        seed_canonical(
            &conn,
            "e2",
            day_start + 2 * one_hour,
            "s1",
            "reported",
            &[("input", 5)],
        );

        struct FixtureReader(Vec<TrackerEventRecord>);
        impl TrackerEventReader for FixtureReader {
            fn read_events(&self) -> Result<Vec<TrackerEventRecord>, Error> {
                Ok(self.0.clone())
            }
        }
        let reader = FixtureReader(vec![TrackerEventRecord {
            upstream_id: 1,
            task_native: "T1".to_string(),
            event_type: "status_changed".to_string(),
            old_value: Some("open".to_string()),
            new_value: Some("in_progress".to_string()),
            occurred_at: "2026-08-25T01:00:00Z".to_string(),
            actor: None,
        }]);
        crate::store::task_event::ingest(&conn, SourceNamespace::new("beads-a"), &reader).unwrap();

        let ungrouped = assemble_canonical(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![SpendGrouping::Day],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();
        let canonical_total = ungrouped.groups[0].usage.known().input().value();
        assert_eq!(canonical_total, 7);

        let by_task = assemble_canonical(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![SpendGrouping::Task],
            false,
            None,
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();
        let grouped_total: u64 = by_task
            .groups
            .iter()
            .map(|group| group.usage.known().input().value())
            .sum();
        assert_eq!(
            grouped_total, canonical_total,
            "task-grouped usage must reconcile to the canonical total"
        );
        let labels: BTreeSet<&str> = by_task
            .groups
            .iter()
            .map(|group| group.key.as_str())
            .collect();
        assert_eq!(
            labels,
            BTreeSet::from(["task=overhead:before_first_claim", "task=beads-a:T1",])
        );
        let overhead_group = by_task
            .groups
            .iter()
            .find(|group| group.key.as_str() == "task=overhead:before_first_claim")
            .unwrap();
        assert_eq!(overhead_group.usage.known().input().value(), 2);
        let task_group = by_task
            .groups
            .iter()
            .find(|group| group.key.as_str() == "task=beads-a:T1")
            .unwrap();
        assert_eq!(task_group.usage.known().input().value(), 5);
    }

    #[test]
    fn a_failed_refresh_qualifies_the_known_canonical_subtotal() {
        let (_root, conn) = canonical_conn("canonical-partial");
        seed_session(&conn, "s1");
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        seed_canonical(&conn, "e1", day + 1, "s1", "reported", &[("input", 9)]);

        let report = assemble_canonical(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![SpendGrouping::Day],
            true,
            Some("refresh failed: fixture unreadable; retained prior subtotal".to_string()),
            None,
            CreditReporting::NotRequested,
        )
        .unwrap();

        assert_eq!(report.groups[0].usage.known().input().value(), 9);
        assert!(report.groups[0].usage.coverage().missing().is_some());
        assert!(crate::presentation::render_spend_report(&report).contains("refresh incomplete"));
        let json = crate::presentation::spend_json(&report, crate::logging::RunId::new(now()));
        assert!(json.contains("\"ingestion_generation\":0"));
        assert!(json.contains("\"refresh_failure\""));
        crate::presentation::validate_spend_report_json(&json).unwrap();
    }

    #[test]
    fn rate_card_version_populates_in_assemble_canonical() {
        let (_root, conn) = canonical_conn("canonical-valuation-ver");
        seed_session(&conn, "s1");
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        seed_canonical(&conn, "e1", day + 1, "s1", "reported", &[("input", 100)]);

        let cards = vec![crate::domain::rate_card::RateCard {
            id: 1,
            imported_at: UtcTimestamp::from_unix_nanos(100),
            draft: crate::domain::rate_card::RateCardDraft {
                vendor: "fixture".to_string(),
                model: "model-1".to_string(),
                token_class: crate::domain::rate_card::TokenClass::Input,
                rate_micros: 3_000_000,
                currency: crate::domain::rate_card::CurrencyCode::Usd,
                billing_basis: crate::domain::rate_card::BillingBasis::PerMillionTokens,
                effective_start: UtcDate::parse("2026-08-01").unwrap(),
                effective_end: None,
                publication: crate::domain::rate_card::Publication {
                    source: None,
                    published_at: None,
                },
                review_due: crate::domain::rate_card::ReviewDuePolicy::None,
            },
        }];
        let book = RateBook::new(cards);
        let report = assemble_canonical(
            &conn,
            window("2026-08-25", 1),
            now(),
            vec![SpendGrouping::Day],
            false,
            None,
            Some(&book),
            CreditReporting::NotRequested,
        )
        .unwrap();

        assert_eq!(
            report
                .metadata
                .rate_card_version
                .as_ref()
                .map(|v| v.as_str()),
            Some("rate-card-2026-08-01")
        );
    }

    /// A replayed message counts once at its final output, a message on the next
    /// day lands in its own group, and the summary says what happened.
    #[test]
    fn replays_collapse_and_days_separate() {
        let root = scratch("replay");
        write(
            &root.join("a.jsonl"),
            &[
                claude_line("m1", "2026-08-25T10:00:00Z", 100),
                claude_line("m1", "2026-08-25T10:00:01Z", 400),
                claude_line("m2", "2026-08-25T23:59:59Z", 7),
                claude_line("m3", "2026-08-26T00:00:00Z", 9),
                r#"{"type":"assistant","message":{"id":"bad","usage":{"input_tokens":"x"}}}"#
                    .to_string(),
            ]
            .join("\n"),
        );
        let config = config_with(vec![source("cc", &root, "claude-code")]);
        let report = assemble(&config, window("2026-08-25", 2), now()).unwrap();

        assert_eq!(report.groups.len(), 2);
        assert_eq!(report.groups[0].key.as_str(), "2026-08-25 cc");
        let day_one = report.groups[0].usage.known();
        assert_eq!(
            day_one.output().value(),
            407,
            "400 from the replay's last snapshot plus 7"
        );
        assert_eq!(
            day_one.input().value(),
            20,
            "two canonical events, not three occurrences"
        );
        assert_eq!(report.groups[1].key.as_str(), "2026-08-26 cc");
        assert_eq!(report.groups[1].usage.known().output().value(), 9);
        assert_eq!(report.ingest.replayed_occurrences, 1);
        assert_eq!(
            report.ingest.quarantined_by_class.get("wrong_field_type"),
            Some(&1)
        );
        assert_eq!(report.ingest.events_in_window, 3);
        assert_eq!(report.ingest.files_read, 1);
        assert!(
            report
                .provenance
                .resolve(&crate::report::ReportField::SpendGroupTokens {
                    key: report.groups[0].key.clone()
                })
                .is_some()
        );
    }

    /// An event outside the window and an event with no timestamp are counted, not
    /// dropped and not placed in a day.
    #[test]
    fn outside_window_and_undated_events_are_counted_not_placed() {
        let root = scratch("window");
        write(
            &root.join("a.jsonl"),
            &[
                claude_line("m1", "2026-08-24T10:00:00Z", 1),
                r#"{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":1,"output_tokens":1}}}"#.to_string(),
                claude_line("m3", "2026-08-25T10:00:00Z", 3),
            ]
            .join("\n"),
        );
        let config = config_with(vec![source("cc", &root, "claude-code")]);
        let report = assemble(&config, window("2026-08-25", 1), now()).unwrap();
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].usage.known().output().value(), 3);
        assert_eq!(report.ingest.events_outside_window, 1);
        assert_eq!(report.ingest.undated_events, 1);
    }

    /// A file last modified before the window opened is skipped and counted; the
    /// planted negative is the same file with a current mtime, which is read.
    #[test]
    fn files_older_than_the_window_are_skipped_and_counted() {
        let root = scratch("mtime");
        let old = root.join("old.jsonl");
        write(&old, &claude_line("m1", "2026-08-25T10:00:00Z", 5));
        // A century before the file's own mtime, derived from that mtime rather than
        // from the clock: only the clock module reads the system clock.
        let written_at = fs::metadata(&old).unwrap().modified().unwrap();
        let long_ago = written_at - std::time::Duration::from_secs(100 * 365 * 86_400);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let config = config_with(vec![source("cc", &root, "claude-code")]);
        let skipped = assemble(&config, window("2026-08-25", 1), now()).unwrap();
        assert_eq!(skipped.ingest.files_skipped_before_window, 1);
        assert_eq!(skipped.ingest.files_read, 0);
        assert!(skipped.groups.is_empty());

        write(
            &root.join("new.jsonl"),
            &claude_line("m2", "2026-08-25T10:00:00Z", 6),
        );
        let read = assemble(&config, window("2026-08-25", 1), now()).unwrap();
        assert_eq!(read.ingest.files_read, 1);
        assert_eq!(read.groups[0].usage.known().output().value(), 6);
    }

    /// A claude-code line with no message id rides the heuristic domain: same
    /// timestamp, session and input as its sibling, no output count. The pair
    /// shares a heuristic key but normalizes to different payloads (one counts
    /// its output, one admits it does not know), so the pair is quarantined and
    /// the day's aggregate is partial, naming what the pair would have carried.
    /// The planted negative is the same fixture without the disagreeing line:
    /// the surviving occurrence is canonical, the aggregate is complete, and
    /// both occurrences' counts land.
    #[test]
    fn a_quarantined_collision_marks_its_groups_partial_and_counts_no_winner() {
        let with_output = r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00Z","sessionId":"s1","message":{"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        let without_output = r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00Z","sessionId":"s1","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        let strong = r#"{"type":"assistant","timestamp":"2026-08-25T10:05:00Z","sessionId":"s1","message":{"id":"m9","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;

        let root = scratch("collision");
        write(
            &root.join("a.jsonl"),
            &[
                with_output.to_string(),
                without_output.to_string(),
                strong.to_string(),
            ]
            .join("\n"),
        );
        let config = config_with(vec![source("cc", &root, "claude-code")]);
        let report = assemble(&config, window("2026-08-25", 1), now()).unwrap();

        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        assert_eq!(group.usage.known().input().value(), 100);
        assert_eq!(group.usage.known().output().value(), 50);
        let missing: Vec<String> = group
            .usage
            .coverage()
            .missing()
            .expect("the group must be partial")
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect();
        assert_eq!(missing, ["input", "output"]);

        // The planted negative: remove the disagreeing line and the same day
        // reads complete with both occurrences counted, so the partial
        // coverage above is attributable to the collision and nothing else.
        let root = scratch("collision-negative");
        write(
            &root.join("a.jsonl"),
            &[with_output.to_string(), strong.to_string()].join("\n"),
        );
        let config = config_with(vec![source("cc", &root, "claude-code")]);
        let report = assemble(&config, window("2026-08-25", 1), now()).unwrap();
        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        assert_eq!(group.usage.known().input().value(), 110);
        assert_eq!(group.usage.known().output().value(), 55);
        assert!(group.usage.coverage().missing().is_none());
    }

    /// Configuration mistakes are usage errors that name the source.
    #[test]
    fn a_source_without_a_format_or_with_a_missing_root_is_a_usage_error() {
        let root = scratch("usage");
        let mut no_format = source("cc", &root, "claude-code");
        no_format.format = None;
        let error = assemble(
            &config_with(vec![no_format]),
            window("2026-08-25", 1),
            now(),
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::Usage(ref m) if m.contains("cc")),
            "{error:?}"
        );

        let missing = source("gone", &root.join("nowhere"), "pi");
        let error =
            assemble(&config_with(vec![missing]), window("2026-08-25", 1), now()).unwrap_err();
        assert!(
            matches!(error, Error::Usage(ref m) if m.contains("gone")),
            "{error:?}"
        );

        let error = assemble(&config_with(vec![]), window("2026-08-25", 1), now()).unwrap_err();
        assert!(matches!(error, Error::Usage(_)));
        assert!(SpendWindow::starting(UtcDate::parse("2026-08-25").unwrap(), 0).is_err());
    }
}
