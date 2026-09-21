//! Migration 0043: percent-of-window rate cards (`aub-8vpc`).
//!
//! A rate card can state a subscription price as percentage points of a named
//! quota window per million tokens, and only as a declared estimate. Three
//! nullable columns carry what such a card declares: `window` (`five_hour` or
//! `seven_day`), `unit` (`percentage_points`) and `quality` (`estimate`). All
//! three absent is an ordinary money card; all three present is an estimate.
//!
//! The existing `currency` column carries the rate's denomination for every
//! card, which for a percent-of-window row is the same spelling `unit` holds.
//! That is not redundancy to be cleaned away: the column is `NOT NULL` and
//! reading a row's denomination must not depend on which of several columns a
//! decoder happens to look at first, so one column states it for every basis
//! and the trigger below refuses a row where the two disagree. `CurrencyCode`
//! does not parse `percentage_points`, so a percent-of-window row can never be
//! decoded as money even by a caller that ignores `unit`.
//!
//! All three columns join the content-dedup index, so an estimate and a money
//! card for the same vendor, model and class are distinct content and a
//! re-import of either reports unchanged. The immutability triggers from
//! migration 0011 apply unchanged.
//!
//! Recovery: the framework is forward-only (PLAN.md section 11.4), so there is
//! no down step to run. The manual reversal is statements against a copy of the
//! database, and `reversal_statements` below is that text, exercised by this
//! module's own test so the documented path cannot rot into something that no
//! longer runs: drop the pairing trigger, drop the content index, recreate it
//! without the three `COALESCE` lines (migration 0037 holds that text), then
//! drop the three columns. Rows of the new basis have to be removed first, and
//! removing them means recreating the table, because `rate_card` refuses every
//! `DELETE` by design.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 43;

const ADD_RATE_CARD_WINDOW_ESTIMATE: &str = r#"
ALTER TABLE rate_card ADD COLUMN "window" TEXT
    CHECK ("window" IS NULL OR length("window") > 0);
ALTER TABLE rate_card ADD COLUMN unit TEXT
    CHECK (unit IS NULL OR length(unit) > 0);
ALTER TABLE rate_card ADD COLUMN quality TEXT
    CHECK (quality IS NULL OR length(quality) > 0);

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
    COALESCE(schedule_hours, ''),
    COALESCE("window", ''),
    COALESCE(unit, ''),
    COALESCE(quality, '')
);

CREATE TRIGGER rate_card_window_estimate_pairing
    BEFORE INSERT ON rate_card
BEGIN
    SELECT CASE
        WHEN (NEW."window" IS NULL) != (NEW.unit IS NULL)
          OR (NEW."window" IS NULL) != (NEW.quality IS NULL)
        THEN RAISE(ABORT, 'rate_card window, unit and quality must all be present or all absent')
    END;
    SELECT CASE
        WHEN NEW.unit IS NOT NULL AND NEW.currency != NEW.unit
        THEN RAISE(ABORT, 'rate_card currency must repeat unit on a percent-of-window card')
    END;
END;"#;

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_RATE_CARD_WINDOW_ESTIMATE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot add the rate card window-estimate columns: {error}"
            ))
        })
}

/// The documented manual reversal, as the exact statements to run against a
/// copy of the database. Not wired into the forward-only runner: it exists so
/// the reversal this module's header promises is a thing that can be executed
/// and tested rather than prose nobody has run. Test-only: the forward runner
/// never reaches for it, and a reversal statement compiled into the shipped
/// binary would be one an operator could run by accident.
#[cfg(test)]
pub fn reversal_statements() -> &'static str {
    r#"
DROP TRIGGER rate_card_window_estimate_pairing;
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
ALTER TABLE rate_card DROP COLUMN "window";
ALTER TABLE rate_card DROP COLUMN unit;
ALTER TABLE rate_card DROP COLUMN quality;"#
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        // Additive only: three nullable columns, an index rebuild over them and
        // one insert trigger. No irreplaceable row is rewritten, so the
        // verified-backup guard does not apply, and every existing row reads
        // back as the money card it already was.
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: false,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::rate_card::{
        BillingBasis, CardQuality, CurrencyCode, Publication, QuotaWindowKind, RateCardDraft,
        RateDenomination, RateUnit, ReviewDuePolicy, TokenClass, WindowEstimate,
    };
    use crate::domain::time::{Clock, FakeClock, UtcDate, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy};
    use crate::store::migrate::run_migrations;
    use crate::store::rate_card;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-rate-card-window-migration-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open_migrated() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let mut conn = crate::store::connection::open(
            &scratch.0.join("rate-card-window.db"),
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

    fn money_draft() -> RateCardDraft {
        RateCardDraft {
            vendor: "anthropic".to_string(),
            model: "claude-fable-5".to_string(),
            token_class: TokenClass::Input,
            rate_micros: 10_000_000,
            denomination: RateDenomination::Money(CurrencyCode::Usd),
            billing_basis: BillingBasis::PerMillionTokens,
            window_estimate: None,
            effective_start: UtcDate::parse("2026-06-24").unwrap(),
            effective_end: None,
            schedule: None,
            publication: Publication {
                source: None,
                published_at: None,
            },
            review_due: ReviewDuePolicy::None,
        }
    }

    fn estimate_draft() -> RateCardDraft {
        RateCardDraft {
            rate_micros: 850_000,
            denomination: RateDenomination::Points(RateUnit::PercentagePoints),
            billing_basis: BillingBasis::PercentOfWindowPerMillionTokens,
            window_estimate: Some(WindowEstimate {
                window: QuotaWindowKind::FiveHour,
                unit: RateUnit::PercentagePoints,
                quality: CardQuality::Estimate,
            }),
            publication: Publication {
                source: Some("operator measurement notes".to_string()),
                published_at: None,
            },
            ..money_draft()
        }
    }

    /// The round trip the acceptance criterion names: an estimate card written
    /// through the repository reads back with its window, unit and quality
    /// intact, and beside a money card for the same vendor, model and class.
    #[test]
    fn an_estimate_card_round_trips_beside_a_money_card() {
        let (_scratch, conn) = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        rate_card::insert(&conn, &[money_draft(), estimate_draft()], clock.now())
            .expect("both cards must insert");

        let cards = rate_card::history(&conn).expect("history must read");
        assert_eq!(cards.len(), 2);
        let estimate = cards
            .iter()
            .find(|card| card.draft.window_estimate.is_some())
            .expect("the estimate card must read back");
        assert_eq!(estimate.draft, estimate_draft());
        assert_eq!(
            estimate.draft.denomination,
            RateDenomination::Points(RateUnit::PercentagePoints)
        );
    }

    /// The content index covers the three new columns: the same estimate
    /// re-imports as unchanged, and an estimate for the other window is new
    /// content rather than a duplicate.
    #[test]
    fn the_content_index_separates_two_windows_and_dedups_one() {
        let (_scratch, conn) = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        let first = rate_card::insert(&conn, std::slice::from_ref(&estimate_draft()), clock.now())
            .expect("first import must insert");
        assert_eq!(first.cards_added, 1);

        let again = rate_card::insert(&conn, std::slice::from_ref(&estimate_draft()), clock.now())
            .expect("re-import must not fail");
        assert_eq!(again.cards_added, 0, "a re-import must be visibly a no-op");
        assert_eq!(again.cards_unchanged, 1);

        let mut weekly = estimate_draft();
        weekly.window_estimate = Some(WindowEstimate {
            window: QuotaWindowKind::SevenDay,
            unit: RateUnit::PercentagePoints,
            quality: CardQuality::Estimate,
        });
        let second = rate_card::insert(&conn, std::slice::from_ref(&weekly), clock.now())
            .expect("the seven-day card must insert");
        assert_eq!(second.cards_added, 1);
    }

    /// The pairing is a property of the table, not of the repository: a row
    /// with two of the three columns is refused even by direct SQL.
    #[test]
    fn a_half_present_window_estimate_is_refused_by_the_table() {
        let (_scratch, conn) = open_migrated();
        let half = conn.execute(
            "INSERT INTO rate_card (
                vendor, model, token_class, rate_micros, currency, billing_basis,
                effective_start, imported_at, \"window\", unit
            ) VALUES ('anthropic', 'm', 'input', 1, 'percentage_points',
                      'percent_of_window_per_million_tokens', '2026-06-24', 1,
                      'five_hour', 'percentage_points')",
            [],
        );
        assert!(half.is_err(), "a row without quality must be refused");

        let disagreeing = conn.execute(
            "INSERT INTO rate_card (
                vendor, model, token_class, rate_micros, currency, billing_basis,
                effective_start, imported_at, \"window\", unit, quality
            ) VALUES ('anthropic', 'm', 'input', 1, 'USD',
                      'percent_of_window_per_million_tokens', '2026-06-24', 1,
                      'five_hour', 'percentage_points', 'estimate')",
            [],
        );
        assert!(
            disagreeing.is_err(),
            "a row whose currency contradicts its unit must be refused"
        );
    }

    /// The documented reversal runs, and a money card survives it: the header
    /// promises a manual down step, and a promise nobody executes is prose.
    #[test]
    fn the_documented_reversal_drops_the_three_columns() {
        let (_scratch, conn) = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        rate_card::insert(&conn, std::slice::from_ref(&money_draft()), clock.now())
            .expect("the money card must insert");

        conn.execute_batch(super::reversal_statements())
            .expect("the documented reversal must run");

        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('rate_card')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for dropped in ["window", "unit", "quality"] {
            assert!(
                !columns.iter().any(|column| column == dropped),
                "{dropped} must be gone after the reversal, columns: {columns:?}"
            );
        }
        let surviving: i64 = conn
            .query_row("SELECT count(*) FROM rate_card", [], |row| row.get(0))
            .unwrap();
        assert_eq!(surviving, 1, "the money card survives the reversal");
    }
}
