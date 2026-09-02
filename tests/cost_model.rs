//! Test suite for the first cost model and its per-term provenance (`aub-ai3.3`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::cost_model::convert;
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::ids::{AdapterVersion, BillingSemanticsId};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    TokenKind, UsageVector,
};
use agent_usage_book::evidence::{CoverageCompleteness, Derivation, EvidenceQuality};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::cost_model::{
    TermDerivationMethod, anthropic_claude_messages_v1, load_active_at, seed_initial_cost_model,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aub-cost-model-test-{}-{}",
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

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageVector {
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

fn open_migrated() -> (ScratchDir, rusqlite::Connection) {
    let scratch = ScratchDir::new();
    let db_path = scratch.path().join("cost_model_test.db");
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).expect("db must open");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
    run_migrations(&mut conn, &registry(), None, &clock).expect("migrations must apply");
    (scratch, conn)
}

/// 1. Unit: completeness checking against a usage vector containing all four known kinds.
#[test]
fn completeness_checking_against_usage_vector_with_all_four_known_kinds() {
    let published_at = UtcTimestamp::from_unix_nanos(1_717_200_000_000_000_000); // 2024-06-01
    let model = anthropic_claude_messages_v1(published_at);

    assert!(model.is_complete());
    assert_eq!(model.missing_token_kinds(), Vec::<TokenKind>::new());
    assert_eq!(model.terms().len(), 4);

    // 100k input (3.0 credits/M -> 300,000 micros)
    // 20k output (15.0 credits/M -> 300,000 micros)
    // 50k cache read (0.30 credits/M -> 15,000 micros)
    // 10k cache write (3.75 credits/M -> 37,500 micros)
    // Hand-computed expected total: 300,000 + 300,000 + 15,000 + 37,500 = 652,500 micros (0.6525 credits)
    let usage = sample_usage(100_000, 20_000, 50_000, 10_000);
    let derivation = convert(&model, &usage);

    match derivation {
        Derivation::Available(qualified) => {
            let (credits, coverage, quality, provenance) = qualified.into_parts();
            assert_eq!(credits, Credits::from_micros(652_500));
            assert_eq!(coverage, CoverageCompleteness::Complete);
            assert_eq!(quality, EvidenceQuality::Measured);
            assert!(
                provenance
                    .sources()
                    .contains("cost-model:anthropic-claude-messages-v1")
            );
        }
        Derivation::Unavailable { missing, .. } => {
            panic!("expected Available conversion, but missing: {missing:?}");
        }
    }
}

/// 2. Unit: a term rejected when it carries no derivation method, since a coefficient
/// with unknown provenance is not evidence.
#[test]
fn term_derivation_method_requires_valid_known_provenance() {
    // Valid derivation methods parse cleanly.
    assert_eq!(
        "published_billing_semantics"
            .parse::<TermDerivationMethod>()
            .unwrap(),
        TermDerivationMethod::PublishedBillingSemantics
    );
    assert_eq!(
        "controlled_experiment"
            .parse::<TermDerivationMethod>()
            .unwrap(),
        TermDerivationMethod::ControlledExperiment
    );
    assert_eq!(
        "assumed".parse::<TermDerivationMethod>().unwrap(),
        TermDerivationMethod::Assumed
    );

    // Unknown derivation methods fail closed.
    assert!("unknown_method".parse::<TermDerivationMethod>().is_err());
    assert!("".parse::<TermDerivationMethod>().is_err());
    assert!("none".parse::<TermDerivationMethod>().is_err());
}

/// 3. Unit: no term populated with a placeholder or a zero standing in for an unknown value,
/// asserted over the seeded model.
#[test]
fn seeded_model_has_no_zero_or_placeholder_terms() {
    let published_at = UtcTimestamp::from_unix_nanos(1_717_200_000_000_000_000);
    let model = anthropic_claude_messages_v1(published_at);

    for kind in TokenKind::ALL {
        let term = model
            .term(kind)
            .unwrap_or_else(|| panic!("seeded model must carry term for {kind:?}"));
        assert!(
            term.coefficient().micros_per_million_tokens() > 0,
            "term for {kind:?} has zero or negative coefficient"
        );
        assert_eq!(
            term.derivation_method(),
            TermDerivationMethod::PublishedBillingSemantics,
            "seeded model term for {kind:?} must have published billing semantics derivation"
        );
    }

    assert!(model.validity().valid_until() > model.validity().valid_from());
    assert_eq!(model.published_at(), published_at);
}

/// 4. Integration: the activation lifecycle recorded, with the model queryable as active
/// from that instant.
#[test]
fn activation_lifecycle_recorded_and_queryable_from_instant() {
    let (_scratch, mut conn) = open_migrated();
    let t_before = UtcTimestamp::from_unix_nanos(1_000_000_000);
    let t_activation = UtcTimestamp::from_unix_nanos(2_000_000_000);
    let t_after = UtcTimestamp::from_unix_nanos(3_000_000_000);

    // Before activation, no model is active.
    let before = load_active_at(&conn, t_before).expect("query must succeed");
    assert!(before.is_none());

    // Seed/activate initial model at t_activation.
    let seeded = seed_initial_cost_model(&mut conn, t_activation).expect("seeding must succeed");
    assert_eq!(seeded.id().as_str(), "anthropic-claude-messages-v1");

    // At t_before, still None.
    let before_act = load_active_at(&conn, t_before).expect("query must succeed");
    assert!(before_act.is_none());

    // Exactly at t_activation and at t_after, the seeded model is active.
    let at_act = load_active_at(&conn, t_activation)
        .expect("query must succeed")
        .expect("must be active at activation instant");
    assert_eq!(at_act.id().as_str(), "anthropic-claude-messages-v1");
    assert!(at_act.is_complete());

    let after_act = load_active_at(&conn, t_after)
        .expect("query must succeed")
        .expect("must be active after activation instant");
    assert_eq!(after_act.id().as_str(), "anthropic-claude-messages-v1");

    // Idempotent second seeding at t_after returns the active model without error.
    let reseeded = seed_initial_cost_model(&mut conn, t_after).expect("reseeding must succeed");
    assert_eq!(reseeded.id().as_str(), "anthropic-claude-messages-v1");
}

/// 5. Unit: the billing-semantics identifier set and distinct from the adapter version.
#[test]
fn billing_semantics_identifier_distinct_from_adapter_version() {
    let published_at = UtcTimestamp::from_unix_nanos(1_717_200_000_000_000_000);
    let model = anthropic_claude_messages_v1(published_at);

    let billing_id = model.billing_semantics_id();
    let adapter_version = AdapterVersion::new("0.1.0");

    assert_eq!(
        billing_id,
        &BillingSemanticsId::new("anthropic-messages-subscription-v1")
    );
    assert_ne!(
        billing_id.as_str(),
        adapter_version.as_str(),
        "billing semantics identifier must be distinct from adapter version"
    );
}

/// 6. Conversion: unknown components fail closed with Unavailable.
#[test]
fn conversion_fails_closed_on_unknown_components() {
    let published_at = UtcTimestamp::from_unix_nanos(1_717_200_000_000_000_000);
    let model = anthropic_claude_messages_v1(published_at);

    let mut unknown = BTreeMap::new();
    unknown.insert("audio_input".to_string(), TokenCount::new(100));

    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(10_000),
            OutputTokens::new(5_000),
            CacheReadTokens::new(1_000),
            CacheWriteTokens::new(500),
        ),
        unknown,
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );

    match convert(&model, &usage) {
        Derivation::Unavailable { missing, .. } => {
            let names: BTreeSet<String> = missing.iter().map(|f| f.as_str().to_string()).collect();
            assert!(names.contains("unknown component: audio_input"));
        }
        Derivation::Available(_) => {
            panic!("expected Unavailable derivation for unknown component");
        }
    }
}
