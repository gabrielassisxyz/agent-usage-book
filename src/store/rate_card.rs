//! The rate-card repository: insert-only writes and interval reads over
//! immutable versioned price records (`aub-wyu.1`).
//!
//! Repository conventions (`crate::store`): methods take and return domain
//! types, and an evidence table exposes no update path and no delete path. The
//! immutability is additionally enforced by the table's own triggers
//! (`migrations/0011`), so a future caller that bypasses this module still
//! cannot rewrite a price history.

use rusqlite::params;

use crate::domain::rate_card::{
    BillingBasis, CurrencyCode, Publication, RateCard, RateCardDraft, ReviewDuePolicy, TokenClass,
};
use crate::domain::time::{UtcDate, UtcTimestamp};
use crate::error::Error;

/// What one import pass did. A re-import of an unchanged book is visibly a
/// no-op: the counts say so instead of the operator having to diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSummary {
    pub cards_added: u64,
    pub cards_unchanged: u64,
}

/// Persists drafts idempotently. The content index (`migrations/0011`) makes
/// the operation safe to repeat: a draft whose full content already exists is
/// reported unchanged and nothing is written; a corrected price or a filled-in
/// provenance is new content and becomes a new record.
pub fn insert(
    connection: &rusqlite::Connection,
    drafts: &[RateCardDraft],
    imported_at: UtcTimestamp,
) -> Result<ImportSummary, Error> {
    let mut summary = ImportSummary {
        cards_added: 0,
        cards_unchanged: 0,
    };
    for draft in drafts {
        let added = connection
            .execute(
                "INSERT INTO rate_card (
                    vendor, model, token_class, rate_micros, currency, billing_basis,
                    effective_start, effective_end, imported_at, published_at, source, review_due
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT DO NOTHING",
                params![
                    draft.vendor,
                    draft.model,
                    draft.token_class.as_str(),
                    draft.rate_micros,
                    draft.currency.as_str(),
                    draft.billing_basis.as_str(),
                    draft.effective_start.iso(),
                    draft.effective_end.map(UtcDate::iso),
                    imported_at.unix_nanos(),
                    draft.publication.published_at.map(UtcTimestamp::unix_nanos),
                    draft.publication.source,
                    draft.review_due.iso(),
                ],
            )
            .map_err(|error| Error::Store(format!("cannot insert rate card: {error}")))?;
        if added == 1 {
            summary.cards_added += 1;
        } else {
            summary.cards_unchanged += 1;
        }
    }
    Ok(summary)
}

/// Every persisted record, oldest effective interval first. `rate-card
/// history` renders this; superseded records appear here with their intervals.
pub fn history(connection: &rusqlite::Connection) -> Result<Vec<RateCard>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT id, imported_at, vendor, model, token_class, rate_micros, currency,
                    billing_basis, effective_start, effective_end, published_at, source, review_due
             FROM rate_card
             ORDER BY vendor, model, token_class, effective_start, id",
        )
        .map_err(|error| Error::Store(format!("cannot read rate card history: {error}")))?;
    let rows = statement
        .query_map([], row_to_card)
        .map_err(|error| Error::Store(format!("cannot query rate card history: {error}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode rate card: {error}")))
}

/// The records effective at an instant: the interval contains it. `rate-card
/// show` asks for now; the valuation layer (aub-wyu.2) asks for an event's
/// time through its own named path.
pub fn effective_at(
    connection: &rusqlite::Connection,
    at: UtcTimestamp,
) -> Result<Vec<RateCard>, Error> {
    let day = at.utc_date().iso();
    let mut statement = connection
        .prepare(
            "SELECT id, imported_at, vendor, model, token_class, rate_micros, currency,
                    billing_basis, effective_start, effective_end, published_at, source, review_due
             FROM rate_card
             WHERE effective_start <= ?1
               AND (effective_end IS NULL OR effective_end > ?1)
             ORDER BY vendor, model, token_class, effective_start, id",
        )
        .map_err(|error| Error::Store(format!("cannot read effective rate cards: {error}")))?;
    let rows = statement
        .query_map(params![day], row_to_card)
        .map_err(|error| Error::Store(format!("cannot query effective rate cards: {error}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode rate card: {error}")))
}

/// How many records the table holds, for import reporting and the e2e cases.
pub fn count(connection: &rusqlite::Connection) -> Result<u64, Error> {
    connection
        .query_row("SELECT count(*) FROM rate_card", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count as u64)
        .map_err(|error| Error::Store(format!("cannot count rate cards: {error}")))
}

fn row_to_card(row: &rusqlite::Row<'_>) -> Result<RateCard, rusqlite::Error> {
    let id: i64 = row.get(0)?;
    let imported_at: i64 = row.get(1)?;
    let vendor: String = row.get(2)?;
    let model: String = row.get(3)?;
    let token_class: String = row.get(4)?;
    let rate_micros: i64 = row.get(5)?;
    let currency: String = row.get(6)?;
    let billing_basis: String = row.get(7)?;
    let effective_start: String = row.get(8)?;
    let effective_end: Option<String> = row.get(9)?;
    let published_at: Option<i64> = row.get(10)?;
    let source: Option<String> = row.get(11)?;
    let review_due: Option<String> = row.get(12)?;

    let token_class = TokenClass::parse(&token_class).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("unknown token class {token_class:?}").into(),
        )
    })?;
    let currency = CurrencyCode::parse(&currency).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            format!("unknown currency {currency:?}").into(),
        )
    })?;
    let billing_basis = BillingBasis::parse(&billing_basis).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            format!("unknown billing basis {billing_basis:?}").into(),
        )
    })?;
    let effective_start = UtcDate::parse(&effective_start).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            format!("unparseable effective_start {effective_start:?}").into(),
        )
    })?;
    let effective_end = match effective_end {
        None => None,
        Some(text) => Some(UtcDate::parse(&text).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                format!("unparseable effective_end {text:?}").into(),
            )
        })?),
    };
    let review_due = match review_due {
        None => ReviewDuePolicy::None,
        Some(text) => ReviewDuePolicy::On(UtcDate::parse(&text).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                format!("unparseable review_due {text:?}").into(),
            )
        })?),
    };

    Ok(RateCard {
        id,
        imported_at: UtcTimestamp::from_unix_nanos(imported_at),
        draft: RateCardDraft {
            vendor,
            model,
            token_class,
            rate_micros,
            currency,
            billing_basis,
            effective_start,
            effective_end,
            publication: Publication {
                source,
                published_at: published_at.map(UtcTimestamp::from_unix_nanos),
            },
            review_due,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::rate_card::parse_rate_micros;
    use crate::domain::time::FakeClock;
    use crate::store::connection::{AccessMode, PragmaPolicy};
    use crate::store::migrate::run_migrations;

    /// A fresh scratch directory under the system temp dir, removed on drop.
    /// A file database rather than `:memory:`: the connection policy requires
    /// WAL, which an in-memory database cannot report.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-rate-card-repo-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// The scratch must outlive the connection, so both travel together.
    fn open_migrated() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let mut conn = crate::store::connection::open(
            &scratch.path().join("rate-card.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1_000),
            },
        )
        .expect("scratch database must open");
        run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .expect("migrations must run");
        (scratch, conn)
    }

    fn draft(vendor: &str, model: &str, class: TokenClass, rate: &str) -> RateCardDraft {
        RateCardDraft {
            vendor: vendor.to_string(),
            model: model.to_string(),
            token_class: class,
            rate_micros: parse_rate_micros(rate).expect("test rate must parse"),
            currency: CurrencyCode::Usd,
            billing_basis: BillingBasis::PerMillionTokens,
            effective_start: UtcDate::parse("2026-06-24").unwrap(),
            effective_end: None,
            publication: Publication {
                source: None,
                published_at: None,
            },
            review_due: ReviewDuePolicy::None,
        }
    }

    #[test]
    fn effective_at_includes_open_ended_and_excludes_future_and_expired() {
        let (_scratch, conn) = open_migrated();
        let clock_at = UtcTimestamp::from_unix_nanos(1_000);
        let mut past = draft("anthropic", "model-a", TokenClass::Input, "1.00");
        past.effective_start = UtcDate::parse("2026-01-01").unwrap();
        past.effective_end = Some(UtcDate::parse("2026-02-01").unwrap());
        let mut future = draft("anthropic", "model-b", TokenClass::Input, "2.00");
        future.effective_start = UtcDate::parse("2099-01-01").unwrap();
        let open_ended = draft("anthropic", "model-c", TokenClass::Input, "3.00");
        insert(&conn, &[past, future, open_ended], clock_at).expect("insert must work");

        let effective = effective_at(&conn, UtcDate::parse("2026-07-01").unwrap().start())
            .expect("read must work");
        let models: Vec<&str> = effective.iter().map(|c| c.draft.model.as_str()).collect();
        assert_eq!(
            models,
            vec!["model-c"],
            "only the open-ended card is effective"
        );
    }

    #[test]
    fn the_effective_interval_boundary_is_start_inclusive_end_exclusive() {
        let (_scratch, conn) = open_migrated();
        let clock_at = UtcTimestamp::from_unix_nanos(1_000);
        let mut card = draft("anthropic", "model-a", TokenClass::Input, "1.00");
        card.effective_start = UtcDate::parse("2026-06-24").unwrap();
        card.effective_end = Some(UtcDate::parse("2026-08-31").unwrap());
        insert(&conn, std::slice::from_ref(&card), clock_at).expect("insert must work");

        let on_start_day = effective_at(&conn, UtcDate::parse("2026-06-24").unwrap().start())
            .expect("read must work");
        assert_eq!(
            on_start_day.len(),
            1,
            "the start day is inside the interval"
        );
        let last_day = effective_at(&conn, UtcDate::parse("2026-08-30").unwrap().start())
            .expect("read must work");
        assert_eq!(last_day.len(), 1, "the day before the end is inside");
        let end_day = effective_at(&conn, UtcDate::parse("2026-08-31").unwrap().start())
            .expect("read must work");
        assert_eq!(end_day.len(), 0, "the end day is the first day outside");
    }

    #[test]
    fn history_orders_by_vendor_model_class_then_start() {
        let (_scratch, conn) = open_migrated();
        let clock_at = UtcTimestamp::from_unix_nanos(1_000);
        let mut older = draft("anthropic", "claude-fable-5", TokenClass::Input, "10.00");
        older.effective_start = UtcDate::parse("2026-06-24").unwrap();
        let mut newer = draft("anthropic", "claude-fable-5", TokenClass::Input, "12.00");
        newer.effective_start = UtcDate::parse("2026-09-01").unwrap();
        let other_vendor = draft("openai", "gpt-5.6", TokenClass::Input, "5.00");
        insert(
            &conn,
            &[newer.clone(), other_vendor.clone(), older.clone()],
            clock_at,
        )
        .expect("insert must work");

        let cards = history(&conn).expect("read must work");
        let keys: Vec<(String, String, i64)> = cards
            .iter()
            .map(|c| {
                (
                    c.draft.vendor.clone(),
                    c.draft.model.clone(),
                    c.draft.effective_start.start().unix_nanos(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                (
                    "anthropic".into(),
                    "claude-fable-5".into(),
                    older.effective_start.start().unix_nanos()
                ),
                (
                    "anthropic".into(),
                    "claude-fable-5".into(),
                    newer.effective_start.start().unix_nanos()
                ),
                (
                    "openai".into(),
                    "gpt-5.6".into(),
                    other_vendor.effective_start.start().unix_nanos()
                ),
            ],
            "superseded records appear in interval order, not insertion order"
        );
    }
}
