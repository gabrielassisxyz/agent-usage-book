//! Rate-card integration tests (`aub-wyu.1`): the rate book parses, the store
//! persists it immutably, re-import is a visible no-op, and the effective
//! interval answers "which rate is true today".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::rate_card::{
    BillingBasis, CurrencyCode, RateCardDraft, ReviewDuePolicy, TokenClass,
};
use agent_usage_book::domain::time::{FakeClock, UtcDate, UtcTimestamp};
use agent_usage_book::rate_book;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::rate_card::{self, ImportSummary};

fn crate_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh scratch directory under the system temp dir, removed on drop. A
/// file database rather than `:memory:`: the connection policy requires WAL,
/// which an in-memory database cannot report.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aub-rate-card-integration-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).expect("scratch dir must be creatable");
        ScratchDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

/// The scratch must outlive the connection, so both travel together.
fn open_migrated() -> (ScratchDir, rusqlite::Connection) {
    let scratch = ScratchDir::new();
    let mut conn = agent_usage_book::store::connection::open(
        &scratch.path().join("rate-card.db"),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: agent_usage_book::domain::time::MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("scratch database must open");
    run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .expect("migrations must run");
    (scratch, conn)
}

/// The existing price table, imported. The assertion is the bead's integration
/// criterion: the date comments became structured metadata, and every card
/// lands fully dated.
#[test]
fn importing_the_existing_price_table_produces_dated_records() {
    let text = std::fs::read_to_string(crate_root().join("tests/fixtures/rate-book/rates.toml"))
        .expect("rate book fixture must be readable");
    let book = rate_book::parse(&text).expect("the existing price table must parse");
    assert_eq!(book.cards.len(), 55, "the fixture carries the full table");

    let (_scratch, conn) = open_migrated();
    let summary = rate_card::insert(&conn, &book.cards, UtcTimestamp::from_unix_nanos(1_000_000))
        .expect("import must persist");
    assert_eq!(
        summary,
        ImportSummary {
            cards_added: 55,
            cards_unchanged: 0
        }
    );

    // Structured metadata: every card is fully sourced, because this book was
    // written with its publication references. Missing provenance is the other
    // path and is covered by the explicit-absence test below.
    let cards = rate_card::history(&conn).expect("history must read");
    assert_eq!(cards.len(), 55);
    for card in &cards {
        assert!(
            card.draft.publication.fully_sourced(),
            "card {} must carry full provenance",
            card.id
        );
        assert!(
            card.draft.rate_micros > 0,
            "no zero rates arrive from the book"
        );
    }

    // The introductory sonnet rows carry the review-due date the expiry defines.
    let intro: Vec<_> = cards
        .iter()
        .filter(|c| c.draft.effective_end.is_some())
        .collect();
    assert_eq!(intro.len(), 5, "one introductory row per token class");
    for card in intro {
        assert_eq!(
            card.draft.review_due,
            ReviewDuePolicy::On(UtcDate::parse("2026-08-31").unwrap()),
            "the review-due policy is the expiry date"
        );
        assert_eq!(
            card.draft.effective_end.map(UtcDate::iso),
            Some("2026-08-31".to_string())
        );
    }
}

#[test]
fn reimporting_the_existing_price_table_is_visibly_a_no_op() {
    let text = std::fs::read_to_string(crate_root().join("tests/fixtures/rate-book/rates.toml"))
        .expect("rate book fixture must be readable");
    let book = rate_book::parse(&text).expect("the existing price table must parse");

    let (_scratch, conn) = open_migrated();
    rate_card::insert(&conn, &book.cards, UtcTimestamp::from_unix_nanos(1_000))
        .expect("first import must persist");
    let again = rate_card::insert(&conn, &book.cards, UtcTimestamp::from_unix_nanos(2_000))
        .expect("re-import must not fail");
    assert_eq!(
        again,
        ImportSummary {
            cards_added: 0,
            cards_unchanged: 55
        },
        "a re-import reports unchanged, never silently grows history"
    );
    assert_eq!(rate_card::count(&conn).expect("count must read"), 55);
}

#[test]
fn a_missing_publication_reference_is_recorded_as_missing() {
    let (_scratch, conn) = open_migrated();
    let draft = RateCardDraft {
        vendor: "anthropic".to_string(),
        model: "claude-fable-5".to_string(),
        token_class: TokenClass::Input,
        rate_micros: 10_000_000,
        currency: CurrencyCode::Usd,
        billing_basis: BillingBasis::PerMillionTokens,
        effective_start: UtcDate::parse("2026-06-24").unwrap(),
        effective_end: None,
        publication: agent_usage_book::domain::rate_card::Publication {
            source: None,
            published_at: None,
        },
        review_due: ReviewDuePolicy::None,
    };
    rate_card::insert(
        &conn,
        std::slice::from_ref(&draft),
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .expect("import must persist");

    let cards = rate_card::history(&conn).expect("history must read");
    assert_eq!(cards.len(), 1);
    assert!(
        !cards[0].draft.publication.fully_sourced(),
        "the record must carry the absence, not present itself as sourced"
    );
}

/// The effective interval answers day-granularity questions: a rate effective
/// from 2026-06-24 is true on that day and on the day before an expiry, and a
/// superseded rate is not effective once its successor starts.
#[test]
fn the_effective_book_is_the_one_true_today() {
    let (_scratch, conn) = open_migrated();
    let clock_at = UtcTimestamp::from_unix_nanos(1_000);

    // The introductory price and the standard price hand off at the expiry
    // day: the intro covers 2026-06-24 to 2026-08-31 exclusive, the standard
    // rate starts on 2026-08-31. Two records for one vendor, model and token
    // class are never effective at once, so valuation never has to pick a
    // winner between two simultaneous prices for the same key.
    let mut standard = RateCardDraft {
        vendor: "anthropic".to_string(),
        model: "claude-sonnet-5".to_string(),
        token_class: TokenClass::Input,
        rate_micros: 3_000_000,
        currency: CurrencyCode::Usd,
        billing_basis: BillingBasis::PerMillionTokens,
        effective_start: UtcDate::parse("2026-08-31").unwrap(),
        effective_end: None,
        publication: agent_usage_book::domain::rate_card::Publication {
            source: Some("claude-api reference".into()),
            published_at: Some(UtcDate::parse("2026-06-24").unwrap().start()),
        },
        review_due: ReviewDuePolicy::None,
    };
    let intro = RateCardDraft {
        rate_micros: 2_000_000,
        effective_start: UtcDate::parse("2026-06-24").unwrap(),
        effective_end: Some(UtcDate::parse("2026-08-31").unwrap()),
        review_due: ReviewDuePolicy::On(UtcDate::parse("2026-08-31").unwrap()),
        ..standard.clone()
    };
    rate_card::insert(&conn, &[standard.clone(), intro], clock_at).expect("import must persist");

    let on_intro_day =
        rate_card::effective_at(&conn, UtcDate::parse("2026-07-01").unwrap().start())
            .expect("read must work");
    let rates: Vec<i64> = on_intro_day.iter().map(|c| c.draft.rate_micros).collect();
    assert_eq!(
        rates,
        vec![2_000_000],
        "mid-window the introductory rate is the one effective price"
    );

    let on_handoff_day =
        rate_card::effective_at(&conn, UtcDate::parse("2026-08-31").unwrap().start())
            .expect("read must work");
    let rates: Vec<i64> = on_handoff_day.iter().map(|c| c.draft.rate_micros).collect();
    assert_eq!(
        rates,
        vec![3_000_000],
        "the expiry day belongs to the standard rate: the intro interval is end-exclusive and the standard interval is start-inclusive"
    );

    let after_expiry =
        rate_card::effective_at(&conn, UtcDate::parse("2026-09-01").unwrap().start())
            .expect("read must work");
    let rates: Vec<i64> = after_expiry.iter().map(|c| c.draft.rate_micros).collect();
    assert_eq!(
        rates,
        vec![3_000_000],
        "past the expiry only the standard rate survives"
    );

    let before_book = rate_card::effective_at(&conn, UtcDate::parse("2026-06-23").unwrap().start())
        .expect("read must work");
    assert!(
        before_book.is_empty(),
        "nothing is effective before the book starts"
    );

    // History keeps both, with their intervals, after the expiry.
    assert_eq!(
        rate_card::history(&conn).expect("history must read").len(),
        2
    );
}

/// Planted negative: a naive import would let a re-import create duplicates
/// through the NULL-comparison hole in a plain UNIQUE constraint. The content
/// index exists to close exactly that hole; this is the card shape that would
/// have slipped through it.
#[test]
fn two_fully_open_cards_do_not_duplicate_on_reimport() {
    let (_scratch, conn) = open_migrated();
    let draft = RateCardDraft {
        vendor: "anthropic".to_string(),
        model: "claude-fable-5".to_string(),
        token_class: TokenClass::Output,
        rate_micros: 50_000_000,
        currency: CurrencyCode::Usd,
        billing_basis: BillingBasis::PerMillionTokens,
        effective_start: UtcDate::parse("2026-06-24").unwrap(),
        effective_end: None,
        publication: agent_usage_book::domain::rate_card::Publication {
            source: None,
            published_at: None,
        },
        review_due: ReviewDuePolicy::None,
    };
    let first = rate_card::insert(
        &conn,
        std::slice::from_ref(&draft),
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .expect("first import must persist");
    assert_eq!(first.cards_added, 1);
    let second = rate_card::insert(
        &conn,
        std::slice::from_ref(&draft),
        UtcTimestamp::from_unix_nanos(2_000),
    )
    .expect("re-import must not fail");
    assert_eq!(
        second.cards_added, 0,
        "the open-ended no-provenance card is unchanged"
    );
    assert_eq!(second.cards_unchanged, 1);
}

/// Immutability holds against direct SQL, not only against the repository: the
/// triggers refuse UPDATE and DELETE, so no future code path can quietly
/// rewrite a price history.
#[test]
fn direct_update_and_delete_are_refused_by_the_table() {
    let (_scratch, conn) = open_migrated();
    let draft = RateCardDraft {
        vendor: "anthropic".to_string(),
        model: "claude-fable-5".to_string(),
        token_class: TokenClass::Input,
        rate_micros: 10_000_000,
        currency: CurrencyCode::Usd,
        billing_basis: BillingBasis::PerMillionTokens,
        effective_start: UtcDate::parse("2026-06-24").unwrap(),
        effective_end: None,
        publication: agent_usage_book::domain::rate_card::Publication {
            source: None,
            published_at: None,
        },
        review_due: ReviewDuePolicy::None,
    };
    rate_card::insert(
        &conn,
        std::slice::from_ref(&draft),
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .expect("import must persist");

    let update = conn.execute("UPDATE rate_card SET rate_micros = 999", []);
    assert!(update.is_err(), "UPDATE must be refused");
    let delete = conn.execute("DELETE FROM rate_card", []);
    assert!(delete.is_err(), "DELETE must be refused");
    assert_eq!(
        rate_card::history(&conn).expect("history must read").len(),
        1,
        "the old record stays readable after the refused mutation"
    );
}
