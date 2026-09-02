//! Schema step: immutable dated rate cards (`aub-wyu.1`).
//!
//! A rate card is versioned reference data (PLAN.md sections 12.15, 25.3): a
//! corrected price creates a new record rather than mutating history, so
//! historical traffic stays valued at the rate effective at its event time even
//! after the vendor changes a price. Immutability is enforced mechanically the
//! way `meter_attempt` enforces it: triggers reject every `UPDATE` and `DELETE`,
//! and the repository exposes insert and read only.
//!
//! The uniqueness key covers every content field (everything but the row id and
//! the import stamp), expressed through a partial-index-safe `COALESCE` form
//! because SQLite compares NULLs as distinct inside a plain UNIQUE constraint,
//! which would let a re-import of an open-ended card grow the history. A
//! provenance change is content: re-importing a card whose publication reference
//! was later filled in creates a new record, and the old one stays readable.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 11;

const CREATE_RATE_CARD_TABLES: &str = "
CREATE TABLE rate_card (
    id INTEGER PRIMARY KEY,
    vendor TEXT NOT NULL CHECK (length(vendor) > 0),
    model TEXT NOT NULL CHECK (length(model) > 0),
    token_class TEXT NOT NULL CHECK (length(token_class) > 0),
    rate_micros INTEGER NOT NULL CHECK (rate_micros >= 0),
    currency TEXT NOT NULL CHECK (length(currency) > 0),
    billing_basis TEXT NOT NULL CHECK (length(billing_basis) > 0),
    effective_start TEXT NOT NULL CHECK (length(effective_start) > 0),
    effective_end TEXT,
    imported_at INTEGER NOT NULL,
    published_at INTEGER,
    source TEXT,
    review_due TEXT,
    CHECK (
        effective_end IS NULL OR length(effective_end) > 0
    ),
    CHECK (
        source IS NULL OR length(source) > 0
    ),
    CHECK (
        review_due IS NULL OR length(review_due) > 0
    )
) STRICT;

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
    COALESCE(review_due, '')
);

CREATE INDEX idx_rate_card_lookup ON rate_card (
    vendor,
    model,
    token_class,
    effective_start
);

CREATE TRIGGER rate_card_no_update
    BEFORE UPDATE ON rate_card
BEGIN
    SELECT RAISE(ABORT, 'rate_card is immutable: a corrected price is a new record');
END;

CREATE TRIGGER rate_card_no_delete
    BEFORE DELETE ON rate_card
BEGIN
    SELECT RAISE(ABORT, 'rate_card is immutable: history is never removed');
END;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_RATE_CARD_TABLES)
        .map_err(|error| Error::Store(format!("cannot create rate card tables: {error}")))
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::rate_card::{
        BillingBasis, CurrencyCode, Publication, RateCardDraft, ReviewDuePolicy, TokenClass,
    };
    use crate::domain::time::{Clock, FakeClock, UtcDate, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy};
    use crate::store::migrate::run_migrations;
    use crate::store::rate_card;

    fn open_migrated() -> rusqlite::Connection {
        let mut conn = crate::store::connection::open(
            std::path::Path::new(":memory:"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1_000),
            },
        )
        .expect("in-memory database must open");
        run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .expect("migrations must run");
        conn
    }

    fn draft(rate_micros: i64) -> RateCardDraft {
        RateCardDraft {
            vendor: "anthropic".to_string(),
            model: "claude-fable-5".to_string(),
            token_class: TokenClass::Input,
            rate_micros,
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
    fn reimporting_the_same_draft_is_reported_unchanged_not_added() {
        let conn = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));

        let first = rate_card::insert(&conn, std::slice::from_ref(&draft(10_000_000)), clock.now())
            .expect("first import must insert");
        assert_eq!(first.cards_added, 1);
        assert_eq!(first.cards_unchanged, 0);

        let second =
            rate_card::insert(&conn, std::slice::from_ref(&draft(10_000_000)), clock.now())
                .expect("re-import must not fail");
        assert_eq!(second.cards_added, 0, "a re-import must be visibly a no-op");
        assert_eq!(second.cards_unchanged, 1);
    }

    /// The null-handling half of the uniqueness key: two cards whose optional
    /// fields are absent must conflict on re-import, because SQLite compares
    /// NULLs as distinct inside a plain UNIQUE constraint and the content index
    /// exists precisely to prevent that.
    #[test]
    fn two_open_ended_cards_with_no_provenance_still_conflict() {
        let conn = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        rate_card::insert(&conn, std::slice::from_ref(&draft(5_000_000)), clock.now())
            .expect("first import must insert");
        let again = rate_card::insert(&conn, std::slice::from_ref(&draft(5_000_000)), clock.now())
            .expect("re-import must not fail");
        assert_eq!(again.cards_added, 0);
        assert_eq!(again.cards_unchanged, 1);
    }

    /// Immutability is a property of the table, not of the repository: even
    /// direct SQL is refused, so no future code path can quietly rewrite a
    /// price history.
    #[test]
    fn direct_update_and_delete_are_refused_by_the_table() {
        let conn = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        rate_card::insert(&conn, std::slice::from_ref(&draft(10_000_000)), clock.now())
            .expect("insert must work");

        let update = conn.execute("UPDATE rate_card SET rate_micros = 1", []);
        assert!(update.is_err(), "UPDATE must be refused by trigger");
        let delete = conn.execute("DELETE FROM rate_card", []);
        assert!(delete.is_err(), "DELETE must be refused by trigger");

        // The refusal is the immutability mechanism working; the record is
        // still readable and untouched.
        let cards = rate_card::history(&conn).expect("history must read");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].draft.rate_micros, 10_000_000);
    }

    /// A provenance change is content: the corrected record is a new row and
    /// the old one stays readable.
    #[test]
    fn a_provenance_correction_creates_a_new_record_and_keeps_the_old() {
        let conn = open_migrated();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        rate_card::insert(&conn, std::slice::from_ref(&draft(10_000_000)), clock.now())
            .expect("first import must insert");

        let mut corrected = draft(10_000_000);
        corrected.publication.source = Some("claude-api reference".to_string());
        let summary = rate_card::insert(&conn, std::slice::from_ref(&corrected), clock.now())
            .expect("correction import must insert");
        assert_eq!(summary.cards_added, 1);

        let cards = rate_card::history(&conn).expect("history must read");
        assert_eq!(cards.len(), 2, "the old record stays readable");
    }
}
