//! The multivariate path of `calibrate fit`: from a controlled experiment's
//! recorded observations to a joint per-kind candidate, or to a refusal that
//! names what could not be separated (`aub-73xa`).
//!
//! The dispatch rule is the experiment's own premise. A controlled run whose
//! `--expect-kinds` names two or more kinds is fitted jointly through
//! [`super::multivariate::fit_multivariate`]; a premise naming one kind, or
//! an experiment with no controlled premise at all, keeps today's univariate
//! path untouched. The fitter is never told which kinds to fit by a source
//! constant: the premise recorded at `begin` is the only source.
//!
//! The observation frame is the settled block. Each interval between two
//! usable meter readings that carries usage opens a block; the quiet readings
//! that follow it, while the meter catches up, extend the block rather than
//! becoming rows of their own; the block closes at the last reading before
//! the next usage, so its quota delta is the settled movement of exactly its
//! own tokens. A quiet reading treated as a row would carry zero tokens and
//! a nonzero delta, and the fitter would drop it as signal-free, losing the
//! lagged movement it holds. Every null reading on record before this bead
//! was taken shortly after the spend; this frame is where that lesson lives.
//!
//! Nothing here activates anything: the candidate is recorded once,
//! immutably, and `calibrate activate` with its thresholds stays the only
//! path to an active calibration (`docs/INVARIANTS.md`, row 29).
//!
//! May not depend on:
//! - transcripts (the calibration layer never parses transcripts)
//! - presentation

use std::collections::BTreeSet;

use rusqlite::Connection;

use super::fitter::{FitObservation, aggregate_event_tokens, partition_usable_observations};
use super::multivariate::{
    MultivariateFitConfig, MultivariateFitObservation, MultivariateFitResult, fit_multivariate,
};
use crate::domain::credits::Credits;
use crate::domain::provenance::EvidenceId;
use crate::domain::time::{Clock, UtcTimestamp};
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenKind,
};
use crate::error::Error;
use crate::store::calibration::load_experiment_usage;
use crate::store::calibration::{ConditionNumber, EvidenceDigest, ExcludedSample};
use crate::store::calibration_controlled::ControlledExperimentRun;
use crate::store::calibration_multivariate::{
    MultivariateCandidate, MultivariateCandidateId, StoredKindCoefficient,
    insert_multivariate_candidate, load_multivariate_candidate, observations_for_run,
};
use crate::store::cost_model::ValidityInterval;

/// Which fitter `calibrate fit` runs for an experiment, decided by its premise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitPath {
    Univariate,
    Multivariate,
}

/// The dispatch rule: two or more distinct kinds in the premise is a joint
/// fit; one kind, or no controlled premise at all, is the univariate fit.
pub fn fit_path_for_premise(expected_kinds: Option<&[TokenKind]>) -> FitPath {
    let distinct: BTreeSet<&str> = expected_kinds
        .unwrap_or(&[])
        .iter()
        .map(|kind| kind.label())
        .collect();
    if distinct.len() >= 2 {
        FitPath::Multivariate
    } else {
        FitPath::Univariate
    }
}

/// Everything `calibrate fit` reports about an accepted multivariate fit: the
/// recorded candidate and the fit figures that are not stored on it.
#[derive(Debug, Clone, PartialEq)]
pub struct MultivariateFitOutcome {
    pub candidate: MultivariateCandidate,
    pub result: MultivariateFitResult,
    pub usable_observations: u32,
    pub excluded_samples: Vec<ExcludedSample>,
    pub residual_ppm: f64,
}

/// One settled block: the tokens spent in it and the meter movement measured
/// from the reading before its usage to the last reading before the next.
#[derive(Debug, Clone, PartialEq)]
struct SettledBlock {
    evidence_id: EvidenceId,
    tokens: KnownTokenVector,
    delta_ppm: f64,
}

const MICRO: f64 = 1_000_000.0;

fn midpoint_ppm(observation: &FitObservation) -> f64 {
    let interval = observation.interval();
    (interval.lower_ppm() + interval.upper_ppm()) as f64 / 2.0
}

fn tokens_between(
    events: &[(UtcTimestamp, KnownTokenVector)],
    after: UtcTimestamp,
    until: UtcTimestamp,
) -> KnownTokenVector {
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    for (at, tokens) in events {
        if *at > after && *at <= until {
            input += tokens.input().value();
            output += tokens.output().value();
            cache_read += tokens.cache_read().value();
            cache_write += tokens.cache_write().value();
        }
    }
    KnownTokenVector::new(
        InputTokens::new(input),
        OutputTokens::new(output),
        CacheReadTokens::new(cache_read),
        CacheWriteTokens::new(cache_write),
    )
}

fn carries_usage(tokens: KnownTokenVector) -> bool {
    TokenKind::ALL.iter().any(|kind| tokens.value(*kind) > 0)
}

/// Folds usable readings and dated usage into settled blocks. The reading
/// before the first usage is the frame's origin; a run whose readings hold
/// no usage at all yields no blocks, and the fitter refuses that by count.
fn settled_blocks(
    usable: &[FitObservation],
    events: &[(UtcTimestamp, KnownTokenVector)],
) -> Vec<SettledBlock> {
    let mut blocks = Vec::new();
    let mut open: Option<(KnownTokenVector, f64)> = None;
    for pair in usable.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        let tokens = tokens_between(events, previous.at, current.at);
        if carries_usage(tokens) {
            if let Some((block_tokens, origin_ppm)) = open.take() {
                blocks.push(SettledBlock {
                    evidence_id: previous.evidence_id.clone(),
                    tokens: block_tokens,
                    delta_ppm: midpoint_ppm(previous) - origin_ppm,
                });
            }
            open = Some((tokens, midpoint_ppm(previous)));
        }
    }
    if let (Some((block_tokens, origin_ppm)), Some(last)) = (open, usable.last()) {
        blocks.push(SettledBlock {
            evidence_id: last.evidence_id.clone(),
            tokens: block_tokens,
            delta_ppm: midpoint_ppm(last) - origin_ppm,
        });
    }
    blocks
}

fn to_micro(value: f64) -> i64 {
    (value * MICRO).round() as i64
}

/// Fits the run's observations jointly over the kinds its premise names and
/// records the candidate immutably. A refusal from the identifiability gate
/// comes back as the fitter's own message, which names the collinear pair
/// and the condition number against its threshold, and records nothing.
///
/// Never activates the candidate.
pub fn fit_controlled_run_and_record(
    conn: &mut Connection,
    run: &ControlledExperimentRun,
    clock: &impl Clock,
) -> Result<MultivariateFitOutcome, Error> {
    let ended_at = run.ended_at.ok_or_else(|| {
        Error::InsufficientEvidence(format!(
            "controlled experiment '{}' is still running; record `aub calibrate end` before fitting",
            run.id.as_str()
        ))
    })?;
    let now = clock.now();
    let stored = observations_for_run(conn, run, now)?;
    if stored.is_empty() {
        return Err(Error::InsufficientEvidence(format!(
            "no meter observations found for controlled experiment '{}'",
            run.id.as_str()
        )));
    }
    let observations: Vec<FitObservation> = stored
        .iter()
        .map(|obs| {
            FitObservation::new(
                obs.evidence_id.clone(),
                obs.at,
                obs.quota_used_ppm,
                obs.reported_resolution_ppm,
                obs.quantization,
                Credits::from_micros(0),
            )
        })
        .collect();
    let (usable, excluded_samples) = partition_usable_observations(&observations);

    let mut events: Vec<(UtcTimestamp, KnownTokenVector)> =
        aggregate_event_tokens(load_experiment_usage(conn, run.started_at, ended_at)?)?
            .into_values()
            .collect();
    events.sort_by_key(|(at, _)| *at);

    let blocks = settled_blocks(&usable, &events);
    let mut fit_observations = Vec::with_capacity(blocks.len());
    for block in &blocks {
        fit_observations.push(
            MultivariateFitObservation::new(
                block.evidence_id.clone(),
                block.tokens,
                block.delta_ppm,
            )
            .map_err(|e| Error::InsufficientEvidence(format!("unusable block: {e}")))?,
        );
    }

    let kinds = &run.expected_token_kinds;
    let config = MultivariateFitConfig::new(
        MultivariateFitConfig::DEFAULT_CONDITION_NUMBER_THRESHOLD,
        2 * kinds.len(),
        MultivariateFitConfig::DEFAULT_RIDGE_PENALTY,
        true,
    )
    .map_err(|e| Error::Internal(format!("multivariate fit configuration: {e}")))?;
    let kind_labels = kinds
        .iter()
        .map(|kind| kind.label())
        .collect::<Vec<_>>()
        .join(",");
    let phase_design = format!(
        "controlled-run={};kinds={kind_labels};frame=settled-blocks",
        run.id.as_str()
    );
    let result = fit_multivariate(&fit_observations, kinds, &config, &phase_design)
        .map_err(|rejection| rejection.into_error())?;

    let residual_ppm = mean_absolute_residual_ppm(&result, &blocks);
    let mut coefficients = Vec::with_capacity(result.coefficients().len());
    for coefficient in result.coefficients() {
        let estimate = to_micro(coefficient.estimate_ppm_per_token());
        if estimate <= 0 {
            return Err(Error::InsufficientEvidence(format!(
                "fit rejected: the {} coefficient rounds to zero micro-ppm per token and cannot be recorded",
                coefficient.kind().label()
            )));
        }
        coefficients.push(StoredKindCoefficient {
            kind: coefficient.kind(),
            estimate_micro_ppm_per_token: estimate,
            std_error_micro_ppm_per_token: to_micro(coefficient.std_error_ppm_per_token()).max(0),
            interval_low_micro_ppm_per_token: to_micro(coefficient.interval_low_ppm_per_token()),
            interval_high_micro_ppm_per_token: to_micro(coefficient.interval_high_ppm_per_token()),
        });
    }

    let inputs_set: BTreeSet<EvidenceId> =
        stored.iter().map(|obs| obs.evidence_id.clone()).collect();
    let inputs = EvidenceDigest::from_inputs(&inputs_set);
    let candidate = MultivariateCandidate {
        id: MultivariateCandidateId::new(format!(
            "mvcand-{}-{:016x}",
            run.id.as_str(),
            inputs.digest()
        )),
        experiment: run.id.clone(),
        provider: run.provider.clone(),
        plan_tier: run.plan_tier.clone(),
        window_semantic_key: run.window_semantic_key.clone(),
        coefficients,
        condition_number: ConditionNumber::from_micros(to_micro(result.condition_number())),
        condition_number_threshold: ConditionNumber::from_micros(to_micro(
            result.condition_number_threshold(),
        )),
        fit_residual_ppm: residual_ppm.round().clamp(0.0, MICRO) as i64,
        sample_count: result.usable_observations(),
        inputs,
        statistical_method: result.statistical_method().to_string(),
        statistical_parameters: result.statistical_parameters().to_string(),
        phase_design,
        validity: ValidityInterval::new(run.started_at, ended_at)?,
        knowledge_time: now,
    };

    if load_multivariate_candidate(conn, &candidate.id)?.is_none() {
        insert_multivariate_candidate(conn, &candidate)?;
    }

    let mut all_excluded = excluded_samples;
    all_excluded.extend(result.excluded_samples().iter().cloned());
    Ok(MultivariateFitOutcome {
        usable_observations: result.usable_observations(),
        candidate,
        result,
        excluded_samples: all_excluded,
        residual_ppm,
    })
}

/// The mean absolute distance between each block's predicted and measured
/// movement, over the blocks the fitter used, in quota parts per million.
fn mean_absolute_residual_ppm(result: &MultivariateFitResult, blocks: &[SettledBlock]) -> f64 {
    let used: Vec<&SettledBlock> = blocks
        .iter()
        .filter(|block| {
            result
                .coefficients()
                .iter()
                .any(|c| block.tokens.value(c.kind()) > 0)
        })
        .collect();
    if used.is_empty() {
        return 0.0;
    }
    let total: f64 = used
        .iter()
        .map(|block| {
            let predicted: f64 = result
                .coefficients()
                .iter()
                .map(|c| c.estimate_ppm_per_token() * block.tokens.value(c.kind()) as f64)
                .sum();
            (predicted - block.delta_ppm).abs()
        })
        .sum();
    total / used.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::window::QuantizationSemantics;

    fn kinds(labels: &[TokenKind]) -> Vec<TokenKind> {
        labels.to_vec()
    }

    /// The dispatch follows the premise and nothing else: zero or one kind
    /// keeps the univariate path, two or four go joint, and a premise that
    /// repeats one kind counts it once.
    #[test]
    fn fit_path_follows_the_premise_over_zero_one_two_and_four_kinds() {
        assert_eq!(fit_path_for_premise(None), FitPath::Univariate);
        assert_eq!(fit_path_for_premise(Some(&kinds(&[]))), FitPath::Univariate);
        assert_eq!(
            fit_path_for_premise(Some(&kinds(&[TokenKind::Output]))),
            FitPath::Univariate
        );
        assert_eq!(
            fit_path_for_premise(Some(&kinds(&[TokenKind::Output, TokenKind::Output]))),
            FitPath::Univariate
        );
        assert_eq!(
            fit_path_for_premise(Some(&kinds(&[TokenKind::Input, TokenKind::CacheRead]))),
            FitPath::Multivariate
        );
        assert_eq!(
            fit_path_for_premise(Some(&TokenKind::ALL)),
            FitPath::Multivariate
        );
    }

    fn reading(id: &str, at: i64, ppm: i64) -> FitObservation {
        FitObservation::new(
            EvidenceId::new(id),
            UtcTimestamp::from_unix_nanos(at),
            ppm,
            10_000,
            QuantizationSemantics::Exact,
            Credits::from_micros(0),
        )
    }

    fn spend(at: i64, output: u64) -> (UtcTimestamp, KnownTokenVector) {
        (
            UtcTimestamp::from_unix_nanos(at),
            KnownTokenVector::new(
                InputTokens::new(0),
                OutputTokens::new(output),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
        )
    }

    /// Two blocks of spend, each followed by a lagging reading and then two
    /// settled ones. The block's delta is the settled movement, measured at
    /// the last reading before the next spend, never at the lagging one.
    #[test]
    fn a_block_closes_at_the_last_reading_before_the_next_spend() {
        let usable = vec![
            reading("r0", 0, 100_000),
            reading("r1", 20, 100_000),
            reading("r2", 30, 120_000),
            reading("r3", 40, 120_000),
            reading("r4", 60, 125_000),
            reading("r5", 70, 130_000),
            reading("r6", 80, 130_000),
        ];
        let events = vec![spend(10, 2_000), spend(50, 1_000)];
        let blocks = settled_blocks(&usable, &events);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].tokens.output().value(), 2_000);
        assert_eq!(blocks[0].delta_ppm, 20_000.0);
        assert_eq!(blocks[0].evidence_id.as_str(), "r3");
        assert_eq!(blocks[1].tokens.output().value(), 1_000);
        assert_eq!(blocks[1].delta_ppm, 10_000.0);
        assert_eq!(blocks[1].evidence_id.as_str(), "r6");
    }

    /// Readings with no spend between them open no block at all.
    #[test]
    fn readings_without_spend_yield_no_block() {
        let usable = vec![reading("r0", 0, 100_000), reading("r1", 10, 100_000)];
        assert!(settled_blocks(&usable, &[]).is_empty());
    }
}
