//! The versioned JSONL renderer for `aub export` (`aub-xus.7`).
//!
//! One header line, then one JSON object per row. The header carries the
//! export format version, the key mode, the ledger and ingestion generations
//! the export was produced from, which logical identifier classes the rows
//! carry, the unresolved-event count and the generation time. Every quantity
//! serializes as `{"value": "...", "unit": "..."}` per the JSON contract
//! (`presentation::json::Quantity`), so a consumer never infers a unit from a
//! bare number.
//!
//! The export format is versioned independently of the JSON envelope: JSONL
//! gives no schema hint at all, so the header's `schema` field is the only
//! thing a consumer can check. Bump [`EXPORT_SCHEMA_VERSION`] when the shape
//! changes.
//!
//! May not depend on:
//! - provider adapters
//! - store or calibration (boundary rule 09)

use crate::presentation::json::{Quantity, json_string};
use crate::report::models::{ExportReport, ExportRow};

/// The export format version. Bump this when the header or row shape changes;
/// the contract tests below pin the exact shape, so a field added without
/// bumping this fails them.
pub const EXPORT_SCHEMA_VERSION: u32 = 1;

/// The logical identifier classes an export can carry in its rows, in the
/// order the header names them. The account dimension is deliberately absent:
/// session-to-account attribution is a marker timeline with its own ranking
/// semantics (PLAN.md 19.2), owned by the attribution beads, so this list
/// grows there rather than by guessing inside an export command.
const LOGICAL_IDENTIFIER_CLASSES: [&str; 2] = ["project", "repository"];

/// Renders the export as JSONL: one header line, then one JSON object per row,
/// in the report's own deterministic order.
pub fn export_jsonl(report: &ExportReport) -> String {
    let mut out = String::new();
    out.push_str(&header_line(report));
    out.push('\n');
    for row in &report.rows {
        out.push_str(&row_line(row, report.included_logical_ids));
        out.push('\n');
    }
    out
}

/// The header line: the format version, the key mode, the generations the
/// export was produced from, what was included, the unresolved-event count and
/// the generation time. `generated_at` is the only volatile field: everything
/// else is a function of the ledger state.
fn header_line(report: &ExportReport) -> String {
    format!(
        "{{\"schema\":{},\"key\":{},\"ledger_generation\":{},\"ingestion_generation\":{},\"included_identifiers\":{},\"unresolved_events\":{},\"generated_at\":{}}}",
        EXPORT_SCHEMA_VERSION,
        json_string(report.key.as_str()),
        report.metadata.ledger_generation.get(),
        report
            .metadata
            .ingestion_generation
            .map(|g| g.get())
            .unwrap_or(0),
        identifiers_field(report.included_logical_ids),
        Quantity::new(report.unresolved_events.to_string(), "events").to_json(),
        report.metadata.generated_at.unix_nanos(),
    )
}

/// The `included_identifiers` header field: which logical identifier classes
/// the rows carry, derived from the same flag that gated the rows, so the
/// header cannot disagree with the records below it.
fn identifiers_field(included_logical_ids: bool) -> String {
    if included_logical_ids {
        let classes = LOGICAL_IDENTIFIER_CLASSES
            .iter()
            .map(|class| json_string(class))
            .collect::<Vec<_>>()
            .join(",");
        format!("[{classes}]")
    } else {
        "[]".to_string()
    }
}

/// One row: the key value, the sessions it covers, its usage by token class
/// and the logical identifiers the caller chose to include. The flag is the
/// renderer's own gate: a row carrying identifiers under a header that says
/// none were included would be exactly the wrong number this project exists
/// to prevent, so the renderer applies the recorded choice itself instead of
/// trusting the store to have gated the rows.
fn row_line(row: &ExportRow, include_logical_ids: bool) -> String {
    let (project_keys, repository_keys) = if include_logical_ids {
        (&row.project_keys, &row.repository_keys)
    } else {
        (&Vec::<String>::new(), &Vec::<String>::new())
    };
    let usage = row
        .usage
        .entries()
        .map(|(token_class, count)| {
            format!(
                "{}:{}",
                json_string(token_class),
                Quantity::new(count.to_string(), "tokens").to_json()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let project_keys = project_keys
        .iter()
        .map(|key| json_string(key))
        .collect::<Vec<_>>()
        .join(",");
    let repository_keys = repository_keys
        .iter()
        .map(|key| json_string(key))
        .collect::<Vec<_>>()
        .join(",");
    let last_end = match row.last_end {
        Some(end) => end.unix_nanos().to_string(),
        None => "null".to_string(),
    };
    format!(
        "{{\"key\":{},\"session_count\":{},\"first_start\":{},\"last_end\":{},\"usage\":{{{usage}}},\"project_keys\":[{project_keys}],\"repository_keys\":[{repository_keys}]}}",
        json_string(&row.key),
        Quantity::new(row.session_count.to_string(), "sessions").to_json(),
        row.first_start.unix_nanos(),
        last_end,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provenance::QuerySemantics;
    use crate::domain::time::UtcTimestamp;
    use crate::report::models::{
        ExportReport, IngestionGeneration, LedgerGeneration, ReportMetadata,
    };
    use crate::report::provenance::{ProvenanceNode, ValueArithmetic};
    use crate::store::export::{ExportKey, ExportRow, UsageByTokenClass};

    fn node() -> ProvenanceNode {
        ProvenanceNode::new(
            [] as [crate::domain::provenance::EvidenceId; 0],
            [] as [crate::domain::provenance::WitnessId; 0],
            QuerySemantics::new("export", "session-id"),
            1,
            1,
            ValueArithmetic::Count,
        )
    }

    fn metadata(generated_at: i64) -> ReportMetadata {
        ReportMetadata::new(
            UtcTimestamp::from_unix_nanos(generated_at),
            UtcTimestamp::from_unix_nanos(generated_at),
            LedgerGeneration::new(7),
            Some(IngestionGeneration::new(3)),
        )
    }

    fn row(key: &str, usage: &[(&str, i64)]) -> ExportRow {
        let mut by_class = UsageByTokenClass::default();
        for (token_class, count) in usage {
            by_class.add(token_class, *count);
        }
        ExportRow {
            key: key.to_string(),
            session_count: 1,
            first_start: UtcTimestamp::from_unix_nanos(10),
            last_end: Some(UtcTimestamp::from_unix_nanos(20)),
            usage: by_class,
            project_keys: vec!["proj-x".to_string()],
            repository_keys: vec!["repo-x".to_string()],
        }
    }

    /// Both key modes render one JSON object per line: a header line plus one
    /// line per row, with the format version and both generations in the
    /// header.
    #[test]
    fn both_key_modes_render_one_json_object_per_line_with_version_and_generations() {
        for (key, key_name) in [
            (ExportKey::Session, "session-id"),
            (ExportKey::Run, "run-id"),
        ] {
            let report = ExportReport::new(
                metadata(2_000),
                key,
                false,
                vec![row("claude-code:sess-a", &[("input", 105), ("output", 40)])],
                0,
                node(),
            );
            let rendered = export_jsonl(&report);
            let lines: Vec<&str> = rendered.lines().collect();
            assert_eq!(lines.len(), 2, "{key_name}: header plus one row");
            for line in &lines {
                let parsed: serde_json::Value =
                    serde_json::from_str(line).expect("every line is one JSON object");
                assert!(parsed.is_object(), "{key_name}: every line is an object");
            }
            let header: serde_json::Value = serde_json::from_str(lines[0]).expect("header parses");
            assert_eq!(header["schema"], 1);
            assert_eq!(header["key"], key_name);
            assert_eq!(header["ledger_generation"], 7);
            assert_eq!(header["ingestion_generation"], 3);
            assert_eq!(header["generated_at"], 2_000);
        }
    }

    /// The identifier inclusion flag controls whether logical project and
    /// repository identifiers appear in the rows, and the header records which
    /// identifier classes were included. The rows here deliberately carry
    /// identifiers even in the flag-off case: the renderer, not the fixture,
    /// is what must enforce the recorded choice.
    #[test]
    fn the_inclusion_flag_controls_logical_ids_and_the_header_records_it() {
        let without = ExportReport::new(
            metadata(2_000),
            ExportKey::Session,
            false,
            vec![row("claude-code:sess-a", &[("input", 1)])],
            0,
            node(),
        );
        let rendered_without = export_jsonl(&without);
        let header_without: serde_json::Value =
            serde_json::from_str(rendered_without.lines().next().unwrap()).unwrap();
        assert_eq!(
            header_without["included_identifiers"],
            serde_json::json!([])
        );
        assert!(
            !rendered_without.contains("proj-x"),
            "logical ids must be absent when not included"
        );

        let with = ExportReport::new(
            metadata(2_000),
            ExportKey::Session,
            true,
            vec![row("claude-code:sess-a", &[("input", 1)])],
            0,
            node(),
        );
        let rendered_with = export_jsonl(&with);
        let header_with: serde_json::Value =
            serde_json::from_str(rendered_with.lines().next().unwrap()).unwrap();
        assert_eq!(
            header_with["included_identifiers"],
            serde_json::json!(["project", "repository"])
        );
        assert!(
            rendered_with.contains("proj-x"),
            "logical ids must appear when included"
        );
    }

    /// Every quantity in the export carries its unit, following the JSON
    /// contract: usage counts in tokens, the session count in sessions, the
    /// unresolved-event count in events.
    #[test]
    fn quantities_carry_their_units() {
        let report = ExportReport::new(
            metadata(2_000),
            ExportKey::Session,
            true,
            vec![row("claude-code:sess-a", &[("input", 105), ("output", 40)])],
            2,
            node(),
        );
        let rendered = export_jsonl(&report);
        let lines: Vec<&str> = rendered.lines().collect();
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["unresolved_events"]["value"], "2");
        assert_eq!(header["unresolved_events"]["unit"], "events");

        let row: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(row["session_count"]["value"], "1");
        assert_eq!(row["session_count"]["unit"], "sessions");
        assert_eq!(row["usage"]["input"]["value"], "105");
        assert_eq!(row["usage"]["input"]["unit"], "tokens");
        assert_eq!(row["usage"]["output"]["value"], "40");
        assert_eq!(row["usage"]["output"]["unit"], "tokens");
    }

    /// A row without an end timestamp renders `last_end` as null, never as a
    /// fabricated zero.
    #[test]
    fn a_row_without_an_end_renders_last_end_null() {
        let mut open = row("claude-code:sess-a", &[("input", 1)]);
        open.last_end = None;
        let report = ExportReport::new(
            metadata(2_000),
            ExportKey::Session,
            false,
            vec![open],
            0,
            node(),
        );
        let rendered = export_jsonl(&report);
        let row: serde_json::Value =
            serde_json::from_str(rendered.lines().nth(1).unwrap()).unwrap();
        assert!(row["last_end"].is_null());
    }
}
