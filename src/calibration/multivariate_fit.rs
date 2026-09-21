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

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use super::fitter::{
    FitObservation, aggregate_event_tokens, partition_usable_observations, still_running_refusal,
};
use super::multivariate::{
    MultivariateFitConfig, MultivariateFitObservation, MultivariateFitResult, fit_multivariate,
};
use crate::attribution::account_segment::{
    self, AccountMarkerBoundary, AccountSegmentTarget, AccountSegmentationInputs, AccountUsageEvent,
};
use crate::domain::credits::Credits;
use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::provenance::EvidenceId;
use crate::domain::quota::QuotaFractionPpm;
use crate::domain::time::{Clock, UtcTimestamp};
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenKind,
};
use crate::error::Error;
use crate::store::calibration::{
    CandidateId, ConditionNumber, EvidenceDigest, EvidenceFingerprint, ExcludedSample,
    promoted_result_id,
};
use crate::store::calibration::{StoredUsageEvent, load_experiment_usage};
use crate::store::calibration_controlled::{ControlledExperimentRun, load_by_experiment_id};
use crate::store::calibration_multivariate::{
    MultivariateCandidate, MultivariateCandidateId, StoredKindCoefficient,
    insert_multivariate_candidate, load_multivariate_candidate, observations_for_run,
};
use crate::store::calibration_multivariate_result::{
    MultivariateCalibration, insert_multivariate_result, load_multivariate_result_for_candidate,
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

/// The account-marker timeline of every session the usage rows name. A Codex
/// subagent session with no markers of its own resolves through its governing
/// marker timeline (`aub-wvrw`), so the child's tokens count under the
/// parent's account in the block tokens as well as in the meter delta.
pub(super) fn markers_by_session(
    conn: &Connection,
    usage: &[StoredUsageEvent],
) -> Result<BTreeMap<(String, String), Vec<AccountMarkerBoundary>>, Error> {
    let mut markers = BTreeMap::new();
    for (source, native) in usage.iter().filter_map(|row| row.session.as_ref()) {
        if markers.contains_key(&(source.clone(), native.clone())) {
            continue;
        }
        let session_id = SessionId::new(
            SourceNamespace::new(source.clone()),
            NativeSessionId::new(native.clone()),
        );
        let (governing, _) =
            crate::store::session_account_marker::governing_marker_timeline(conn, &session_id)?;
        let boundaries = governing.iter().map(|marker| marker.boundary()).collect();
        markers.insert((source.clone(), native.clone()), boundaries);
    }
    Ok(markers)
}

/// Keeps the usage rows of the events [`account_segment::assign`] places on
/// `account`. `calibrate begin --assert-exclusive` can only promise that
/// nothing else spends the account; the machine driving the burst always runs
/// other sessions on other accounts, and their tokens are not the run's. Rows
/// no marker places on any account are left out too, one excluded sample per
/// session, so a run whose own sessions lost their markers is visible rather
/// than fitted to empty blocks.
///
/// The univariate controlled fit and its promotion scope their credit series
/// through this same rule (`aub-s9qw`), so both paths price one account.
pub(super) fn retain_account_usage(
    usage: Vec<StoredUsageEvent>,
    markers: &BTreeMap<(String, String), Vec<AccountMarkerBoundary>>,
    account: &str,
) -> Result<(Vec<StoredUsageEvent>, Vec<ExcludedSample>), Error> {
    let mut event_times: BTreeMap<Option<(String, String)>, BTreeMap<String, UtcTimestamp>> =
        BTreeMap::new();
    for row in &usage {
        event_times
            .entry(row.session.clone())
            .or_default()
            .insert(row.canonical_event_id.clone(), row.timestamp);
    }

    let mut kept_events: BTreeSet<String> = BTreeSet::new();
    let mut unattributed = Vec::new();
    for (session, times) in &event_times {
        let boundaries = session
            .as_ref()
            .and_then(|key| markers.get(key))
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&String> = times.keys().collect();
        let assigned = account_segment::assign(&AccountSegmentationInputs {
            markers: boundaries,
            usage: times
                .values()
                .map(|at| AccountUsageEvent {
                    occurred_at: *at,
                    usage: KnownTokenVector::new(
                        InputTokens::new(0),
                        OutputTokens::new(0),
                        CacheReadTokens::new(0),
                        CacheWriteTokens::new(0),
                    ),
                })
                .collect(),
        });
        let mut unplaced = 0usize;
        for (id, (target, _)) in ids.into_iter().zip(assigned) {
            match target {
                AccountSegmentTarget::Account(owner) if owner == account => {
                    kept_events.insert(id.clone());
                }
                AccountSegmentTarget::Account(_) => {}
                AccountSegmentTarget::UnknownAccount => unplaced += 1,
            }
        }
        if unplaced > 0 {
            let reference = match session {
                Some((source, native)) => format!("session:{source}/{native}"),
                None => "session:none".to_string(),
            };
            unattributed.push(ExcludedSample::new(
                reference,
                format!(
                    "excluded: {unplaced} usage events unattributed to any account by session markers"
                ),
            )?);
        }
    }

    let own = usage
        .into_iter()
        .filter(|row| kept_events.contains(&row.canonical_event_id))
        .collect();
    Ok((own, unattributed))
}

fn to_micro(value: f64) -> i64 {
    (value * MICRO).round() as i64
}

/// The last instant a reading still measures the run: the end of controlled
/// work plus the settlement grace recorded at `begin`, cut short by the first
/// usage the markers place on the run's own account after the end. The last
/// block has no next spend to close it, so without this bound it closes on
/// whatever the account's window did after the run: on 2026-09-14 two
/// cold-resume probes 28 and 41 minutes after `end` added 130,000 ppm to the
/// input arm's last block. Usage no marker places on any account does not cut
/// the bound, the same rule that keeps it out of the blocks.
fn settlement_bound(
    conn: &Connection,
    run: &ControlledExperimentRun,
    ended_at: UtcTimestamp,
) -> Result<UtcTimestamp, Error> {
    let grace = i64::try_from(
        run.contamination_thresholds
            .post_settlement_grace()
            .as_nanos(),
    )
    .unwrap_or(i64::MAX);
    let grace_end = UtcTimestamp::from_unix_nanos(ended_at.unix_nanos().saturating_add(grace));
    let after_end: Vec<StoredUsageEvent> = load_experiment_usage(conn, ended_at, grace_end)?
        .into_iter()
        .filter(|row| row.timestamp > ended_at)
        .collect();
    let markers = markers_by_session(conn, &after_end)?;
    let (own, _unattributed) = retain_account_usage(after_end, &markers, &run.account)?;
    Ok(own
        .iter()
        .map(|row| row.timestamp)
        .min()
        .map_or(grace_end, |first| {
            UtcTimestamp::from_unix_nanos(first.unix_nanos().saturating_sub(1))
        }))
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
    let ended_at = run
        .ended_at
        .ok_or_else(|| still_running_refusal(run.id.as_str()))?;
    let now = clock.now();
    let until = settlement_bound(conn, run, ended_at)?.min(now);
    let stored = observations_for_run(conn, run, until)?;
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

    let usage = load_experiment_usage(conn, run.started_at, ended_at)?;
    let markers = markers_by_session(conn, &usage)?;
    let (own_usage, unattributed) = retain_account_usage(usage, &markers, &run.account)?;
    let mut events: Vec<(UtcTimestamp, KnownTokenVector)> =
        aggregate_event_tokens(own_usage)?.into_values().collect();
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
        coefficients.push(StoredKindCoefficient {
            kind: coefficient.kind(),
            estimate_micro_ppm_per_token: to_micro(coefficient.estimate_ppm_per_token()),
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
    all_excluded.extend(unattributed);
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

/// The validation procedure a promoted per-kind result records: the mean
/// absolute settled-block residual of [`held_out_block_residual`], over
/// evidence disjoint from the fit.
pub const PER_KIND_VALIDATION_METHOD: &str = "held-out-block-residual";

/// The version of that procedure, recorded on the result.
pub const PER_KIND_VALIDATION_VERSION: &str = "v1";

/// The held-out residual of a joint candidate: the validation readings are
/// folded into settled blocks over the run account's own usage, exactly as
/// the fit folded its readings, each block's movement is predicted from the
/// recorded coefficients, and the mean absolute miss over the blocks that
/// carry a fitted kind is returned in quota parts per million, the unit of the
/// candidate's own fit residual. Returns the residual and the number of
/// validation readings it read.
///
/// Refuses when the readings hold no such block, because a residual over no
/// movement would read as a perfect validation of nothing.
pub fn held_out_block_residual(
    conn: &Connection,
    run: &ControlledExperimentRun,
    coefficients: &[StoredKindCoefficient],
    validation: &[crate::store::calibration::StoredFitObservation],
) -> Result<(QuotaFractionPpm, u32), Error> {
    let mut observations: Vec<FitObservation> = validation
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
    observations
        .sort_by(|a, b| (a.at, a.evidence_id.as_str()).cmp(&(b.at, b.evidence_id.as_str())));
    let (usable, _) = partition_usable_observations(&observations);
    let (Some(first), Some(last)) = (usable.first(), usable.last()) else {
        return Err(Error::InsufficientEvidence(
            "held-out validation holds no usable reading".into(),
        ));
    };
    let usage = load_experiment_usage(conn, first.at, last.at)?;
    let markers = markers_by_session(conn, &usage)?;
    let (own_usage, _) = retain_account_usage(usage, &markers, &run.account)?;
    let mut events: Vec<(UtcTimestamp, KnownTokenVector)> =
        aggregate_event_tokens(own_usage)?.into_values().collect();
    events.sort_by_key(|(at, _)| *at);

    let predicted_ppm = |tokens: KnownTokenVector| -> f64 {
        coefficients
            .iter()
            .map(|c| c.estimate_micro_ppm_per_token as f64 / MICRO * tokens.value(c.kind) as f64)
            .sum()
    };
    let misses: Vec<f64> = settled_blocks(&usable, &events)
        .into_iter()
        .filter(|block| coefficients.iter().any(|c| block.tokens.value(c.kind) > 0))
        .map(|block| (predicted_ppm(block.tokens) - block.delta_ppm).abs())
        .collect();
    if misses.is_empty() {
        return Err(Error::InsufficientEvidence(format!(
            "held-out validation holds no settled block of account '{}' usage in a fitted kind, so there is no movement to predict",
            run.account
        )));
    }
    let mean = misses.iter().sum::<f64>() / misses.len() as f64;
    let residual = i32::try_from(mean.round() as i64)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .ok_or_else(|| {
            Error::InsufficientEvidence(format!(
                "held-out residual {mean:.0} ppm is not a fraction of the quota; the coefficients do not describe this window"
            ))
        })?;
    let readings = u32::try_from(validation.len())
        .map_err(|_| Error::Internal("validation reading count out of u32 range".into()))?;
    Ok((residual, readings))
}

/// Records a per-kind result from a joint candidate: the coefficients, their
/// intervals and the condition number exactly as the candidate recorded them,
/// plus the held-out residual over validation evidence disjoint from the fit.
///
/// Refused rather than approximated whenever the figures would not be the
/// candidate's own: a training set that is not the evidence the candidate was
/// fitted from, a validation set that overlaps it, validation evidence the
/// ledger does not hold, or a candidate already promoted. No coefficient is
/// refitted: the joint candidate records its method, parameters and phase
/// design itself, which is what the scalar path refits to recover.
///
/// Never activates anything.
pub fn promote_multivariate_candidate(
    conn: &mut Connection,
    promotion: &super::fitter::CandidatePromotion<'_>,
    clock: &impl Clock,
) -> Result<MultivariateCalibration, Error> {
    let candidate_id = MultivariateCandidateId::new(promotion.candidate_id.as_str());
    let candidate = load_multivariate_candidate(conn, &candidate_id)?.ok_or_else(|| {
        Error::Usage(format!(
            "no joint calibration candidate '{}'; fit one with `aub calibrate fit`",
            candidate_id.as_str()
        ))
    })?;
    super::fitter::check_promotion_evidence(promotion)?;
    let training_digest = EvidenceDigest::from_inputs(promotion.training);
    if training_digest != candidate.inputs {
        return Err(Error::Usage(format!(
            "promote '{}': --training names {} evidence ids digesting to {:016x}, and the candidate was fitted from {} digesting to {:016x}",
            candidate_id.as_str(),
            training_digest.count(),
            training_digest.digest(),
            candidate.inputs.count(),
            candidate.inputs.digest(),
        )));
    }
    if let Some(existing) = load_multivariate_result_for_candidate(conn, &candidate_id)? {
        return Err(Error::Usage(format!(
            "promote '{}': already promoted as result '{}'; a result is immutable, and a second row would be a second identity for one fit",
            candidate_id.as_str(),
            existing.id.as_str()
        )));
    }
    let run = load_by_experiment_id(conn, &candidate.experiment)?.ok_or_else(|| {
        Error::InsufficientEvidence(format!(
            "no controlled experiment '{}' behind joint candidate '{}'",
            candidate.experiment.as_str(),
            candidate_id.as_str()
        ))
    })?;
    let stored_validation = super::fitter::load_validation_observations(
        conn,
        candidate_id.as_str(),
        &candidate.provider,
        &candidate.window_semantic_key,
        promotion.validation,
    )?;
    let (held_out_residual, validation_observations) =
        held_out_block_residual(conn, &run, &candidate.coefficients, &stored_validation)?;
    let fit_residual = i32::try_from(candidate.fit_residual_ppm)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .ok_or_else(|| {
            Error::Store(format!(
                "joint candidate '{}' records fit residual {} ppm, outside a quota fraction",
                candidate_id.as_str(),
                candidate.fit_residual_ppm
            ))
        })?;

    let result = MultivariateCalibration {
        id: promoted_result_id(&CandidateId::new(candidate_id.as_str())),
        candidate: candidate_id,
        experiment: candidate.experiment.clone(),
        provider: candidate.provider.clone(),
        plan_tier: candidate.plan_tier.clone(),
        window_semantic_key: candidate.window_semantic_key.clone(),
        coefficients: candidate.coefficients.clone(),
        condition_number: candidate.condition_number,
        condition_number_threshold: candidate.condition_number_threshold,
        fit_residual,
        held_out_residual,
        validation_observations,
        sample_count: candidate.sample_count,
        inputs: candidate.inputs,
        fitting_evidence: EvidenceFingerprint::from_inputs(promotion.training),
        validation_evidence: EvidenceFingerprint::from_inputs(promotion.validation),
        validation_method: PER_KIND_VALIDATION_METHOD.to_string(),
        validation_version: PER_KIND_VALIDATION_VERSION.to_string(),
        statistical_method: candidate.statistical_method.clone(),
        statistical_parameters: candidate.statistical_parameters.clone(),
        phase_design: candidate.phase_design.clone(),
        activation_policy_version: promotion.activation_policy_version.to_string(),
        aub_version: crate::build_info::crate_version().to_string(),
        source_revision: crate::build_info::source_revision().to_string(),
        validity: candidate.validity,
        fit_timestamp: candidate.knowledge_time,
        knowledge_time: clock.now(),
    };
    insert_multivariate_result(conn, &result)?;
    Ok(result)
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

    fn usage_row(id: &str, session: &str, at: i64, cache_read: u64) -> StoredUsageEvent {
        StoredUsageEvent {
            canonical_event_id: id.to_string(),
            timestamp: UtcTimestamp::from_unix_nanos(at),
            model_id: None,
            token_class: "cache_read".to_string(),
            count: cache_read,
            session: Some(("claude-code".to_string(), session.to_string())),
        }
    }

    /// A session that switches accounts mid-way contributes only its events
    /// after the switch to the new account; a session on another account
    /// contributes nothing; a session with no marker is reported, not kept.
    #[test]
    fn only_usage_placed_on_the_run_account_is_kept() {
        use crate::attribution::account_segment::AccountEvidenceClass;
        let boundary = |account: &str, at: i64| {
            AccountMarkerBoundary::new(
                account,
                UtcTimestamp::from_unix_nanos(at),
                None,
                AccountEvidenceClass::ExplicitLauncherOrHook,
                true,
            )
        };
        let mut markers = BTreeMap::new();
        markers.insert(
            ("claude-code".to_string(), "switching".to_string()),
            vec![boundary("other", 0), boundary("bianca", 100)],
        );
        markers.insert(
            ("claude-code".to_string(), "foreign".to_string()),
            vec![boundary("other", 0)],
        );
        let usage = vec![
            usage_row("before-switch", "switching", 50, 1),
            usage_row("after-switch", "switching", 150, 2),
            usage_row("foreign-turn", "foreign", 150, 4),
            usage_row("orphan-turn", "orphan", 150, 8),
        ];

        let (kept, excluded) = retain_account_usage(usage, &markers, "bianca").unwrap();

        let kept_ids: Vec<&str> = kept
            .iter()
            .map(|row| row.canonical_event_id.as_str())
            .collect();
        assert_eq!(kept_ids, vec!["after-switch"]);
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].sample_ref(), "session:claude-code/orphan");
    }

    /// Readings with no spend between them open no block at all.
    #[test]
    fn readings_without_spend_yield_no_block() {
        let usable = vec![reading("r0", 0, 100_000), reading("r1", 10, 100_000)];
        assert!(settled_blocks(&usable, &[]).is_empty());
    }

    /// A Codex subagent's usage counts under its parent's account
    /// (`aub-wvrw`): the marker timeline comes from the first marked
    /// ancestor, so the child's block tokens join the run instead of being
    /// left out while the meter delta keeps them.
    #[test]
    fn subagent_usage_counts_under_the_parent_account() {
        use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
        use crate::domain::time::MonotonicDuration;
        use crate::sessions::{ProjectKey, RepositoryKey};
        use crate::store::connection::PragmaPolicy;
        use crate::store::session::{NewSession, insert_session};
        use crate::store::session_account_marker::{
            EvidenceDesignation, MarkerSource, NewSessionAccountMarker,
        };

        let dir = std::env::temp_dir().join(format!(
            "aub-calibration-subagent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock must be after the epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::store::test_schema::open_migrated(
            &dir.join("ledger.db"),
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(100),
            },
        );

        insert_session(
            &conn,
            &NewSession {
                source: SourceNamespace::new("codex"),
                native_session_id: NativeSessionId::new("parent-1"),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: ProjectKey::new("project-a"),
                repository_key: RepositoryKey::new("repository-a"),
                working_directory: None,
                parent_native_session_id: None,
                run_id: None,
            },
        )
        .unwrap();
        insert_session(
            &conn,
            &NewSession {
                source: SourceNamespace::new("codex"),
                native_session_id: NativeSessionId::new("child-1"),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: ProjectKey::new("project-a"),
                repository_key: RepositoryKey::new("repository-a"),
                working_directory: None,
                parent_native_session_id: Some(NativeSessionId::new("parent-1")),
                run_id: None,
            },
        )
        .unwrap();
        crate::store::session_account_marker::insert_marker(
            &conn,
            &NewSessionAccountMarker {
                session_id: SessionId::new(
                    SourceNamespace::new("codex"),
                    NativeSessionId::new("parent-1"),
                ),
                observed_at: UtcTimestamp::from_unix_nanos(1),
                source_ordering_key: None,
                logical_account: "work".to_owned(),
                resolved_account_id: None,
                marker_source: MarkerSource::new("hook"),
                run_id: None,
                evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
            },
        )
        .unwrap();

        let usage = vec![StoredUsageEvent {
            canonical_event_id: "child-turn".to_string(),
            timestamp: UtcTimestamp::from_unix_nanos(20),
            model_id: None,
            token_class: "input".to_string(),
            count: 5,
            session: Some(("codex".to_string(), "child-1".to_string())),
        }];

        let markers = markers_by_session(&conn, &usage).unwrap();
        assert_eq!(markers.len(), 1);
        assert!(
            !markers[&("codex".to_string(), "child-1".to_string())].is_empty(),
            "the child resolves through its parent marker timeline"
        );

        let (kept, _) = retain_account_usage(usage, &markers, "work").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].canonical_event_id, "child-turn");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
