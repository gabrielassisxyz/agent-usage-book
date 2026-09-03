//! Assembly of the spend report from configured transcript sources.
//!
//! This is the in-memory path: every invocation discovers the files under each
//! configured source, parses them with the parser the source declares, collapses
//! replayed occurrences by strong identity, and groups what falls inside the
//! requested window by UTC day and source. Nothing is persisted and nothing is
//! cached, so the report is the transcripts as they are on disk at the moment of
//! the call. The canonical store and the incremental index replace this path
//! without changing what it reports.
//!
//! Two things are never done here. A count is never printed without the ingest
//! summary that qualifies it, because a total with quarantined records behind it is
//! not a total. And an event whose record carried no timestamp is never placed in a
//! day: it is counted as undated and left out of every group.
//!
//! May not depend on:
//! - presentation
//! - store
//! - calibration, cost models, rate cards or meter observations

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::Config;
use crate::dedup::deduplicate;
use crate::domain::provenance::{DerivationId, EvidenceId, QuerySemantics};
use crate::domain::time::{UtcDate, UtcTimestamp, unix_nanos};
use crate::domain::tokens::{TokenCount, UsageVector};
use crate::error::Error;
use crate::evidence::{ComponentKind, CoverageCompleteness, Provenance};
use crate::logging::LogicalName;
use crate::report::models::{
    IngestSummary, LedgerGeneration, ReportMetadata, SpendGroup, SpendGroupProvenance, SpendReport,
};
use crate::report::provenance::{ProvenanceNode, ValueArithmetic};
use crate::transcripts::{
    DiscoveryError, DiscoveryOptions, NormalizedUsageEvent, ParserAdapter, ParserVersion,
    SourceLocation, discover, parser_for_format,
};

/// The UTC day range a spend report covers: `since` inclusive, `until` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendWindow {
    pub since: UtcDate,
    pub until: UtcDate,
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
            let Ok(contents) = std::fs::read_to_string(file) else {
                summary.unreadable_files.push(file.display().to_string());
                continue;
            };
            summary.files_read += 1;
            let output = parser.parse(
                &contents,
                &SourceLocation::new(file.display().to_string(), 1),
            );
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
                "transcript source {} declares no format; set format to one of claude-code, codex, pi",
                source.name
            ))
        })?;
        let parser = parser_for_format(format).ok_or_else(|| {
            Error::Usage(format!(
                "transcript source {} declares unknown format {format}; known formats are claude-code, codex, pi",
                source.name
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TranscriptConfig;
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
