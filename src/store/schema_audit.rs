//! The schema-compliance audit (`aub-sth.8`, PLAN.md 11.3, 12, 36).
//!
//! The database is a module boundary like any other, and it is the one boundary where the
//! Rust type system provides no help at all: SQLite will store `"forty"` in a column the
//! code believes is an integer, and the mistake surfaces later as a parse failure in a
//! report somebody trusted. STRICT tables remove that class entirely, and explicit CHECK,
//! FOREIGN KEY and UNIQUE constraints remove the domain-specific subset the type system
//! cannot express across the boundary.
//!
//! This module owns the audit that keeps that true as tables are added. It enumerates the
//! live schema rather than a hand-kept list, so a table added later without constraints is
//! a regression a test catches rather than a gap nobody notices.
//!
//! # What "quantity column" means here
//!
//! A quantity column is an `INTEGER` column, not the rowid and not a foreign key, whose
//! name marks it as carrying a magnitude: a `_micros`, `_nanos`, `_ppm`, `_count`,
//! `_bytes` or `_index` suffix, or a `token`/`credits`/`quota` stem. Instant columns
//! (`_at`, `_from`, `_until`, `_time`, `_ts`, `_start`, plus `start`, `end`, `resets_at`,
//! `due_at`, `mtime_nanos`) are magnitudes too, but a UTC instant has no domain range to
//! assert, so they are not in scope.
//!
//! Every quantity column must appear inside a CHECK constraint on its table, or be named
//! in [`QUANTITY_CHECK_EXEMPT`] with the reason it is not. The two mandatory classes from
//! the bead, non-negative token counts and quota fractions within `0..=1_000_000`, are a
//! floor the exemption list may never touch: `audit` rejects an exemption for a column
//! whose name ends in `_ppm` or is a token count.
//!
//! # Repair
//!
//! A schema finding is a build-time regression: the fix is a migration, never a runtime
//! action, so this module exposes facts only. `aub-n27.7` owns how `doctor` renders and
//! classifies them.

use std::collections::BTreeSet;

use rusqlite::Connection;

use crate::error::Error;

/// One way the live schema falls short of the contract.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaFinding {
    /// A table is not declared `STRICT`, so SQLite applies its ordinary dynamic typing
    /// and a value of the wrong type is stored rather than rejected.
    TableNotStrict { table: String },
    /// A quantity column carries no CHECK constraint and is not a documented exemption.
    QuantityColumnWithoutCheck { table: String, column: String },
    /// A column named `<table>_id` where a table `<table>` exists carries no foreign key,
    /// so an orphan row can be written. External identifiers (`tracker_event_id` and the
    /// like, where no such table exists) are not caught by this: their name collision
    /// with the child-reference convention is unavoidable.
    ChildColumnWithoutForeignKey { table: String, column: String },
    /// A column named `*_ppm` carries no `0..=1_000_000` range CHECK, which the bead
    /// makes a hard floor rather than an exemptible column.
    QuotaFractionColumnWithoutRangeCheck { table: String, column: String },
}

impl SchemaFinding {
    /// A one-line human description, for `doctor` and for test failure messages.
    pub fn describe(&self) -> String {
        match self {
            Self::TableNotStrict { table } => format!("table {table} is not STRICT"),
            Self::QuantityColumnWithoutCheck { table, column } => {
                format!("{table}.{column} is a quantity column with no CHECK and no exemption")
            }
            Self::ChildColumnWithoutForeignKey { table, column } => {
                format!("{table}.{column} looks like a child reference but has no foreign key")
            }
            Self::QuotaFractionColumnWithoutRangeCheck { table, column } => {
                format!("{table}.{column} is a quota fraction with no 0..=1_000_000 range CHECK")
            }
        }
    }
}

/// The outcome of auditing a live schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaAudit {
    findings: Vec<SchemaFinding>,
}

impl SchemaAudit {
    /// Every finding, sorted, so two audits of the same schema compare equal.
    pub fn findings(&self) -> &[SchemaFinding] {
        &self.findings
    }

    /// True when the schema meets the contract.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// A multi-line report, or `None` when clean.
    pub fn report(&self) -> Option<String> {
        if self.findings.is_empty() {
            return None;
        }
        Some(
            self.findings
                .iter()
                .map(SchemaFinding::describe)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// Quantity columns that deliberately carry no CHECK, each with the reason.
///
/// A column here relies on STRICT typing plus its Rust-side constructor for its domain
/// bound, because the bound is either not a simple range or asserting it needs a
/// table-recreate migration across a table this bead does not own. Adding a new bare
/// quantity column means adding a CHECK or, with a justification a reviewer accepts, a
/// row here: `audit` treats an unlisted, unchecked quantity column as a finding.
///
/// The floor classes (`_ppm`, token counts) may never be exempted; `audit` rejects an
/// entry that tries.
pub const QUANTITY_CHECK_EXEMPT: &[(&str, &str, &str)] = &[
    (
        "sampling_policy_snapshot",
        "ordinary_cadence_nanos",
        "sampling cadence; validated positive by the policy constructor (aub-me5.3). \
         A DB range CHECK needs recreating a table meter_attempt has a foreign key into \
         and invariant 27 depends on; tracked for a follow-up hardening bead.",
    ),
    (
        "sampling_policy_snapshot",
        "freshness_horizon_nanos",
        "freshness horizon; same ownership and constructor guard as ordinary_cadence_nanos.",
    ),
    (
        "sampling_policy_snapshot",
        "command_budget_nanos",
        "per-command wall budget; same ownership and constructor guard as ordinary_cadence_nanos.",
    ),
    (
        "cost_model_term",
        "credits_per_token_micros",
        "a per-term credit coefficient; the fail-closed conversion (aub-ai3.2) is the \
         semantic guard. A >= 0 CHECK needs recreating a table with immutability triggers \
         (aub-ai3.1); tracked for a follow-up hardening bead.",
    ),
    (
        "window_calibration_candidate",
        "fitted_micros_per_point",
        "a proposed fit coefficient; a candidate is evidence, not truth (aub-c0b.7), and \
         its plausibility is judged by the health state machine (aub-c0b.10), not a column range.",
    ),
    (
        "window_calibration_candidate",
        "equivalent_full_window_capacity_micros",
        "derived from the candidate coefficient; its sign follows fitted_micros_per_point.",
    ),
    (
        "window_calibration_result",
        "fitted_micros_per_point",
        "a fitted coefficient; the calibration health state machine (aub-c0b.10) is the \
         semantic guard. A >= 0 CHECK is defensible but needs a recreate migration on a \
         table with immutability triggers; tracked for a follow-up hardening bead.",
    ),
    (
        "window_calibration_result",
        "equivalent_full_window_capacity_micros",
        "derived capacity; its sign follows fitted_micros_per_point.",
    ),
    (
        "window_calibration_result",
        "out_of_sample_residual_micros",
        "an out-of-sample residual magnitude, diagnostic only; the fit's acceptance is \
         decided by validation_method, not by a column range.",
    ),
    (
        "window_calibration_result",
        "condition_number_micros",
        "the multivariate fit's condition number; ill-conditioned fits are rejected before \
         a result is written (PLAN.md 22.1), so the stored value is a recorded diagnostic.",
    ),
];

/// Column-name stems and suffixes that mark an `INTEGER` column as a magnitude with a
/// domain range worth asserting. Timestamp columns are magnitudes too but a UTC instant
/// has no range, so they are excluded by [`is_timestamp_column`].
fn is_quantity_column(name: &str) -> bool {
    const SUFFIXES: [&str; 6] = ["_micros", "_nanos", "_ppm", "_count", "_bytes", "_index"];
    const STEMS: [&str; 3] = ["token", "credits", "quota"];
    if is_timestamp_column(name) {
        return false;
    }
    SUFFIXES.iter().any(|s| name.ends_with(s)) || STEMS.iter().any(|s| name.contains(s))
}

/// A UTC instant column: a magnitude with no domain range to assert. `mtime_nanos` is
/// spelled out because `_nanos` otherwise reads as a duration, which does have a floor.
fn is_timestamp_column(name: &str) -> bool {
    const SUFFIXES: [&str; 6] = ["_at", "_from", "_until", "_time", "_ts", "_start"];
    matches!(
        name,
        "start" | "end" | "resets_at" | "due_at" | "mtime_nanos"
    ) || SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// True when `name` is a quota-fraction column, the class the bead makes a hard floor.
fn is_quota_fraction_column(name: &str) -> bool {
    name.ends_with("_ppm")
}

/// True when `name` is `<stem>_id` and a table named `<stem>` exists, so the column is a
/// child reference that must carry a foreign key. An `_id` column whose stem names no
/// table is an external identifier and is not in scope.
fn references_a_known_table(name: &str, tables: &BTreeSet<&str>) -> bool {
    name.strip_suffix("_id")
        .is_some_and(|stem| !stem.is_empty() && tables.contains(stem))
}

/// True when `name` is a token-count column, the other hard-floor class.
fn is_token_count_column(name: &str) -> bool {
    name.contains("token") && name.ends_with("count") || name == "count"
}

/// One column as SQLite reports it through `PRAGMA table_xinfo`.
struct Column {
    name: String,
    declared_type: String,
    is_rowid_pk: bool,
}

/// Audits the live schema behind `conn`, enumerated from SQLite itself.
pub fn audit(conn: &Connection) -> Result<SchemaAudit, Error> {
    reject_illegal_exemptions()?;

    let mut findings: BTreeSet<SchemaFinding> = BTreeSet::new();

    let tables = user_tables(conn)?;
    let table_names: BTreeSet<&str> = tables.iter().map(String::as_str).collect();

    for table in &tables {
        let table = table.as_str();
        if !table_is_strict(conn, table)? {
            findings.insert(SchemaFinding::TableNotStrict {
                table: table.to_string(),
            });
        }

        let table_sql = table_sql(conn, table)?;
        let check_haystack = check_clause_text(&table_sql);
        let foreign_key_columns = foreign_key_columns(conn, table)?;

        for column in columns(conn, table)? {
            if column.is_rowid_pk || column.declared_type.to_uppercase() != "INTEGER" {
                continue;
            }
            let is_fk = foreign_key_columns.contains(&column.name);

            if !is_fk && references_a_known_table(&column.name, &table_names) {
                findings.insert(SchemaFinding::ChildColumnWithoutForeignKey {
                    table: table.to_string(),
                    column: column.name.clone(),
                });
            }

            if is_fk || !is_quantity_column(&column.name) {
                continue;
            }

            let named_in_check = check_haystack.contains(&column.name);

            if is_quota_fraction_column(&column.name)
                && !has_ppm_range_check(&table_sql, &column.name)
            {
                findings.insert(SchemaFinding::QuotaFractionColumnWithoutRangeCheck {
                    table: table.to_string(),
                    column: column.name.clone(),
                });
            } else if !named_in_check && !is_exempt(table, &column.name) {
                findings.insert(SchemaFinding::QuantityColumnWithoutCheck {
                    table: table.to_string(),
                    column: column.name.clone(),
                });
            }
        }
    }

    Ok(SchemaAudit {
        findings: findings.into_iter().collect(),
    })
}

/// The exemption list may never cover a hard-floor column. A programming error here is
/// caught before any audit result is trusted.
fn reject_illegal_exemptions() -> Result<(), Error> {
    for (table, column, _) in QUANTITY_CHECK_EXEMPT {
        if is_quota_fraction_column(column) || is_token_count_column(column) {
            return Err(Error::Store(format!(
                "schema audit exemption list covers a hard-floor column: {table}.{column}"
            )));
        }
    }
    Ok(())
}

fn is_exempt(table: &str, column: &str) -> bool {
    QUANTITY_CHECK_EXEMPT
        .iter()
        .any(|(t, c, _)| *t == table && *c == column)
}

/// Every user table, excluding SQLite's own bookkeeping tables.
fn user_tables(conn: &Connection) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .map_err(|e| Error::Store(format!("cannot list tables: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| Error::Store(format!("cannot read table list: {e}")))?;
    let mut tables = Vec::new();
    for row in rows {
        tables.push(row.map_err(|e| Error::Store(format!("cannot read a table name: {e}")))?);
    }
    Ok(tables)
}

/// Reads the `strict` flag from `PRAGMA table_list`.
fn table_is_strict(conn: &Connection, table: &str) -> Result<bool, Error> {
    conn.query_row(
        "SELECT strict FROM pragma_table_list WHERE name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|flag| flag == 1)
    .map_err(|e| Error::Store(format!("cannot read STRICT flag for {table}: {e}")))
}

fn table_sql(conn: &Connection, table: &str) -> Result<String, Error> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, String>(0),
    )
    .map_err(|e| {
        Error::Store(format!(
            "cannot read the CREATE TABLE text for {table}: {e}"
        ))
    })
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<Column>, Error> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_xinfo({table})"))
        .map_err(|e| Error::Store(format!("cannot prepare table_xinfo for {table}: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Column {
                name: row.get::<_, String>("name")?,
                declared_type: row.get::<_, String>("type")?,
                is_rowid_pk: row.get::<_, i64>("pk")? == 1
                    && row.get::<_, String>("type")?.to_uppercase() == "INTEGER",
            })
        })
        .map_err(|e| Error::Store(format!("cannot read columns of {table}: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| Error::Store(format!("cannot read a column of {table}: {e}")))?);
    }
    Ok(out)
}

fn foreign_key_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>, Error> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .map_err(|e| Error::Store(format!("cannot prepare foreign_key_list for {table}: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>("from"))
        .map_err(|e| Error::Store(format!("cannot read foreign keys of {table}: {e}")))?;
    let mut out = BTreeSet::new();
    for row in rows {
        out.insert(row.map_err(|e| Error::Store(format!("cannot read a foreign key: {e}")))?);
    }
    Ok(out)
}

/// The concatenated text of every `CHECK ( ... )` clause in `table_sql`, lowercased, so a
/// column name can be tested for membership without a SQL parser. Balanced-paren scan so
/// a nested `CHECK (a IN (1, 2))` is captured whole.
fn check_clause_text(table_sql: &str) -> String {
    let lower = table_sql.to_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::new();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("check") {
        let keyword_end = search_from + rel + "check".len();
        let Some(open_rel) = lower[keyword_end..].find('(') else {
            break;
        };
        let open = keyword_end + open_rel;
        // Only a `check` immediately followed (past whitespace) by `(` is a constraint.
        if lower[keyword_end..open].trim().is_empty() {
            let mut depth = 0i32;
            let mut i = open;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            out.push_str(&lower[open..=i]);
                            out.push('\n');
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            search_from = i.max(open + 1);
        } else {
            search_from = keyword_end;
        }
    }
    out
}

/// True when `table_sql` carries a range CHECK on `column` that both bounds it below
/// (`>= 0` or `> 0`) and caps it at `1_000_000`. A ppm value is a fraction in
/// `0..=1_000_000`; whether the floor is inclusive or exclusive is the column's call.
fn has_ppm_range_check(table_sql: &str, column: &str) -> bool {
    let checks = check_clause_text(table_sql);
    checks.split('\n').any(|clause| {
        clause.contains(column)
            && clause.contains("1000000")
            && clause.contains("<=")
            && (clause.contains(">=") || clause.contains('>'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantity_and_timestamp_classification() {
        assert!(is_quantity_column("credits_per_token_micros"));
        assert!(is_quantity_column("sample_count"));
        assert!(is_quantity_column("quota_used_ppm"));
        assert!(is_quantity_column("nominal_duration_nanos"));
        assert!(!is_quantity_column("valid_from"));
        assert!(!is_quantity_column("resets_at"));
        assert!(!is_quantity_column("knowledge_time"));
        assert!(!is_quantity_column("provider"));
        assert!(is_timestamp_column("started_at"));
        assert!(is_timestamp_column("valid_until"));
        assert!(!is_timestamp_column("sample_count"));
    }

    #[test]
    fn hard_floor_columns_are_never_classified_as_exemptible() {
        assert!(is_quota_fraction_column("reported_resolution_ppm"));
        assert!(is_token_count_column("input_token_count"));
        assert!(is_token_count_column("count"));
        assert!(!is_token_count_column("sample_count"));
        // The static exemption list must not smuggle one in.
        reject_illegal_exemptions().expect("the shipped exemption list is legal");
    }

    #[test]
    fn check_clause_extraction_captures_nested_parens() {
        let sql = "CREATE TABLE t (\
            a INTEGER CHECK (a >= 0),\
            b TEXT CHECK (b IN ('x', 'y')),\
            c INTEGER,\
            CHECK (a <= 1000000)\
        ) STRICT";
        let text = check_clause_text(sql);
        assert!(text.contains("a >= 0"));
        assert!(text.contains("b in ('x', 'y')"));
        assert!(text.contains("a <= 1000000"));
        assert!(!text.contains(" c "));
    }

    #[test]
    fn ppm_range_check_detection() {
        let inclusive = "x INTEGER NOT NULL CHECK (x >= 0 AND x <= 1000000)";
        let exclusive_floor = "x INTEGER NOT NULL CHECK (x > 0 AND x <= 1000000)";
        let missing_ceiling = "x INTEGER NOT NULL CHECK (x >= 0)";
        let missing_floor = "x INTEGER NOT NULL CHECK (x <= 1000000)";
        assert!(has_ppm_range_check(inclusive, "x"));
        assert!(has_ppm_range_check(exclusive_floor, "x"));
        assert!(!has_ppm_range_check(missing_ceiling, "x"));
        assert!(!has_ppm_range_check(missing_floor, "x"));
    }

    #[test]
    fn child_reference_detection_ignores_external_ids() {
        let tables: BTreeSet<&str> = ["account", "meter_attempt"].into_iter().collect();
        assert!(references_a_known_table("account_id", &tables));
        assert!(!references_a_known_table("tracker_event_id", &tables));
        assert!(!references_a_known_table("id", &tables));
        assert!(!references_a_known_table("attempt_id", &tables));
    }
}
