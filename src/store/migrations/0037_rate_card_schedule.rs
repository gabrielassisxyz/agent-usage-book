//! Migration 0037: scheduled rate cards (`aub-pwtn`).
//!
//! A rate card can carry a time-of-day window inside which it applies, so
//! valuation can price an event at the card in force at its instant rather
//! than at the card effective on its date. The window is two nullable
//! columns: `schedule_days`, the three-letter day names in ISO order joined
//! by `,` (e.g. `mon,tue,wed,thu,fri`), and `schedule_hours`, the half-open
//! UTC minute window as `HH:MM-HH:MM` (e.g. `12:00-18:00`). Both absent is
//! the default card for its (vendor, model, class); both present is a card
//! that applies only inside the window. One half present without the other
//! is unrepresentable and refused by the pairing trigger below.
//!
//! Both columns join the content-dedup index, so a scheduled card and its
//! default are distinct content and a re-import of either reports unchanged.
//! The exclusivity between two cards (at most one default per triple and
//! date, no overlapping scheduled windows) is a book-level rule enforced by
//! the importer (`rate_book::parse`), not by this schema: the index
//! distinguishes content, it does not arbitrate which card is in force.
//! The pairing, by contrast, is a property of the table the way migration
//! 0011 treats immutability: an insert trigger refuses a row with exactly
//! one of the two columns present, so no future code path can land a
//! half-present window even by bypassing the repository.
//!
//! Recovery: the framework is forward-only (PLAN.md section 11.4), so there
//! is no down step to run. The manual reversal is statements against a
//! copy of the database: `DROP TRIGGER rate_card_schedule_pairing`,
//! `DROP INDEX idx_rate_card_content`, recreate the index without the two
//! `COALESCE` lines (migration 0011 holds the text), then
//! `ALTER TABLE rate_card DROP COLUMN schedule_days` and
//! `ALTER TABLE rate_card DROP COLUMN schedule_hours`. A book without any
//! `schedule` values behaves exactly as before except that overlapping
//! unscheduled rows are now refused at import.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 37;

const ADD_RATE_CARD_SCHEDULE: &str = "
ALTER TABLE rate_card ADD COLUMN schedule_days TEXT
    CHECK (schedule_days IS NULL OR length(schedule_days) > 0);
ALTER TABLE rate_card ADD COLUMN schedule_hours TEXT
    CHECK (schedule_hours IS NULL OR length(schedule_hours) > 0);

DROP INDEX idx_rate_card_content;
CREATE UNIQUE INDEX idx_rate_card_content ON rate_card (
    vendor,
    model,
    token_class,
    rate_micros,
    currency,
    billing_basis,
    effective_start,
    COALESCE(effective_end, ''),
    COALESCE(source, ''),
    COALESCE(published_at, -1),
    COALESCE(review_due, ''),
    COALESCE(schedule_days, ''),
    COALESCE(schedule_hours, '')
);

CREATE TRIGGER rate_card_schedule_pairing
    BEFORE INSERT ON rate_card
BEGIN
    SELECT CASE WHEN (NEW.schedule_days IS NULL) != (NEW.schedule_hours IS NULL)
        THEN RAISE(ABORT, 'rate_card schedule_days and schedule_hours must both be present or both absent')
    END;
END;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_RATE_CARD_SCHEDULE)
        .map_err(|e| Error::Store(format!("cannot add the rate card schedule columns: {e}")))
}

/// This step, for the registry.
///
/// Additive only: two nullable columns and an index rebuild over them, so no
/// irreplaceable data is at risk and the verified-backup guard does not
/// apply. Existing rows read back with no schedule, which is the default
/// card, so a pre-schedule book values exactly as before.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: false,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::rate_card::{
        BillingBasis, CurrencyCode, Publication, RateCardDraft, ReviewDuePolicy, Schedule,
        TokenClass,
    };
    use crate::domain::time::{Clock, FakeClock, UtcDate, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy};
    use crate::store::migrate::run_migrations;
    use crate::store::rate_card;

    /// A fresh scratch directory under the system temp dir. A file database
    /// rather than `:memory:`: the connection policy requires WAL, which an
    /// in-memory database cannot report.
    struct ScratchDir(std::path::PathBuf);

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-rate-card-schedule-migration-test-{}-{}",
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

    fn draft(rate_micros: i64, schedule: Option<Schedule>) -> RateCardDraft {
        RateCardDraft {
            vendor: "ollama".to_string(),
            model: "deepseek-v4-flash".to_string(),
            token_class: TokenClass::Input,
            rate_micros,
            currency: CurrencyCode::Usd,
            billing_basis: BillingBasis::PerMillionTokens,
            effective_start: UtcDate::parse("2026-09-07").unwrap(),
            effective_end: None,
            schedule,
            publication: Publication {
                source: Some("https://ollama.com/pricing".to_string()),
                published_at: Some(UtcDate::parse("2026-09-07").unwrap().start()),
            },
            review_due: ReviewDuePolicy::None,
        }
    }

    fn weekday_peak() -> Schedule {
        Schedule::new(&[1, 2, 3, 4, 5], 12 * 60, 18 * 60).expect("peak window must build")
    }

    /// Pre-schedule rows survive the migration and read back as default
    /// cards, and the schedule columns round-trip through the store.
    #[test]
    fn schedule_columns_round_trip_and_default_absent() {
        let (_scratch, conn) = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));

        let default = draft(220_000, None);
        let peak = draft(440_000, Some(weekday_peak()));
        let summary = rate_card::insert(&conn, &[default, peak], clock.now())
            .expect("scheduled import must persist");
        assert_eq!(summary.cards_added, 2);

        let cards = rate_card::history(&conn).expect("history must read");
        assert_eq!(cards.len(), 2);
        let stored_default = cards
            .iter()
            .find(|card| card.draft.rate_micros == 220_000)
            .expect("default must be stored");
        assert_eq!(stored_default.draft.schedule, None);
        let stored_peak = cards
            .iter()
            .find(|card| card.draft.rate_micros == 440_000)
            .expect("peak must be stored");
        assert_eq!(stored_peak.draft.schedule, Some(weekday_peak()));
        assert_eq!(
            stored_peak.draft.schedule.map(|window| window.describe()),
            Some("peak mon-fri 12:00-18:00 UTC".to_string())
        );
    }

    /// The schedule columns are content: the same price with and without a
    /// window are two records, and re-importing either reports unchanged.
    #[test]
    fn schedule_columns_join_the_content_dedup_key() {
        let (_scratch, conn) = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));

        let peak = draft(220_000, Some(weekday_peak()));
        let first = rate_card::insert(&conn, std::slice::from_ref(&peak), clock.now())
            .expect("first import must insert");
        assert_eq!(first.cards_added, 1);
        let again = rate_card::insert(&conn, std::slice::from_ref(&peak), clock.now())
            .expect("re-import must not fail");
        assert_eq!(again.cards_added, 0);
        assert_eq!(again.cards_unchanged, 1);

        // Same price, no window: distinct content, a second record.
        let default = draft(220_000, None);
        let sibling = rate_card::insert(&conn, std::slice::from_ref(&default), clock.now())
            .expect("the default beside its peak must insert");
        assert_eq!(sibling.cards_added, 1);
        assert_eq!(
            rate_card::count(&conn).expect("count must read"),
            2,
            "the default and its peak are two records"
        );
    }

    /// A half-present window is refused by the pairing trigger: days without
    /// hours (and hours without days) cannot land.
    #[test]
    fn a_half_present_window_is_refused() {
        let (_scratch, conn) = open_migrated();
        let refused = conn.execute(
            "INSERT INTO rate_card (vendor, model, token_class, rate_micros, currency, billing_basis, effective_start, imported_at, schedule_days) \
             VALUES ('ollama', 'deepseek-v4-flash', 'input', 1, 'USD', 'per_million_tokens', '2026-09-07', 1, 'mon')",
            [],
        );
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("both be present or both absent"),
            "days without hours must violate the pairing trigger"
        );
    }
}
