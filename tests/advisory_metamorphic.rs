//! Advisory metamorphic property suite (`aub-cab.5`, PLAN.md 26, 34.23).
//!
//! Rather than asserting fixed literal results, this suite asserts the metamorphic
//! relationships that must hold across the advisory and `can-run` system when one
//! input changes and all others remain fixed.
//!
//! Ten metamorphic laws carry the suite:
//! 1. More current remaining quota cannot worsen the margin.
//! 2. Increasing historical task consumption cannot improve the margin.
//! 3. Widening calibration uncertainty cannot narrow the advice interval.
//! 4. Adding a tighter applicable window cannot increase headroom.
//! 5. A window with more remaining percentage but less calibrated headroom must be
//!    capable of becoming the limiting constraint (the divergence case).
//! 6. Removing a window's calibration must move the verdict to unknown rather than
//!    letting the remaining windows answer alone.
//! 7. Removing the fresh meter must make current advice unavailable.
//! 8. Making calibration health suspect removes every current quantitative answer,
//!    returns the typed refusal state and prints no justified headroom or margin.
//!    (Conditional verdict-label case: `AMPLE` cannot survive).
//! 9. Adding estimated historical tasks does not improve the exact-evidence verdict.
//! 10. Adding an unknown token kind cannot shrink the consumption interval.
//!
//! Every property is implemented over generated inputs using a deterministic PRNG seeded
//! from a `u64`, reporting the generating inputs on failure so any violation is
//! reproducible from the test output alone. The suite runs with no network, no real
//! credentials and a fake clock.

use std::collections::{BTreeMap, BTreeSet};

use agent_usage_book::advice::headroom::WindowHeadroom;
use agent_usage_book::advice::historical_distribution::{
    AttributionCoverage, DistributionVerdict, ExclusionCounts, HistoricalDistributionConfig,
    Percentile, QuantileMethod, SelectionPeriod, TaskHistorySample, TaskPricing,
    build_group_reports,
};
use agent_usage_book::advice::verdict::{
    AmpleMarginMultiple, CanRunAssessment, CanRunHeadroomBound, CanRunVerdictConfig,
    CanRunVerdictLabel, TaskReferenceInput, assess_can_run,
};
use agent_usage_book::attribution::TaskKind;
use agent_usage_book::attribution::account_segment::AccountEvidenceClass;
use agent_usage_book::attribution::quality::{AttributionFraction, AttributionQualityFloor};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::freshness::StaleReason;
use agent_usage_book::domain::interval::Interval;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::window::{
    MeterWindow, ModelId, NominalWindowDuration, QuantizationSemantics, ReportedResolution,
    WindowScope, WindowSemanticKey,
};
use agent_usage_book::evidence::EvidenceQuality;
use agent_usage_book::presentation::render::render_can_run_report;
use agent_usage_book::report::can_run::{
    CanRunJoinInputs, CanRunMeterReadiness, CanRunOutcome, compose_can_run_report,
};
use agent_usage_book::report::models::{LedgerGeneration, ReportMetadata};
use agent_usage_book::report::provenance::ProvenanceGraph;
use proptest::prelude::*;
use test_support::rng::{Rng, Seed};

// -----------------------------------------------------------------------------------------
// Helper constructors & fixtures
// -----------------------------------------------------------------------------------------

fn credits(whole: i64) -> Credits {
    Credits::from_micros(whole * 1_000_000)
}

fn credits_from_micros(micros: i64) -> Credits {
    Credits::from_micros(micros)
}

fn fake_metadata() -> ReportMetadata {
    ReportMetadata::new(
        UtcTimestamp::from_unix_nanos(1_000_000_000),
        UtcTimestamp::from_unix_nanos(1_000_000_000),
        LedgerGeneration::new(1),
        None,
    )
}

fn make_window(key: &str, scope: WindowScope, used_ppm: i32) -> MeterWindow {
    MeterWindow::new(
        WindowSemanticKey::new(key),
        scope,
        QuotaUsed::new(QuotaFractionPpm::new(used_ppm).expect("valid used ppm")),
        ReportedResolution::new(QuotaFractionPpm::new(10_000).expect("valid resolution"))
            .expect("resolution valid"),
        QuantizationSemantics::RoundedToNearest,
        UtcTimestamp::from_unix_nanos(3_000_000_000),
        NominalWindowDuration::from_nanos(1_000_000_000),
    )
}

fn default_verdict_config(labels_enabled: bool) -> CanRunVerdictConfig {
    CanRunVerdictConfig {
        labels_enabled,
        ample_margin_multiple: AmpleMarginMultiple::new(2.0).expect("positive multiple"),
        headroom_bound: CanRunHeadroomBound::Low,
    }
}

fn default_dist_config() -> HistoricalDistributionConfig {
    HistoricalDistributionConfig {
        central_low: Percentile::new(25).expect("valid percentile"),
        central_high: Percentile::new(75).expect("valid percentile"),
        upper: Percentile::new(90).expect("valid percentile"),
        min_samples: 12,
        quantile_method: QuantileMethod::NearestRank,
        attribution_floor: AttributionQualityFloor::new(0.80).expect("valid floor"),
    }
}

fn fake_selection_period() -> SelectionPeriod {
    SelectionPeriod {
        start: UtcTimestamp::from_unix_nanos(1_000_000_000),
        end: UtcTimestamp::from_unix_nanos(2_000_000_000),
    }
}

// -----------------------------------------------------------------------------------------
// Law 1: More current remaining quota cannot worsen the margin.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law1Inputs {
    pub seed: u64,
    pub used_ppm_1: i32,
    pub used_ppm_2: i32,
    pub cal_micros: i64,
    pub task_low: i64,
    pub task_high: i64,
}

pub fn generate_law_1_inputs(rng: &mut Rng) -> Law1Inputs {
    let seed = rng.next_u64();
    // u1 > u2 implies remaining quota 2 >= remaining quota 1.
    let u1 = 200_000 + (rng.next_below(700_000) as i32);
    let delta = rng.next_below((u1 - 100_000) as u64) as i32;
    let u2 = u1 - delta; // u2 <= u1 -> remaining 2 >= remaining 1

    let cal_micros = 500 + (rng.next_below(5_000) as i64);
    let task_low = 10 + (rng.next_below(1_000) as i64);
    let task_width = rng.next_below(1_000) as i64;
    let task_high = task_low + task_width;

    Law1Inputs {
        seed,
        used_ppm_1: u1,
        used_ppm_2: u2,
        cal_micros,
        task_low,
        task_high,
    }
}

pub fn verify_law_1(inputs: &Law1Inputs) -> Result<(), String> {
    let window1 = make_window("w1", WindowScope::AccountWide, inputs.used_ppm_1);
    let window2 = make_window("w2", WindowScope::AccountWide, inputs.used_ppm_2);

    let rem1 = i64::from(window1.remaining_percentage_points().get());
    let rem2 = i64::from(window2.remaining_percentage_points().get());

    let h1 = rem1 * inputs.cal_micros;
    let h2 = rem2 * inputs.cal_micros;

    let eval1 = [WindowHeadroom::Known {
        window: &window1,
        headroom: Interval::new(credits(h1), credits(h1 + 50)).map_err(|e| format!("{e:?}"))?,
    }];
    let eval2 = [WindowHeadroom::Known {
        window: &window2,
        headroom: Interval::new(credits(h2), credits(h2 + 50)).map_err(|e| format!("{e:?}"))?,
    }];

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits(inputs.task_low),
            central_range: Interval::new(credits(inputs.task_low), credits(inputs.task_high))
                .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits(inputs.task_high),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true);
    let r1 = match assess_can_run(&eval1, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready for input 1, got {other:?}")),
    };
    let r2 = match assess_can_run(&eval2, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready for input 2, got {other:?}")),
    };

    let m1 = r1.per_window[0].margin;
    let m2 = r2.per_window[0].margin;

    if m2.lower().micros() < m1.lower().micros() {
        return Err(format!(
            "More remaining quota worsened margin lower bound: m1={m1:?}, m2={m2:?}"
        ));
    }
    if m2.upper().micros() < m1.upper().micros() {
        return Err(format!(
            "More remaining quota worsened margin upper bound: m1={m1:?}, m2={m2:?}"
        ));
    }

    Ok(())
}

pub fn check_law_1(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_1_inputs(&mut rng);
    if let Err(err) = verify_law_1(&inputs) {
        panic!("Law 1 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 2: Increasing historical consumption cannot improve the margin.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law2Inputs {
    pub seed: u64,
    pub head_low: i64,
    pub head_width: i64,
    pub task1_low: i64,
    pub task1_high: i64,
    pub raise_low: i64,
    pub raise_high: i64,
}

pub fn generate_law_2_inputs(rng: &mut Rng) -> Law2Inputs {
    let seed = rng.next_u64();
    let head_low = 100_000 + (rng.next_below(1_000_000) as i64);
    let head_width = rng.next_below(200_000) as i64;
    let task1_low = 10_000 + (rng.next_below(500_000) as i64);
    let task1_width = rng.next_below(100_000) as i64;
    let task1_high = task1_low + task1_width;

    let raise_low = rng.next_below(100_000) as i64;
    let raise_high = raise_low + (rng.next_below(100_000) as i64);

    Law2Inputs {
        seed,
        head_low,
        head_width,
        task1_low,
        task1_high,
        raise_low,
        raise_high,
    }
}

pub fn verify_law_2(inputs: &Law2Inputs) -> Result<(), String> {
    let window = make_window("w", WindowScope::AccountWide, 500_000);
    let headroom = Interval::new(
        credits_from_micros(inputs.head_low),
        credits_from_micros(inputs.head_low + inputs.head_width),
    )
    .map_err(|e| format!("{e:?}"))?;

    let eval = [WindowHeadroom::Known {
        window: &window,
        headroom,
    }];

    let task1 = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits_from_micros(inputs.task1_low),
            central_range: Interval::new(
                credits_from_micros(inputs.task1_low),
                credits_from_micros(inputs.task1_high),
            )
            .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits_from_micros(inputs.task1_high),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let task2_low = inputs.task1_low + inputs.raise_low;
    let task2_high = inputs.task1_high + inputs.raise_high;
    let task2 = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits_from_micros(task2_low),
            central_range: Interval::new(
                credits_from_micros(task2_low),
                credits_from_micros(task2_high),
            )
            .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits_from_micros(task2_high),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true);
    let r1 = match assess_can_run(&eval, &task1, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready 1, got {other:?}")),
    };
    let r2 = match assess_can_run(&eval, &task2, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready 2, got {other:?}")),
    };

    let m1 = r1.per_window[0].margin;
    let m2 = r2.per_window[0].margin;

    if m2.lower().micros() > m1.lower().micros() {
        return Err(format!(
            "Increasing consumption improved margin lower: m1={m1:?}, m2={m2:?}"
        ));
    }
    if m2.upper().micros() > m1.upper().micros() {
        return Err(format!(
            "Increasing consumption improved margin upper: m1={m1:?}, m2={m2:?}"
        ));
    }

    Ok(())
}

pub fn check_law_2(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_2_inputs(&mut rng);
    if let Err(err) = verify_law_2(&inputs) {
        panic!("Law 2 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 3: Widening calibration uncertainty cannot narrow the advice interval.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law3Inputs {
    pub seed: u64,
    pub c1_low: i64,
    pub c1_high: i64,
    pub c2_low: i64,
    pub c2_high: i64,
    pub remaining_points: i64,
}

pub fn generate_law_3_inputs(rng: &mut Rng) -> Law3Inputs {
    let seed = rng.next_u64();
    let c1_low = 20_000 + (rng.next_below(20_000) as i64);
    let c1_high = c1_low + (rng.next_below(20_000) as i64);

    let expand_left = rng.next_below(10_000) as i64;
    let expand_right = rng.next_below(10_000) as i64;
    let c2_low = c1_low - expand_left;
    let c2_high = c1_high + expand_right;

    let remaining_points = 10 + (rng.next_below(80) as i64);

    Law3Inputs {
        seed,
        c1_low,
        c1_high,
        c2_low,
        c2_high,
        remaining_points,
    }
}

pub fn verify_law_3(inputs: &Law3Inputs) -> Result<(), String> {
    let h1_low = inputs.remaining_points * inputs.c1_low;
    let h1_high = inputs.remaining_points * inputs.c1_high;
    let h2_low = inputs.remaining_points * inputs.c2_low;
    let h2_high = inputs.remaining_points * inputs.c2_high;

    let headroom1 = Interval::new(credits_from_micros(h1_low), credits_from_micros(h1_high))
        .map_err(|e| format!("{e:?}"))?;
    let headroom2 = Interval::new(credits_from_micros(h2_low), credits_from_micros(h2_high))
        .map_err(|e| format!("{e:?}"))?;

    let window = make_window("w", WindowScope::AccountWide, 500_000);
    let eval1 = [WindowHeadroom::Known {
        window: &window,
        headroom: headroom1,
    }];
    let eval2 = [WindowHeadroom::Known {
        window: &window,
        headroom: headroom2,
    }];

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits(50),
            central_range: Interval::new(credits(40), credits(80)).map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits(120),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true);
    let r1 = match assess_can_run(&eval1, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready 1, got {other:?}")),
    };
    let r2 = match assess_can_run(&eval2, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready 2, got {other:?}")),
    };

    let w_head1 = headroom1.width().micros();
    let w_head2 = headroom2.width().micros();
    if w_head2 < w_head1 {
        return Err(format!(
            "Widening calibration narrowed headroom interval: w1={w_head1}, w2={w_head2}"
        ));
    }

    let m1 = r1.per_window[0].margin.width().micros();
    let m2 = r2.per_window[0].margin.width().micros();
    if m2 < m1 {
        return Err(format!(
            "Widening calibration narrowed margin interval: m1={m1}, m2={m2}"
        ));
    }

    Ok(())
}

pub fn check_law_3(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_3_inputs(&mut rng);
    if let Err(err) = verify_law_3(&inputs) {
        panic!("Law 3 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 4: Adding a tighter applicable window cannot increase headroom.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law4Inputs {
    pub seed: u64,
    pub base_head_low: i64,
    pub base_head_high: i64,
    pub tighter_head_low: i64,
    pub tighter_head_high: i64,
}

pub fn generate_law_4_inputs(rng: &mut Rng) -> Law4Inputs {
    let seed = rng.next_u64();
    let base_head_low = 100_000 + (rng.next_below(500_000) as i64);
    let base_head_high = base_head_low + (rng.next_below(50_000) as i64);

    let tighter_head_low = 10_000 + (rng.next_below((base_head_low - 20_000) as u64) as i64);
    let tighter_head_high = tighter_head_low + (rng.next_below(20_000) as i64);

    Law4Inputs {
        seed,
        base_head_low,
        base_head_high,
        tighter_head_low,
        tighter_head_high,
    }
}

pub fn verify_law_4(inputs: &Law4Inputs) -> Result<(), String> {
    let w1 = make_window("w1", WindowScope::AccountWide, 500_000);
    let w_tighter = make_window(
        "w_tighter",
        WindowScope::ModelSpecific(ModelId::new("m")),
        500_000,
    );

    let h1 = Interval::new(
        credits_from_micros(inputs.base_head_low),
        credits_from_micros(inputs.base_head_high),
    )
    .map_err(|e| format!("{e:?}"))?;
    let h_tighter = Interval::new(
        credits_from_micros(inputs.tighter_head_low),
        credits_from_micros(inputs.tighter_head_high),
    )
    .map_err(|e| format!("{e:?}"))?;

    let eval_base = [WindowHeadroom::Known {
        window: &w1,
        headroom: h1,
    }];
    let eval_combined = [
        WindowHeadroom::Known {
            window: &w1,
            headroom: h1,
        },
        WindowHeadroom::Known {
            window: &w_tighter,
            headroom: h_tighter,
        },
    ];

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits_from_micros(5_000),
            central_range: Interval::new(credits_from_micros(4_000), credits_from_micros(6_000))
                .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits_from_micros(10_000),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true);
    let r_base = match assess_can_run(&eval_base, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready base, got {other:?}")),
    };
    let r_comb = match assess_can_run(&eval_combined, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready combined, got {other:?}")),
    };

    let base_limiting_entry = r_base
        .per_window
        .iter()
        .find(|a| a.window.semantic_key() == r_base.limiting_window.semantic_key())
        .ok_or_else(|| "missing base limiting window".to_string())?;
    let comb_limiting_entry = r_comb
        .per_window
        .iter()
        .find(|a| a.window.semantic_key() == r_comb.limiting_window.semantic_key())
        .ok_or_else(|| "missing comb limiting window".to_string())?;

    if comb_limiting_entry.headroom.lower().micros() > base_limiting_entry.headroom.lower().micros()
    {
        return Err(format!(
            "Adding tighter window increased limiting headroom: base={:?}, combined={:?}",
            base_limiting_entry.headroom, comb_limiting_entry.headroom
        ));
    }
    if comb_limiting_entry.margin.lower().micros() > base_limiting_entry.margin.lower().micros() {
        return Err(format!(
            "Adding tighter window increased limiting margin: base={:?}, combined={:?}",
            base_limiting_entry.margin, comb_limiting_entry.margin
        ));
    }

    Ok(())
}

pub fn check_law_4(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_4_inputs(&mut rng);
    if let Err(err) = verify_law_4(&inputs) {
        panic!("Law 4 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 5: Divergence case (higher-percentage window can be limiting constraint).
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law5Inputs {
    pub seed: u64,
    pub used_ppm_a: i32,
    pub used_ppm_b: i32,
    pub headroom_a: i64,
    pub headroom_b: i64,
}

pub fn generate_law_5_inputs(rng: &mut Rng) -> Law5Inputs {
    let seed = rng.next_u64();
    // Window A has LESS used ppm -> MORE remaining percentage (e.g. 70% remaining).
    // Window B has MORE used ppm -> LESS remaining percentage (e.g. 40% remaining).
    let used_ppm_a = 200_000 + (rng.next_below(200_000) as i32); // 200k..400k (60..80% remaining)
    let used_ppm_b = 500_000 + (rng.next_below(200_000) as i32); // 500k..700k (30..50% remaining)

    // But Window A has SMALLER headroom than Window B!
    let headroom_a = 50_000 + (rng.next_below(50_000) as i64); // 50k..100k
    let headroom_b = 200_000 + (rng.next_below(100_000) as i64); // 200k..300k

    Law5Inputs {
        seed,
        used_ppm_a,
        used_ppm_b,
        headroom_a,
        headroom_b,
    }
}

pub fn verify_law_5(inputs: &Law5Inputs) -> Result<(), String> {
    let model = ModelId::new("m5");
    let win_a = make_window("win_a", WindowScope::AccountWide, inputs.used_ppm_a);
    let win_b = make_window(
        "win_b",
        WindowScope::ModelSpecific(model),
        inputs.used_ppm_b,
    );

    let ha = Interval::new(
        credits_from_micros(inputs.headroom_a),
        credits_from_micros(inputs.headroom_a + 1_000),
    )
    .map_err(|e| format!("{e:?}"))?;
    let hb = Interval::new(
        credits_from_micros(inputs.headroom_b),
        credits_from_micros(inputs.headroom_b + 1_000),
    )
    .map_err(|e| format!("{e:?}"))?;

    let eval = [
        WindowHeadroom::Known {
            window: &win_a,
            headroom: ha,
        },
        WindowHeadroom::Known {
            window: &win_b,
            headroom: hb,
        },
    ];

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits_from_micros(10_000),
            central_range: Interval::new(credits_from_micros(8_000), credits_from_micros(12_000))
                .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits_from_micros(20_000),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true);
    let ready = match assess_can_run(&eval, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => return Err(format!("expected Ready, got {other:?}")),
    };

    if ready.limiting_window.semantic_key() != win_a.semantic_key() {
        return Err(format!(
            "Limiting window should be win_a (headroom={ha:?}), but got {}",
            ready.limiting_window.semantic_key().as_str()
        ));
    }

    if ready.lowest_percentage_window.semantic_key() != win_b.semantic_key() {
        return Err(format!(
            "Lowest percentage window should be win_b (used={}), but got {}",
            inputs.used_ppm_b,
            ready.lowest_percentage_window.semantic_key().as_str()
        ));
    }

    if !ready.windows_differ {
        return Err("windows_differ must be true for the divergence case".to_string());
    }

    Ok(())
}

pub fn check_law_5(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_5_inputs(&mut rng);
    if let Err(err) = verify_law_5(&inputs) {
        panic!("Law 5 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 6: Removing a window's calibration moves verdict to unknown rather than letting
// remaining windows answer alone.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law6Inputs {
    pub seed: u64,
    pub h_a: i64,
    pub h_b: i64,
}

pub fn generate_law_6_inputs(rng: &mut Rng) -> Law6Inputs {
    Law6Inputs {
        seed: rng.next_u64(),
        h_a: 100_000 + (rng.next_below(100_000) as i64),
        h_b: 200_000 + (rng.next_below(100_000) as i64),
    }
}

pub fn verify_law_6(inputs: &Law6Inputs) -> Result<(), String> {
    let win_a = make_window("win_a", WindowScope::AccountWide, 400_000);
    let win_b = make_window("win_b", WindowScope::AccountWide, 500_000);

    let ha = Interval::new(
        credits_from_micros(inputs.h_a),
        credits_from_micros(inputs.h_a + 5_000),
    )
    .map_err(|e| format!("{e:?}"))?;
    let hb = Interval::new(
        credits_from_micros(inputs.h_b),
        credits_from_micros(inputs.h_b + 5_000),
    )
    .map_err(|e| format!("{e:?}"))?;

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits_from_micros(10_000),
            central_range: Interval::new(credits_from_micros(8_000), credits_from_micros(12_000))
                .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits_from_micros(15_000),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };
    let config = default_verdict_config(true);

    // Both calibrated: answers Ready
    let both_calibrated = [
        WindowHeadroom::Known {
            window: &win_a,
            headroom: ha,
        },
        WindowHeadroom::Known {
            window: &win_b,
            headroom: hb,
        },
    ];
    let ready = assess_can_run(&both_calibrated, &task, &config);
    if !matches!(ready, CanRunAssessment::Ready(_)) {
        return Err("expected Ready when both are calibrated".to_string());
    }

    // Removing win_b calibration moves to Unknown
    let one_uncalibrated = [
        WindowHeadroom::Known {
            window: &win_a,
            headroom: ha,
        },
        WindowHeadroom::Unknown { window: &win_b },
    ];
    let refused = assess_can_run(&one_uncalibrated, &task, &config);
    match refused {
        CanRunAssessment::Unknown(unknown) => {
            if !unknown.missing.iter().any(|m| m.subject == "win_b") {
                return Err("missing fact did not name uncalibrated window win_b".to_string());
            }
        }
        other => {
            return Err(format!(
                "Removing a calibration must move verdict to Unknown, got {other:?}"
            ));
        }
    }

    Ok(())
}

pub fn check_law_6(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_6_inputs(&mut rng);
    if let Err(err) = verify_law_6(&inputs) {
        panic!("Law 6 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 7: Removing the fresh meter must make current advice unavailable.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law7Inputs {
    pub seed: u64,
    pub stale_reason: StaleReason,
}

pub fn generate_law_7_inputs(rng: &mut Rng) -> Law7Inputs {
    let seed = rng.next_u64();
    let reasons = [
        StaleReason::AgeExceeded,
        StaleReason::NoSuccessfulObservation,
        StaleReason::RateLimited,
        StaleReason::MalformedProviderResponse,
        StaleReason::SamplingGap,
        StaleReason::ClockAnomaly,
        StaleReason::CollectorInterrupted,
    ];
    let idx = (rng.next_below(reasons.len() as u64)) as usize;
    Law7Inputs {
        seed,
        stale_reason: reasons[idx],
    }
}

pub fn verify_law_7(inputs: &Law7Inputs) -> Result<(), String> {
    let model = ModelId::new("m7");
    let join_inputs = CanRunJoinInputs {
        metadata: fake_metadata(),
        task_kind: "task".to_string(),
        account: "acc".to_string(),
        model,
        meter: CanRunMeterReadiness::Stale {
            reason: inputs.stale_reason,
        },
        window_calibrations: BTreeMap::new(),
        cost_model_missing_token_classes: Vec::new(),
        plan_tier_mismatch: None,
        task: TaskReferenceInput {
            verdict: DistributionVerdict::Distribution {
                median: credits(100),
                central_range: Interval::new(credits(80), credits(120))
                    .map_err(|e| format!("{e:?}"))?,
                central_low_percentile: Percentile::new(25).expect("valid"),
                central_high_percentile: Percentile::new(75).expect("valid"),
                upper_reference: credits(150),
                upper_percentile: Percentile::new(90).expect("valid"),
                quantile_method: QuantileMethod::NearestRank,
            },
            sample_count: 20,
        },
        attribution: AttributionCoverage {
            fraction: AttributionFraction::new(90, 100),
            floor: AttributionQualityFloor::new(0.80).expect("valid"),
        },
        attribution_exclusions: ExclusionCounts::default(),
        attribution_selection_window: "w".to_string(),
        attribution_group: "g".to_string(),
        ample_margin_multiple: AmpleMarginMultiple::new(2.0).expect("valid"),
        headroom_bound: CanRunHeadroomBound::Low,
        provenance: ProvenanceGraph::default(),
    };

    let report = compose_can_run_report(join_inputs);
    match report.outcome {
        CanRunOutcome::Refused(refusal) => {
            if refusal.verdict != CanRunVerdictLabel::Unknown {
                return Err(format!(
                    "Expected Unknown verdict for stale meter, got {:?}",
                    refusal.verdict
                ));
            }
            if !refusal.missing.iter().any(|m| m.subject == "meter") {
                return Err("Missing facts must include meter".to_string());
            }
        }
        CanRunOutcome::Ready(_) => {
            return Err("Removing fresh meter must refuse advice, but returned Ready".to_string());
        }
    }

    Ok(())
}

pub fn check_law_7(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_7_inputs(&mut rng);
    if let Err(err) = verify_law_7(&inputs) {
        panic!("Law 7 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 8: Making calibration health suspect removes every current quantitative answer,
// returns typed refusal state and prints no justified headroom or margin.
// (Conditional verdict-label case: AMPLE cannot survive).
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law8Inputs {
    pub seed: u64,
    pub used_ppm: i32,
    pub task_credits: i64,
}

pub fn generate_law_8_inputs(rng: &mut Rng) -> Law8Inputs {
    Law8Inputs {
        seed: rng.next_u64(),
        used_ppm: 100_000 + (rng.next_below(500_000) as i32),
        task_credits: 10 + (rng.next_below(100) as i64),
    }
}

pub fn verify_law_8(inputs: &Law8Inputs) -> Result<(), String> {
    let window = make_window("win_suspect", WindowScope::AccountWide, inputs.used_ppm);

    // Suspect health produces an Unknown headroom
    let eval = [WindowHeadroom::Unknown { window: &window }];

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits(inputs.task_credits),
            central_range: Interval::new(
                credits(inputs.task_credits - 5),
                credits(inputs.task_credits + 5),
            )
            .map_err(|e| format!("{e:?}"))?,
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits(inputs.task_credits * 2),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true); // labels enabled
    let assessment = assess_can_run(&eval, &task, &config);

    match assessment {
        CanRunAssessment::Unknown(unknown) => {
            if !unknown.missing.iter().any(|m| m.subject == "win_suspect") {
                return Err("Missing fact must cite win_suspect".to_string());
            }
        }
        CanRunAssessment::Ready(ready) => {
            // Conditional verdict-label case: AMPLE cannot survive
            if ready
                .label_basis
                .as_ref()
                .is_some_and(|basis| basis.label == CanRunVerdictLabel::Ample)
            {
                return Err(
                    "AMPLE verdict survived suspect calibration health in label basis".to_string(),
                );
            }
            return Err("Suspect calibration must not produce a Ready assessment".to_string());
        }
        other => {
            return Err(format!(
                "Expected Unknown refusal for suspect calibration, got {other:?}"
            ));
        }
    }

    // Now test through compose_can_run_report and render_can_run_report
    // When calibration is missing or not current, it produces Refused and prints no headroom/margin
    let model = ModelId::new("m8");
    let win_meter = make_window(
        "win_suspect",
        WindowScope::ModelSpecific(model.clone()),
        inputs.used_ppm,
    );
    let join_inputs = CanRunJoinInputs {
        metadata: fake_metadata(),
        task_kind: "kind".to_string(),
        account: "acc".to_string(),
        model,
        meter: CanRunMeterReadiness::Fresh {
            windows: vec![win_meter],
            observed_age: Some(MonotonicDuration::from_seconds(10)),
        },
        window_calibrations: BTreeMap::new(), // no current calibration for this window
        cost_model_missing_token_classes: Vec::new(),
        plan_tier_mismatch: None,
        task,
        attribution: AttributionCoverage {
            fraction: AttributionFraction::new(100, 100),
            floor: AttributionQualityFloor::new(0.80).expect("valid"),
        },
        attribution_exclusions: ExclusionCounts::default(),
        attribution_selection_window: "w".to_string(),
        attribution_group: "g".to_string(),
        ample_margin_multiple: AmpleMarginMultiple::new(2.0).expect("valid"),
        headroom_bound: CanRunHeadroomBound::Low,
        provenance: ProvenanceGraph::default(),
    };

    let report = compose_can_run_report(join_inputs);
    let rendered = render_can_run_report(&report);

    if rendered.contains("headroom") {
        return Err("Rendered refusal text must not contain headroom intervals".to_string());
    }
    if rendered.contains("margin:") {
        return Err("Rendered refusal text must not contain margin intervals".to_string());
    }
    if rendered.contains("AMPLE") {
        return Err("Rendered refusal text must not contain AMPLE label".to_string());
    }

    Ok(())
}

pub fn check_law_8(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_8_inputs(&mut rng);
    if let Err(err) = verify_law_8(&inputs) {
        panic!("Law 8 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 9: Adding estimated historical tasks does not improve the exact-evidence verdict.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law9Inputs {
    pub seed: u64,
    pub measured_count: usize,
    pub estimated_count: usize,
}

pub fn generate_law_9_inputs(rng: &mut Rng) -> Law9Inputs {
    let seed = rng.next_u64();
    // Generate below or above min_samples (12)
    let measured_count = 5 + (rng.next_below(15) as usize); // 5..19
    let estimated_count = 1 + (rng.next_below(20) as usize); // 1..20
    Law9Inputs {
        seed,
        measured_count,
        estimated_count,
    }
}

pub fn verify_law_9(inputs: &Law9Inputs) -> Result<(), String> {
    let period = fake_selection_period();
    let config = default_dist_config();
    let kind = TaskKind::Task;

    let mut samples_before: Vec<TaskHistorySample<TaskKind>> = Vec::new();
    for i in 0..inputs.measured_count {
        samples_before.push(TaskHistorySample {
            group: kind,
            pricing: TaskPricing::Priced {
                credits: credits(50 + (i as i64)),
                quality: EvidenceQuality::Measured,
            },
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        });
    }

    let reports_before = build_group_reports(samples_before.clone(), period, &config);
    let r_before = reports_before
        .get(&kind)
        .ok_or_else(|| "missing before report".to_string())?;

    let mut samples_after = samples_before;
    for i in 0..inputs.estimated_count {
        samples_after.push(TaskHistorySample {
            group: kind,
            pricing: TaskPricing::Priced {
                credits: credits(10 + (i as i64)),
                quality: EvidenceQuality::Estimated {
                    methods: BTreeSet::new(),
                    uncertainty: None,
                },
            },
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        });
    }

    let reports_after = build_group_reports(samples_after, period, &config);
    let r_after = reports_after
        .get(&kind)
        .ok_or_else(|| "missing after report".to_string())?;

    if r_after.sample_count != r_before.sample_count {
        return Err(format!(
            "Adding estimated tasks changed eligible sample count: before={}, after={}",
            r_before.sample_count, r_after.sample_count
        ));
    }

    match (&r_before.verdict, &r_after.verdict) {
        (
            DistributionVerdict::InsufficientEvidence { min_samples: m1 },
            DistributionVerdict::InsufficientEvidence { min_samples: m2 },
        ) => {
            if m1 != m2 {
                return Err("min_samples changed".to_string());
            }
        }
        (
            DistributionVerdict::InsufficientEvidence { .. },
            DistributionVerdict::Distribution { .. },
        ) => {
            return Err(
                "Adding estimated tasks improperly upgraded InsufficientEvidence to Distribution!"
                    .to_string(),
            );
        }
        (
            DistributionVerdict::Distribution {
                median: med1,
                central_range: cr1,
                ..
            },
            DistributionVerdict::Distribution {
                median: med2,
                central_range: cr2,
                ..
            },
        ) => {
            if med1 != med2 || cr1 != cr2 {
                return Err(format!(
                    "Adding estimated tasks distorted distribution: before=({med1:?}, {cr1:?}), after=({med2:?}, {cr2:?})"
                ));
            }
        }
        (DistributionVerdict::Distribution { .. }, _) => {
            return Err("Adding estimated tasks degraded distribution to insufficient".to_string());
        }
    }

    if r_after.exclusions.estimated_tokens != inputs.estimated_count {
        return Err(format!(
            "Expected exclusions.estimated_tokens == {}, got {}",
            inputs.estimated_count, r_after.exclusions.estimated_tokens
        ));
    }

    Ok(())
}

pub fn check_law_9(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_9_inputs(&mut rng);
    if let Err(err) = verify_law_9(&inputs) {
        panic!("Law 9 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Law 10: Adding an unknown token kind cannot shrink the consumption interval.
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Law10Inputs {
    pub seed: u64,
    pub valid_count: usize,
    pub unknown_token_count: usize,
}

pub fn generate_law_10_inputs(rng: &mut Rng) -> Law10Inputs {
    let seed = rng.next_u64();
    let valid_count = 12 + (rng.next_below(15) as usize); // >= 12
    let unknown_token_count = 1 + (rng.next_below(10) as usize);
    Law10Inputs {
        seed,
        valid_count,
        unknown_token_count,
    }
}

pub fn verify_law_10(inputs: &Law10Inputs) -> Result<(), String> {
    let period = fake_selection_period();
    let config = default_dist_config();
    let kind = TaskKind::Task;

    let mut samples_before: Vec<TaskHistorySample<TaskKind>> = Vec::new();
    for i in 0..inputs.valid_count {
        samples_before.push(TaskHistorySample {
            group: kind,
            pricing: TaskPricing::Priced {
                credits: credits(100 + (i as i64) * 10),
                quality: EvidenceQuality::Measured,
            },
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        });
    }

    let reports_before = build_group_reports(samples_before.clone(), period, &config);
    let r_before = reports_before
        .get(&kind)
        .ok_or_else(|| "missing before report".to_string())?;

    let width_before = match &r_before.verdict {
        DistributionVerdict::Distribution { central_range, .. } => central_range.width().micros(),
        other => return Err(format!("expected Distribution before, got {other:?}")),
    };

    let mut samples_after = samples_before;
    for _ in 0..inputs.unknown_token_count {
        samples_after.push(TaskHistorySample {
            group: kind,
            pricing: TaskPricing::UnknownTokenComponents,
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        });
    }

    let reports_after = build_group_reports(samples_after, period, &config);
    let r_after = reports_after
        .get(&kind)
        .ok_or_else(|| "missing after report".to_string())?;

    let width_after = match &r_after.verdict {
        DistributionVerdict::Distribution { central_range, .. } => central_range.width().micros(),
        other => return Err(format!("expected Distribution after, got {other:?}")),
    };

    if width_after < width_before {
        return Err(format!(
            "Adding unknown token kind shrank consumption interval: before={width_before}, after={width_after}"
        ));
    }

    if r_after.exclusions.unknown_token_components != inputs.unknown_token_count {
        return Err(format!(
            "Expected unknown_token_components == {}, got {}",
            inputs.unknown_token_count, r_after.exclusions.unknown_token_components
        ));
    }

    Ok(())
}

pub fn check_law_10(seed: u64) {
    let mut rng = Rng::new(Seed(seed));
    let inputs = generate_law_10_inputs(&mut rng);
    if let Err(err) = verify_law_10(&inputs) {
        panic!("Law 10 violated for seed {seed}!\nReason: {err}\nGenerating inputs:\n{inputs:#?}");
    }
}

// -----------------------------------------------------------------------------------------
// Unit test: Divergence case constructed explicitly
// -----------------------------------------------------------------------------------------

#[test]
fn divergence_case_higher_percentage_window_can_be_limiting() {
    let model = ModelId::new("divergence-model");
    // Window A: 60% remaining (400k used). Small headroom [1,000, 1,200] credits.
    let win_a = make_window("win_a_higher_pct", WindowScope::AccountWide, 400_000);
    // Window B: 30% remaining (700k used). Large headroom [5,000, 6,000] credits.
    let win_b = make_window(
        "win_b_lower_pct",
        WindowScope::ModelSpecific(model),
        700_000,
    );

    let ha = Interval::new(credits(1_000), credits(1_200)).expect("valid");
    let hb = Interval::new(credits(5_000), credits(6_000)).expect("valid");

    let eval = [
        WindowHeadroom::Known {
            window: &win_a,
            headroom: ha,
        },
        WindowHeadroom::Known {
            window: &win_b,
            headroom: hb,
        },
    ];

    let task = TaskReferenceInput {
        verdict: DistributionVerdict::Distribution {
            median: credits(500),
            central_range: Interval::new(credits(400), credits(600)).expect("valid"),
            central_low_percentile: Percentile::new(25).expect("valid"),
            central_high_percentile: Percentile::new(75).expect("valid"),
            upper_reference: credits(900),
            upper_percentile: Percentile::new(90).expect("valid"),
            quantile_method: QuantileMethod::NearestRank,
        },
        sample_count: 20,
    };

    let config = default_verdict_config(true);
    let ready = match assess_can_run(&eval, &task, &config) {
        CanRunAssessment::Ready(ready) => ready,
        other => panic!("expected Ready, got {other:?}"),
    };

    assert_eq!(
        ready.limiting_window.semantic_key().as_str(),
        "win_a_higher_pct",
        "Window A (with higher remaining percentage 60%) must be chosen as limiting"
    );
    assert_eq!(
        ready.lowest_percentage_window.semantic_key().as_str(),
        "win_b_lower_pct",
        "Window B (with lower remaining percentage 30%) must be co-reported as lowest percentage"
    );
    assert!(
        ready.windows_differ,
        "windows_differ must be true when limiting and lowest percentage diverge"
    );
}

// -----------------------------------------------------------------------------------------
// Seed-driven property tests and unit sweeps
// -----------------------------------------------------------------------------------------

#[test]
fn test_law_1_more_current_remaining_quota_cannot_worsen_margin() {
    for seed in 1..=64 {
        check_law_1(seed);
    }
}

#[test]
fn test_law_2_increasing_historical_consumption_cannot_improve_margin() {
    for seed in 1..=64 {
        check_law_2(seed);
    }
}

#[test]
fn test_law_3_widening_calibration_uncertainty_cannot_narrow_advice_interval() {
    for seed in 1..=64 {
        check_law_3(seed);
    }
}

#[test]
fn test_law_4_adding_tighter_applicable_window_cannot_increase_headroom() {
    for seed in 1..=64 {
        check_law_4(seed);
    }
}

#[test]
fn test_law_5_divergence_higher_percentage_window_can_be_limiting() {
    for seed in 1..=64 {
        check_law_5(seed);
    }
}

#[test]
fn test_law_6_removing_window_calibration_moves_verdict_to_unknown() {
    for seed in 1..=64 {
        check_law_6(seed);
    }
}

#[test]
fn test_law_7_removing_fresh_meter_makes_current_advice_unavailable() {
    for seed in 1..=64 {
        check_law_7(seed);
    }
}

#[test]
fn test_law_8_suspect_calibration_health_removes_quantitative_advice() {
    for seed in 1..=64 {
        check_law_8(seed);
    }
}

#[test]
fn test_law_9_adding_estimated_historical_tasks_does_not_improve_exact_evidence() {
    for seed in 1..=64 {
        check_law_9(seed);
    }
}

#[test]
fn test_law_10_adding_unknown_token_kind_cannot_shrink_consumption_interval() {
    for seed in 1..=64 {
        check_law_10(seed);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 40,
        ..ProptestConfig::default()
    })]

    #[test]
    fn prop_law_1_remaining_quota_monotonicity(seed in any::<u64>()) {
        check_law_1(seed);
    }

    #[test]
    fn prop_law_2_historical_consumption_monotonicity(seed in any::<u64>()) {
        check_law_2(seed);
    }

    #[test]
    fn prop_law_3_uncertainty_widening_monotonicity(seed in any::<u64>()) {
        check_law_3(seed);
    }

    #[test]
    fn prop_law_4_window_tightening_monotonicity(seed in any::<u64>()) {
        check_law_4(seed);
    }

    #[test]
    fn prop_law_5_divergence_percentage_vs_headroom(seed in any::<u64>()) {
        check_law_5(seed);
    }

    #[test]
    fn prop_law_6_calibration_completeness(seed in any::<u64>()) {
        check_law_6(seed);
    }

    #[test]
    fn prop_law_7_fresh_meter_requirement(seed in any::<u64>()) {
        check_law_7(seed);
    }

    #[test]
    fn prop_law_8_suspect_health_fail_closed(seed in any::<u64>()) {
        check_law_8(seed);
    }

    #[test]
    fn prop_law_9_estimated_tasks_invariance(seed in any::<u64>()) {
        check_law_9(seed);
    }

    #[test]
    fn prop_law_10_unknown_token_kind_invariance(seed in any::<u64>()) {
        check_law_10(seed);
    }
}
