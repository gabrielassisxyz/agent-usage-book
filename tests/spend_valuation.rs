//! Comprehensive unit, integration, and contract tests for `aub spend --value api-list` (`aub-wyu.3`).

use std::collections::BTreeMap;

use agent_usage_book::domain::money::{Money, Usd};
use agent_usage_book::domain::provenance::{
    DerivationId, EvidenceId, QuerySemantics, RateCardId, WitnessId,
};
use agent_usage_book::domain::rate_card::{
    BillingBasis, CurrencyCode, Publication, RateCard, RateCardDraft, ReviewDuePolicy, TokenClass,
};
use agent_usage_book::domain::time::{UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{
    spend_json, spend_json_with_explain, validate_spend_report_json,
};
use agent_usage_book::presentation::render::{
    ExplainMode, render_spend_report, render_spend_report_with_explain,
};
use agent_usage_book::report::{
    IngestSummary, LedgerGeneration, ReportMetadata, SpendGroup, SpendGroupProvenance, SpendReport,
};
use agent_usage_book::valuation::{RateBook, ValuationOutcome};

#[allow(clippy::too_many_arguments)]
fn helper_card(
    id: i64,
    vendor: &str,
    model: &str,
    token_class: TokenClass,
    rate_micros: i64,
    currency: CurrencyCode,
    start: &str,
    end: Option<&str>,
    review_due: ReviewDuePolicy,
) -> RateCard {
    RateCard {
        id,
        imported_at: UtcTimestamp::from_unix_nanos(100),
        draft: RateCardDraft {
            vendor: vendor.to_string(),
            model: model.to_string(),
            token_class,
            rate_micros,
            currency,
            billing_basis: BillingBasis::PerMillionTokens,
            effective_start: UtcDate::parse(start).expect("valid start date"),
            effective_end: end.map(|d| UtcDate::parse(d).expect("valid end date")),
            publication: Publication {
                source: Some("https://pricing.vendor.example".to_string()),
                published_at: None,
            },
            review_due,
        },
    }
}

fn helper_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageVector {
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

fn sample_rate_book() -> RateBook {
    RateBook::new(vec![
        helper_card(
            1,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Input,
            3_000_000,
            CurrencyCode::Usd,
            "2026-06-24",
            None,
            ReviewDuePolicy::None,
        ),
        helper_card(
            2,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Output,
            15_000_000,
            CurrencyCode::Usd,
            "2026-06-24",
            None,
            ReviewDuePolicy::None,
        ),
        helper_card(
            3,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::CacheRead,
            300_000,
            CurrencyCode::Usd,
            "2026-06-24",
            None,
            ReviewDuePolicy::None,
        ),
        helper_card(
            4,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::CacheWrite5m,
            3_750_000,
            CurrencyCode::Usd,
            "2026-06-24",
            None,
            ReviewDuePolicy::None,
        ),
    ])
}

/// 1. Integration: unvalued spend run produces a complete report with no rate-card lookup (Criterion 1 & 7).
#[test]
fn integration_unvalued_spend_with_no_rate_cards_produces_complete_report() {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    let since = UtcDate::parse("2026-08-25").unwrap();
    let until = UtcDate::parse("2026-08-26").unwrap();
    let usage = helper_usage(1000, 500, 200, 100);

    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![EvidenceId::new("ev-1")],
        vec![],
        QuerySemantics::new("day", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&manifest);
    let key = LogicalName::new("day=2026-08-25");
    let group = SpendGroup::new(
        key.clone(),
        usage,
        Provenance::new(["transcripts/claude-code/session.jsonl".to_string()]),
        derivation_id,
    );
    assert!(group.valuation.is_none());

    let node = agent_usage_book::report::ProvenanceNode::new(
        vec![EvidenceId::new("ev-1")],
        vec![],
        QuerySemantics::new("day", "none"),
        1,
        1,
        agent_usage_book::report::ValueArithmetic::Sum,
    );
    let group_prov = vec![SpendGroupProvenance::new(key, node)];
    let metadata = ReportMetadata::new(now, now, LedgerGeneration::new(1), None);
    assert!(metadata.rate_card_version.is_none());

    let ingest = IngestSummary {
        refresh_attempted: false,
        refresh_failure: None,
        files_read: 1,
        files_skipped_before_window: 0,
        unreadable_files: vec![],
        quarantined_by_class: BTreeMap::new(),
        replayed_occurrences: 0,
        collisions: 0,
        without_identity: 0,
        heuristic_identities: 0,
        undated_events: 0,
        events_outside_window: 0,
        events_in_window: 1,
    };
    let report = SpendReport::new(metadata, since, until, vec![group], group_prov, ingest);

    // Text rendering: unvalued run omits valuation header clause and monetary columns
    let text = render_spend_report(&report);
    assert!(
        text.contains(
            "spend from 2026-08-25 to 2026-08-26 (UTC days, end exclusive), grouped by day"
        )
    );
    assert!(!text.contains("valued at API list-price equivalent"));
    assert!(!text.contains("API list-price equivalent"));
    assert!(text.contains("day=2026-08-25  input 1000 tokens · output 500 tokens · cache read 200 tokens · cache write 100 tokens (complete)"));

    // JSON rendering: unvalued run omits api_list_price_equivalent and rate_card_version
    let run = RunId::from_string("run-unvalued".to_string());
    let json_str = spend_json(&report, run);
    assert!(!json_str.contains("api_list_price_equivalent"));
    assert!(!json_str.contains("rate_card_version"));
    validate_spend_report_json(&json_str)
        .expect("unvalued spend JSON must validate against schema");

    // In-memory assemble_canonical with no rate cards
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    agent_usage_book::store::migrate::run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &agent_usage_book::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    let window = agent_usage_book::report::spend::SpendWindow::starting(since, 1).unwrap();
    let assembled = agent_usage_book::report::spend::assemble_canonical(
        &conn,
        window,
        now,
        vec![agent_usage_book::report::SpendGrouping::Day],
        false,
        None,
        None,
        agent_usage_book::report::spend::CreditReporting::NotRequested,
    )
    .expect("unvalued spend with no rate cards must produce a complete report");
    assert!(assembled.metadata.rate_card_version.is_none());
}

/// 2. Integration: unavailable valuation renders unavailable form while other dimensions stay intact (Criterion 2).
#[test]
fn integration_requested_valuation_unavailable_renders_unavailable_form() {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    let since = UtcDate::parse("2026-08-25").unwrap();
    let until = UtcDate::parse("2026-08-26").unwrap();
    let usage = helper_usage(10_000, 5_000, 1_000, 2_000);

    // Rate book with input/output rates but MISSING cache_write rate
    let book = RateBook::new(vec![
        helper_card(
            1,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Input,
            3_000_000,
            CurrencyCode::Usd,
            "2026-06-24",
            None,
            ReviewDuePolicy::None,
        ),
        helper_card(
            2,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Output,
            15_000_000,
            CurrencyCode::Usd,
            "2026-06-24",
            None,
            ReviewDuePolicy::None,
        ),
    ]);

    let val_outcome = agent_usage_book::valuation::value_usage_vector::<Usd>(
        &book,
        "anthropic",
        "claude-3-5-sonnet",
        since,
        &usage,
    );
    assert!(matches!(val_outcome, ValuationOutcome::Incomplete { .. }));

    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![EvidenceId::new("ev-1")],
        vec![WitnessId::RateCard(RateCardId::new("rate-card-2026-06-24"))],
        QuerySemantics::new("day", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&manifest);
    let key = LogicalName::new("day=2026-08-25");
    let group = SpendGroup::new(
        key.clone(),
        usage,
        Provenance::new(["transcripts/claude-code/session.jsonl".to_string()]),
        derivation_id,
    )
    .with_valuation(Some(val_outcome));

    let node = agent_usage_book::report::ProvenanceNode::new(
        vec![EvidenceId::new("ev-1")],
        vec![WitnessId::RateCard(RateCardId::new("rate-card-2026-06-24"))],
        QuerySemantics::new("day", "none"),
        1,
        1,
        agent_usage_book::report::ValueArithmetic::Sum,
    );
    let group_prov = vec![SpendGroupProvenance::new(key, node)];
    let metadata = ReportMetadata::new(now, now, LedgerGeneration::new(1), None)
        .with_rate_card_version(book.version());

    let ingest = IngestSummary::default();
    let report = SpendReport::new(metadata, since, until, vec![group], group_prov, ingest);

    // Text rendering: renders unavailable form and keeps other dimensions intact
    let text = render_spend_report(&report);
    assert!(text.contains("valued at API list-price equivalent"));
    assert!(text.contains(
        "input 10000 tokens · output 5000 tokens · cache read 1000 tokens · cache write 2000 tokens"
    ));
    assert!(text.contains("API list-price equivalent unavailable"));
    assert!(text.contains("(complete)"));
    // Neither prints a monetary zero for the missing rate
    assert!(!text.contains("$0.00"));

    // JSON rendering: api_list_price_equivalent carries status unavailable and known_price_subtotal
    let run = RunId::from_string("run-unavailable".to_string());
    let json_str = spend_json(&report, run);
    assert!(json_str.contains("\"api_list_price_equivalent\":{\"status\":\"unavailable\",\"known_price_subtotal\":{\"value\":\"0.11\",\"unit\":\"usd\"}}"));
    validate_spend_report_json(&json_str).expect("unavailable valuation JSON must validate");
}

/// 3. Unit: rate-card version present in report metadata and under `--explain` (Criterion 4).
#[test]
fn unit_rate_card_version_in_metadata_and_under_explain() {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    let since = UtcDate::parse("2026-08-25").unwrap();
    let until = UtcDate::parse("2026-08-26").unwrap();
    let usage = helper_usage(1000, 500, 0, 0);

    let book = sample_rate_book();
    let rc_ver = book.version().expect("rate book version must exist");
    assert_eq!(rc_ver.as_str(), "rate-card-2026-06-24");

    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![EvidenceId::new("ev-1")],
        vec![WitnessId::RateCard(rc_ver.clone())],
        QuerySemantics::new("day", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&manifest);
    let key = LogicalName::new("day=2026-08-25");
    let val_outcome = agent_usage_book::valuation::value_usage_vector::<Usd>(
        &book,
        "anthropic",
        "claude-3-5-sonnet",
        since,
        &usage,
    );
    let group = SpendGroup::new(
        key.clone(),
        usage,
        Provenance::new(["session.jsonl".to_string()]),
        derivation_id,
    )
    .with_valuation(Some(val_outcome));

    let node = agent_usage_book::report::ProvenanceNode::new(
        vec![EvidenceId::new("ev-1")],
        vec![WitnessId::RateCard(rc_ver.clone())],
        QuerySemantics::new("day", "none"),
        1,
        1,
        agent_usage_book::report::ValueArithmetic::Sum,
    );
    let group_prov = vec![SpendGroupProvenance::new(key, node)];
    let metadata = ReportMetadata::new(now, now, LedgerGeneration::new(1), None)
        .with_rate_card_version(Some(rc_ver));

    let report = SpendReport::new(
        metadata,
        since,
        until,
        vec![group],
        group_prov,
        IngestSummary::default(),
    );

    // 1. In report metadata
    assert_eq!(
        report
            .metadata
            .rate_card_version
            .as_ref()
            .map(|r| r.as_str()),
        Some("rate-card-2026-06-24")
    );

    // 2. Under --explain in text
    let explain_summary = render_spend_report_with_explain(&report, ExplainMode::Summary);
    assert!(explain_summary.contains("rate card: rate-card-2026-06-24"));

    let explain_full = render_spend_report_with_explain(&report, ExplainMode::Full);
    assert!(explain_full.contains("rate card: rate-card-2026-06-24"));

    // 3. Under --explain in JSON
    let run = RunId::from_string("run-explain".to_string());
    let json_explain = spend_json_with_explain(&report, run, ExplainMode::Summary);
    assert!(json_explain.contains("\"rate_card\":\"rate-card-2026-06-24\""));
    assert!(json_explain.contains("\"rate_card_version\":\"rate-card-2026-06-24\""));
    validate_spend_report_json(&json_explain).expect("explain JSON must validate");
}

/// 4. Unit: stale rate card reported by doctor and noted on a valued report (Criterion 5).
#[test]
fn unit_stale_rate_card_reported_by_doctor_and_noted_on_valued_report() {
    let cards = vec![helper_card(
        1,
        "anthropic",
        "claude-sonnet-5",
        TokenClass::Input,
        3_000_000,
        CurrencyCode::Usd,
        "2026-06-24",
        Some("2026-08-31"),
        ReviewDuePolicy::On(UtcDate::parse("2026-08-31").unwrap()),
    )];
    let book = RateBook::new(cards);

    // 1. Before review-due date: not stale
    let date_before = UtcDate::parse("2026-08-30").unwrap();
    assert!(book.stale_cards(date_before).is_empty());

    // 2. On or after review-due date: stale
    let date_after = UtcDate::parse("2026-09-01").unwrap();
    let stale = book.stale_cards(date_after);
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].draft.vendor, "anthropic");
    assert_eq!(stale[0].draft.model, "claude-sonnet-5");
    assert_eq!(
        stale[0].draft.review_due,
        ReviewDuePolicy::On(UtcDate::parse("2026-08-31").unwrap())
    );

    // 3. Noted on valued spend report
    let now = UtcTimestamp::from_unix_nanos(2_000);
    let since = UtcDate::parse("2026-08-25").unwrap();
    let until = UtcDate::parse("2026-08-26").unwrap();
    let usage = helper_usage(100, 100, 0, 0);
    let group = SpendGroup::new(
        LogicalName::new("day=2026-08-25"),
        usage,
        Provenance::new(["s.jsonl".to_string()]),
        DerivationId::from_manifest(
            &agent_usage_book::domain::provenance::ProvenanceManifest::new(
                vec![],
                vec![],
                QuerySemantics::new("day", "none"),
            ),
        ),
    )
    .with_valuation(Some(ValuationOutcome::Complete(
        agent_usage_book::valuation::ApiListPriceEquivalent::new(Money::<Usd>::from_micros(10_000)),
    )));

    let stale_note =
        "rate card review is due (configured review-due date 2026-08-31 has passed)".to_string();
    let report = SpendReport::new(
        ReportMetadata::new(now, now, LedgerGeneration::new(1), None)
            .with_rate_card_version(book.version()),
        since,
        until,
        vec![group],
        vec![],
        IngestSummary::default(),
    )
    .with_stale_rate_card_note(Some(stale_note));

    let text = render_spend_report(&report);
    assert!(text.contains(
        "note: rate card review is due (configured review-due date 2026-08-31 has passed)"
    ));
    assert!(text.contains("valued at API list-price equivalent"));

    let json_str = spend_json(&report, RunId::from_string("run-stale".to_string()));
    assert!(json_str.contains("\"stale_rate_card_note\":\"rate card review is due (configured review-due date 2026-08-31 has passed)\""));
    validate_spend_report_json(&json_str).expect("stale note JSON must validate");
}

/// 5. Integration: grouping composes with valuation and reconciles totals (Criterion 6).
#[test]
fn integration_grouping_composed_with_valuation_reconciles_totals() {
    let book = sample_rate_book();
    let date = UtcDate::parse("2026-08-25").unwrap();

    // Event 1 (session 1, project A): 100k input ($0.30), 10k output ($0.15) => $0.45 (450_000 micros)
    let u1 = helper_usage(100_000, 10_000, 0, 0);
    let v1 = agent_usage_book::valuation::value_usage_vector::<Usd>(
        &book,
        "anthropic",
        "claude-3-5-sonnet",
        date,
        &u1,
    );
    assert_eq!(
        v1,
        ValuationOutcome::Complete(agent_usage_book::valuation::ApiListPriceEquivalent::new(
            Money::<Usd>::from_micros(450_000)
        ))
    );

    // Event 2 (session 2, project B): 200k input ($0.60), 20k output ($0.30) => $0.90 (900_000 micros)
    let u2 = helper_usage(200_000, 20_000, 0, 0);
    let v2 = agent_usage_book::valuation::value_usage_vector::<Usd>(
        &book,
        "anthropic",
        "claude-3-5-sonnet",
        date,
        &u2,
    );
    assert_eq!(
        v2,
        ValuationOutcome::Complete(agent_usage_book::valuation::ApiListPriceEquivalent::new(
            Money::<Usd>::from_micros(900_000)
        ))
    );

    // Parent group (Day): u1 + u2 => 300k input ($0.90), 30k output ($0.45) => $1.35 (1_350_000 micros)
    let u_parent = helper_usage(300_000, 30_000, 0, 0);
    let v_combined = v1.clone().combine(v2.clone());
    assert_eq!(
        v_combined,
        ValuationOutcome::Complete(agent_usage_book::valuation::ApiListPriceEquivalent::new(
            Money::<Usd>::from_micros(1_350_000)
        ))
    );

    let dummy_manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![],
        vec![],
        QuerySemantics::new("day", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&dummy_manifest);

    let child_1 = SpendGroup::new(
        LogicalName::new("day=2026-08-25 / session=s1"),
        u1,
        Provenance::new(["s1.jsonl".to_string()]),
        derivation_id,
    )
    .with_valuation(Some(v1));

    let child_2 = SpendGroup::new(
        LogicalName::new("day=2026-08-25 / session=s2"),
        u2,
        Provenance::new(["s2.jsonl".to_string()]),
        derivation_id,
    )
    .with_valuation(Some(v2));

    let parent = SpendGroup::new(
        LogicalName::new("day=2026-08-25"),
        u_parent,
        Provenance::new(["s1.jsonl".to_string(), "s2.jsonl".to_string()]),
        derivation_id,
    )
    .with_valuation(Some(v_combined))
    .with_children(vec![child_1, child_2]);

    // Check usage totals reconcile
    let child_input_sum: u64 = parent
        .children
        .iter()
        .map(|c| c.usage.known().input().value())
        .sum();
    let child_output_sum: u64 = parent
        .children
        .iter()
        .map(|c| c.usage.known().output().value())
        .sum();
    assert_eq!(child_input_sum, parent.usage.known().input().value());
    assert_eq!(child_output_sum, parent.usage.known().output().value());

    // Check valuation totals reconcile
    let child_micros_sum: i64 = parent
        .children
        .iter()
        .map(|c| match c.valuation.as_ref().unwrap() {
            ValuationOutcome::Complete(equiv) => equiv.micros(),
            _ => panic!("expected complete valuation"),
        })
        .sum();
    let parent_micros = match parent.valuation.as_ref().unwrap() {
        ValuationOutcome::Complete(equiv) => equiv.micros(),
        _ => panic!("expected complete valuation"),
    };
    assert_eq!(child_micros_sum, parent_micros);
    assert_eq!(parent_micros, 1_350_000);
}

/// 6. Unit: column header and JSON field both reading `API list-price equivalent` (Criterion 3).
#[test]
fn unit_column_header_and_json_field_both_say_api_list_price_equivalent() {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    let since = UtcDate::parse("2026-08-25").unwrap();
    let until = UtcDate::parse("2026-08-26").unwrap();
    let usage = helper_usage(100_000, 10_000, 0, 0);

    let dummy_manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![],
        vec![],
        QuerySemantics::new("day", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&dummy_manifest);
    let group = SpendGroup::new(
        LogicalName::new("day=2026-08-25"),
        usage,
        Provenance::new(["s.jsonl".to_string()]),
        derivation_id,
    )
    .with_valuation(Some(ValuationOutcome::Complete(
        agent_usage_book::valuation::ApiListPriceEquivalent::new(Money::<Usd>::from_micros(
            450_000,
        )),
    )));

    let report = SpendReport::new(
        ReportMetadata::new(now, now, LedgerGeneration::new(1), None)
            .with_rate_card_version(Some(RateCardId::new("rate-card-2026-06-24"))),
        since,
        until,
        vec![group],
        vec![],
        IngestSummary::default(),
    );

    // 1. Text column header and field name
    let text = render_spend_report(&report);
    assert!(
        text.contains("valued at API list-price equivalent"),
        "header must state API list-price equivalent"
    );
    assert!(
        text.contains("API list-price equivalent $0.45"),
        "group line must state API list-price equivalent"
    );

    // 2. JSON field name
    let run = RunId::from_string("run-naming".to_string());
    let json_str = spend_json(&report, run);
    assert!(
        json_str.contains("\"api_list_price_equivalent\":{\"value\":\"0.45\",\"unit\":\"usd\"}"),
        "JSON field must be api_list_price_equivalent with usd unit"
    );

    // 3. No forbidden terms in any output
    for forbidden in [
        "actual cost",
        "actual_cost",
        "savings",
        "counterfactual cost",
    ] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "text output must not contain forbidden term '{forbidden}'"
        );
        assert!(
            !json_str.to_lowercase().contains(forbidden),
            "JSON output must not contain forbidden term '{forbidden}'"
        );
    }
}
