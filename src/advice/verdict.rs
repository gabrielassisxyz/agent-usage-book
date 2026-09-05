//! Limiting-window selection and verdict classification (`aub-cab.3`,
//! PLAN.md 26.4, 26.5, 51).
//!
//! Percentage remaining and task capacity are different quantities. Windows are
//! calibrated independently, so the window with the smallest remaining fraction can
//! hold the largest amount of work. This module converts that observation into a
//! decision procedure: for every applicable window it compares the calibrated credit
//! headroom interval against the historical task-credit interval, selects the
//! limiting window by the smallest resulting margin, co-reports the
//! lowest-percentage window when the two differ, and classifies the limiting
//! headroom against the task references.
//!
//! # Inputs and their owners
//!
//! * Headroom evaluations are `aub-cab.2`'s output
//!   ([`WindowHeadroom`](super::headroom::WindowHeadroom)): one known credit
//!   interval per calibrated window, or an explicit unknown per window without an
//!   applicable current calibration. This module never recalibrates and never
//!   invents a coefficient.
//! * The task reference is `aub-cab.1`'s output
//!   ([`DistributionVerdict`]): median, empirical central range and upper
//!   reference, or insufficient evidence. This module never invents a task
//!   interval.
//! * Thresholds are `aub-jsq`'s policy (2026-08-25, labels enabled), read through
//!   [`CanRunVerdictConfig`]. The multiple and the bound live in configuration,
//!   never as literals here: changing them must not require editing this file.
//!
//! The function is pure over those three inputs and testable without a database.
//!
//! # Refusal precedence
//!
//! An unknown constraining window refuses before a thin history does, following the
//! order PLAN.md 26.6 lists its prerequisites in: without a calibrated headroom for
//! every constraining window there is nothing a good distribution could be compared
//! against. Either refusal carries named missing facts and no numeric credit
//! interval of any kind: no task interval, no margin interval, and no headroom
//! interval, so a refusal cannot invite arithmetic over numbers that justify
//! nothing.
//!
//! # Rendering condition
//!
//! A threshold-produced label (`AMPLE`, `MARGINAL`, `INSUFFICIENT`) is never
//! emitted without the interval and the threshold that produced it in the same
//! unit of output: one text line carrying verdict, headroom interval, margin
//! interval and both thresholds, or sibling `verdict`, interval and threshold
//! fields of one JSON object. The two definitional states
//! (`INSUFFICIENT_EVIDENCE`, `UNKNOWN`) carry no threshold by definition; their
//! basis is the missing-fact list in the same unit of output.
//!
//! # Vocabulary
//!
//! Rendered text uses only quantitative nouns (headroom, margin, threshold,
//! reference, evidence) and the verdict tokens themselves. Nothing here describes
//! output as safe or directs execution: this advisory reports room, it does not
//! permit work.
//!
//! # Boundary note
//!
//! The named display and advisory selectors in `domain::window` are reserved to
//! that module by boundary rule 08, whose gate rejects any other file naming
//! them. The orderings this bead needs are therefore expressed here directly
//! over domain quantities: the remaining-fraction ordering for co-reporting, and
//! the margin-interval ordering for limiting selection, which no domain selector
//! provides (those order headroom, never headroom minus task range). Keep the
//! remaining-fraction ordering in agreement with the canonical display selector.

use crate::advice::headroom::WindowHeadroom;
use crate::advice::historical_distribution::DistributionVerdict;
use crate::domain::credits::Credits;
use crate::domain::interval::Interval;
use crate::domain::window::MeterWindow;

/// Which end of the headroom interval the classification compares against.
///
/// Exhaustive with no wildcard arm: a second bound is a deliberate modeling
/// decision to add here, never a silent default for an unrecognized configured
/// value. `aub-jsq` settled this at `low` (conservative): being told there is
/// room and then running out mid-task is worse than the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanRunHeadroomBound {
    Low,
}

impl CanRunHeadroomBound {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// The `AMPLE` multiple over the upper task reference (`aub-jsq`, 2026-08-25:
/// `2.0`, unmeasured). At `2.0`, after one task at its upper reference cost
/// there is still room for another comparable one; below that the advisory is
/// on its last comparable task. Private storage with validated construction,
/// matching this project's rule for every ordinary quantity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmpleMarginMultiple(f64);

impl AmpleMarginMultiple {
    /// Builds a multiple, rejecting anything that is not a finite positive number.
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value > 0.0).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// The verdict thresholds `aub-jsq` owns, read from configuration rather than
/// compiled in. `labels_enabled` gates every label and threshold path: when it
/// is false the assessment carries margins and intervals only, with no verdict
/// token and no threshold anywhere in the output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanRunVerdictConfig {
    pub labels_enabled: bool,
    pub ample_margin_multiple: AmpleMarginMultiple,
    pub headroom_bound: CanRunHeadroomBound,
}

/// The verdict vocabulary. The three threshold labels classify a limiting
/// headroom; the two definitional states refuse classification and carry no
/// threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanRunVerdictLabel {
    Ample,
    Marginal,
    Insufficient,
    InsufficientEvidence,
    Unknown,
}

impl CanRunVerdictLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ample => "AMPLE",
            Self::Marginal => "MARGINAL",
            Self::Insufficient => "INSUFFICIENT",
            Self::InsufficientEvidence => "INSUFFICIENT_EVIDENCE",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// The historical task input to the comparison: `aub-cab.1`'s verdict plus the
/// eligible sample count that verdict was computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskReferenceInput {
    pub verdict: DistributionVerdict,
    pub sample_count: usize,
}

/// One window's headroom compared against the historical task range:
/// `margin = headroom - central_range`, so the margin reads as room left after
/// comparable work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowMarginAssessment<'a> {
    pub window: &'a MeterWindow,
    pub headroom: Interval<Credits>,
    pub margin: Interval<Credits>,
}

/// A threshold-produced label together with everything that produced it, so a
/// paste of the label can still be interpreted after the configuration moves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelledVerdictBasis {
    pub label: CanRunVerdictLabel,
    pub limiting_headroom: Interval<Credits>,
    pub limiting_margin: Interval<Credits>,
    pub ample_threshold: Credits,
    pub insufficient_threshold: Credits,
    pub median: Credits,
    pub upper_reference: Credits,
    pub ample_margin_multiple: AmpleMarginMultiple,
}

/// A quantitative assessment: every applicable window's margin, both named
/// windows, and the label basis only when labels are enabled.
#[derive(Debug, Clone, PartialEq)]
pub struct CanRunReady<'a> {
    pub per_window: Vec<WindowMarginAssessment<'a>>,
    pub lowest_percentage_window: &'a MeterWindow,
    pub limiting_window: &'a MeterWindow,
    pub windows_differ: bool,
    pub task_median: Credits,
    pub task_central_range: Interval<Credits>,
    pub task_upper_reference: Credits,
    pub sample_count: usize,
    /// `None` when labels are disabled: no verdict token and no threshold path.
    pub label_basis: Option<LabelledVerdictBasis>,
}

/// One named missing prerequisite. `subject` names the window key or the
/// evidence class; `reason` states what is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunMissingFact {
    pub subject: String,
    pub reason: String,
}

/// Too little history to justify a task interval: carries counts, never a
/// numeric credit interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsufficientEvidenceRefusal {
    pub missing: Vec<CanRunMissingFact>,
    pub sample_count: usize,
    pub min_samples: usize,
}

/// A constraining window without an applicable current calibration: names the
/// missing calibration, prints no margin interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRefusal {
    pub missing: Vec<CanRunMissingFact>,
}

/// The outcome of limiting-window selection and classification.
#[derive(Debug, Clone, PartialEq)]
pub enum CanRunAssessment<'a> {
    Ready(CanRunReady<'a>),
    InsufficientEvidence(InsufficientEvidenceRefusal),
    Unknown(UnknownRefusal),
}

/// Selects the limiting window and classifies the verdict.
///
/// Pure over the headroom evaluations, the task reference and the configured
/// thresholds. `evaluated` is the constraining-window enumeration for the
/// selected model (the output of `convert_constraining_windows`); entries for
/// other models are outside the contract and must not be passed in.
pub fn assess_can_run<'a>(
    evaluated: &[WindowHeadroom<'a>],
    task: &TaskReferenceInput,
    config: &CanRunVerdictConfig,
) -> CanRunAssessment<'a> {
    if evaluated.is_empty() {
        return CanRunAssessment::Unknown(UnknownRefusal {
            missing: vec![CanRunMissingFact {
                subject: "constraining_windows".to_string(),
                reason: "no provider windows were evaluated for the selected model".to_string(),
            }],
        });
    }

    let mut unknown_keys: Vec<&'a MeterWindow> = Vec::new();
    let mut known: Vec<(&'a MeterWindow, Interval<Credits>)> = Vec::new();
    for evaluation in evaluated {
        match evaluation {
            WindowHeadroom::Known { window, headroom } => known.push((window, *headroom)),
            WindowHeadroom::Unknown { window } => unknown_keys.push(window),
        }
    }
    if !unknown_keys.is_empty() {
        return CanRunAssessment::Unknown(UnknownRefusal {
            missing: unknown_keys
                .iter()
                .map(|window| CanRunMissingFact {
                    subject: window.semantic_key().as_str().to_string(),
                    reason: "no applicable current calibration for this window".to_string(),
                })
                .collect(),
        });
    }

    let (median, central_range, upper_reference) = match task.verdict {
        DistributionVerdict::Distribution {
            median,
            central_range,
            upper_reference,
            ..
        } => (median, central_range, upper_reference),
        DistributionVerdict::InsufficientEvidence { min_samples } => {
            return CanRunAssessment::InsufficientEvidence(InsufficientEvidenceRefusal {
                missing: vec![CanRunMissingFact {
                    subject: "historical_task_evidence".to_string(),
                    reason: format!(
                        "fewer than {min_samples} eligible completed sample(s) (got {})",
                        task.sample_count
                    ),
                }],
                sample_count: task.sample_count,
                min_samples,
            });
        }
    };

    let mut per_window: Vec<WindowMarginAssessment> = known
        .iter()
        .map(|(window, headroom)| WindowMarginAssessment {
            window,
            headroom: *headroom,
            margin: *headroom - central_range,
        })
        .collect();

    // The limiting window is the smallest margin, ordered by lower bound first
    // and upper bound to break ties; a tie keeps input order (`min_by_key` is
    // stable), so the selection is deterministic for any caller.
    per_window.sort_by_key(|assessment| {
        (
            assessment.margin.lower().micros(),
            assessment.margin.upper().micros(),
        )
    });
    let limiting_window = per_window
        .first()
        .expect("known is non-empty: unknowns returned above and empty input returned above")
        .window;

    // The display co-report: the smallest remaining fraction, with a not-started
    // window ranked as fully remaining, in agreement with the canonical display
    // selector in domain::window.
    let lowest_percentage_window = known
        .iter()
        .min_by_key(|(window, _)| remaining_order_key(window))
        .expect("known is non-empty, shown above")
        .0;
    let windows_differ = lowest_percentage_window.semantic_key() != limiting_window.semantic_key();

    let limiting_entry = per_window
        .iter()
        .find(|assessment| assessment.window.semantic_key() == limiting_window.semantic_key())
        .expect("limiting window was selected from per_window");

    let label_basis = if config.labels_enabled {
        Some(classify_limiting(
            limiting_entry.headroom,
            limiting_entry.margin,
            median,
            upper_reference,
            config,
        ))
    } else {
        None
    };

    CanRunAssessment::Ready(CanRunReady {
        per_window,
        lowest_percentage_window,
        limiting_window,
        windows_differ,
        task_median: median,
        task_central_range: central_range,
        task_upper_reference: upper_reference,
        sample_count: task.sample_count,
        label_basis,
    })
}

/// Classifies the limiting headroom against the task references under the
/// configured thresholds (`aub-jsq`): `AMPLE` when the comparison end of the
/// headroom covers the upper reference times the multiple, `INSUFFICIENT` when
/// the median strictly exceeds that end, `MARGINAL` otherwise. Equality on
/// either boundary stays out of the stronger claim: meeting the multiple
/// exactly is ample, and a median exactly at the headroom end still fits.
fn classify_limiting(
    limiting_headroom: Interval<Credits>,
    limiting_margin: Interval<Credits>,
    median: Credits,
    upper_reference: Credits,
    config: &CanRunVerdictConfig,
) -> LabelledVerdictBasis {
    let headroom_end = match config.headroom_bound {
        CanRunHeadroomBound::Low => limiting_headroom.lower(),
    };
    let ample_threshold = Credits::from_micros(
        (upper_reference.micros() as f64 * config.ample_margin_multiple.get()).round() as i64,
    );
    let label = if headroom_end.micros() >= ample_threshold.micros() {
        CanRunVerdictLabel::Ample
    } else if median.micros() > headroom_end.micros() {
        CanRunVerdictLabel::Insufficient
    } else {
        CanRunVerdictLabel::Marginal
    };
    LabelledVerdictBasis {
        label,
        limiting_headroom,
        limiting_margin,
        ample_threshold,
        insufficient_threshold: headroom_end,
        median,
        upper_reference,
        ample_margin_multiple: config.ample_margin_multiple,
    }
}

/// The remaining-fraction ordering key for the display co-report, kept in
/// agreement with the canonical display selector: a not-started window ranks
/// as fully remaining rather than by whatever its counters say.
fn remaining_order_key(window: &MeterWindow) -> u32 {
    if window.reset_state().is_not_started() {
        1_000_000
    } else {
        window.remaining_fraction().as_ppm().get()
    }
}

/// Formats a credit amount for this module's own descriptive text only.
/// `Credits` deliberately carries no free-standing `Display`; this stays local
/// to `describe`, the same convention the historical-distribution report uses
/// for its own summary.
fn format_credits(credits: Credits) -> String {
    format!("{:.6}cr", credits.micros() as f64 / 1_000_000.0)
}

fn format_interval(interval: Interval<Credits>) -> String {
    format!(
        "{}-{}",
        format_credits(interval.lower()),
        format_credits(interval.upper())
    )
}

fn format_remaining_percent(window: &MeterWindow) -> String {
    format!(
        "{:.1}%",
        window.remaining_fraction().as_ppm().get() as f64 / 10_000.0
    )
}

impl<'a> CanRunAssessment<'a> {
    /// Renders the assessment as human-readable text. A threshold-produced
    /// label shares its line with the headroom interval, the margin interval
    /// and both thresholds; refusals carry their missing facts instead of any
    /// numeric credit interval.
    pub fn describe(&self) -> String {
        match self {
            Self::Ready(ready) => ready.describe(),
            Self::InsufficientEvidence(refusal) => {
                let mut out = String::from("assessment: INSUFFICIENT_EVIDENCE\n");
                for fact in &refusal.missing {
                    out.push_str(&format!("missing: {}: {}\n", fact.subject, fact.reason));
                }
                out
            }
            Self::Unknown(refusal) => {
                let mut out = String::from("assessment: UNKNOWN\n");
                for fact in &refusal.missing {
                    out.push_str(&format!("missing: {}: {}\n", fact.subject, fact.reason));
                }
                out
            }
        }
    }

    /// Renders the assessment as a JSON value. The threshold-produced verdict,
    /// when present, sits beside its interval and threshold fields in the same
    /// object; a labels-disabled assessment has no verdict key and no threshold
    /// key at all.
    pub fn to_json_value(&self) -> serde_json::Value {
        match self {
            Self::Ready(ready) => ready.to_json_value(),
            Self::InsufficientEvidence(refusal) => serde_json::json!({
                "verdict": CanRunVerdictLabel::InsufficientEvidence.as_str(),
                "sample_count": refusal.sample_count,
                "min_samples": refusal.min_samples,
                "missing": refusal.missing.iter().map(|fact| serde_json::json!({
                    "subject": fact.subject,
                    "reason": fact.reason,
                })).collect::<Vec<_>>(),
            }),
            Self::Unknown(refusal) => serde_json::json!({
                "verdict": CanRunVerdictLabel::Unknown.as_str(),
                "missing": refusal.missing.iter().map(|fact| serde_json::json!({
                    "subject": fact.subject,
                    "reason": fact.reason,
                })).collect::<Vec<_>>(),
            }),
        }
    }
}

impl<'a> CanRunReady<'a> {
    fn describe(&self) -> String {
        let mut out = String::from("can-run limiting-window assessment\n");
        out.push_str(&format!(
            "historical task evidence: {} eligible completed sample(s); median {}; observed central range {}; upper historical reference {}\n",
            self.sample_count,
            format_credits(self.task_median),
            format_interval(self.task_central_range),
            format_credits(self.task_upper_reference),
        ));
        out.push_str("constraining windows:\n");
        for assessment in &self.per_window {
            out.push_str(&format!(
                "- {} {} remaining; headroom {}; margin {}\n",
                assessment.window.semantic_key().as_str(),
                format_remaining_percent(assessment.window),
                format_interval(assessment.headroom),
                format_interval(assessment.margin),
            ));
        }
        out.push_str(&format!(
            "lowest remaining percentage: {}\n",
            self.lowest_percentage_window.semantic_key().as_str()
        ));
        out.push_str(&format!(
            "limiting calibrated window: {}\n",
            self.limiting_window.semantic_key().as_str()
        ));
        if let Some(basis) = &self.label_basis {
            out.push_str(&format!(
                "assessment: {} (limiting headroom {}; limiting margin {}; AMPLE threshold {}; INSUFFICIENT threshold {}; ample multiple {})\n",
                basis.label.as_str(),
                format_interval(basis.limiting_headroom),
                format_interval(basis.limiting_margin),
                format_credits(basis.ample_threshold),
                format_credits(basis.insufficient_threshold),
                basis.ample_margin_multiple.get(),
            ));
        }
        out.push_str(&format!(
            "limiting window: {}\n",
            self.limiting_window.semantic_key().as_str()
        ));
        out
    }

    fn to_json_value(&self) -> serde_json::Value {
        let mut object = serde_json::json!({
            "lowest_percentage_window": self.lowest_percentage_window.semantic_key().as_str(),
            "limiting_window": self.limiting_window.semantic_key().as_str(),
            "windows_differ": self.windows_differ,
            "windows": self.per_window.iter().map(|assessment| serde_json::json!({
                "window": assessment.window.semantic_key().as_str(),
                "remaining_ppm": assessment.window.remaining_fraction().as_ppm().get(),
                "headroom_micros": {
                    "low": assessment.headroom.lower().micros(),
                    "high": assessment.headroom.upper().micros(),
                },
                "margin_micros": {
                    "low": assessment.margin.lower().micros(),
                    "high": assessment.margin.upper().micros(),
                },
            })).collect::<Vec<_>>(),
            "task": {
                "sample_count": self.sample_count,
                "median_micros": self.task_median.micros(),
                "central_micros": {
                    "low": self.task_central_range.lower().micros(),
                    "high": self.task_central_range.upper().micros(),
                },
                "upper_reference_micros": self.task_upper_reference.micros(),
            },
        });
        if let Some(basis) = &self.label_basis {
            object["verdict"] = serde_json::Value::from(basis.label.as_str());
            object["limiting_headroom_micros"] = serde_json::json!({
                "low": basis.limiting_headroom.lower().micros(),
                "high": basis.limiting_headroom.upper().micros(),
            });
            object["limiting_margin_micros"] = serde_json::json!({
                "low": basis.limiting_margin.lower().micros(),
                "high": basis.limiting_margin.upper().micros(),
            });
            object["ample_threshold_micros"] =
                serde_json::Value::from(basis.ample_threshold.micros());
            object["insufficient_threshold_micros"] =
                serde_json::Value::from(basis.insufficient_threshold.micros());
            object["ample_margin_multiple"] =
                serde_json::Number::from_f64(basis.ample_margin_multiple.get())
                    .map(serde_json::Value::from)
                    .expect("the multiple is validated finite at construction");
        }
        object
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advice::historical_distribution::{Percentile, QuantileMethod};
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::UtcTimestamp;
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
        WindowSemanticKey,
    };
    use proptest::prelude::*;

    const MICROS_PER_CREDIT: i64 = 1_000_000;

    fn credits(whole: i64) -> Credits {
        Credits::from_micros(whole * MICROS_PER_CREDIT)
    }

    fn interval_between(low_whole: i64, high_whole: i64) -> Interval<Credits> {
        Interval::new(credits(low_whole), credits(high_whole)).expect("valid test interval")
    }

    fn make_window(key: &str, used_ppm: i32) -> MeterWindow {
        MeterWindow::new(
            WindowSemanticKey::new(key),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::RoundedToNearest,
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            NominalWindowDuration::from_nanos(1_000_000_000),
        )
    }

    fn labels_enabled() -> CanRunVerdictConfig {
        CanRunVerdictConfig {
            labels_enabled: true,
            ample_margin_multiple: AmpleMarginMultiple::new(2.0).unwrap(),
            headroom_bound: CanRunHeadroomBound::Low,
        }
    }

    fn labels_disabled() -> CanRunVerdictConfig {
        CanRunVerdictConfig {
            labels_enabled: false,
            ample_margin_multiple: AmpleMarginMultiple::new(2.0).unwrap(),
            headroom_bound: CanRunHeadroomBound::Low,
        }
    }

    /// The plan's worked example (PLAN.md 51): `account:5h` at 38.0% remaining
    /// with headroom 3,040-3,420 credits, `model-x:weekly` at 52.0% with
    /// headroom 1,040-1,180, history n=23 median 640 p25-p75 550-940 p90 1,520.
    /// Margins are headroom minus central range: 2,100-2,870 and 100-630.
    fn worked_example_windows() -> (MeterWindow, MeterWindow) {
        (
            make_window("account:5h", 620_000),
            make_window("model-x:weekly", 480_000),
        )
    }

    fn worked_example_task() -> TaskReferenceInput {
        TaskReferenceInput {
            verdict: DistributionVerdict::Distribution {
                median: credits(640),
                central_range: interval_between(550, 940),
                central_low_percentile: Percentile::new(25).unwrap(),
                central_high_percentile: Percentile::new(75).unwrap(),
                upper_reference: credits(1520),
                upper_percentile: Percentile::new(90).unwrap(),
                quantile_method: QuantileMethod::NearestRank,
            },
            sample_count: 23,
        }
    }

    fn worked_example_evaluated<'a>(
        account: &'a MeterWindow,
        weekly: &'a MeterWindow,
    ) -> Vec<WindowHeadroom<'a>> {
        vec![
            WindowHeadroom::Known {
                window: account,
                headroom: interval_between(3040, 3420),
            },
            WindowHeadroom::Known {
                window: weekly,
                headroom: interval_between(1040, 1180),
            },
        ]
    }

    fn ready_of(assessment: CanRunAssessment<'_>) -> CanRunReady<'_> {
        match assessment {
            CanRunAssessment::Ready(ready) => ready,
            other @ (CanRunAssessment::InsufficientEvidence(_) | CanRunAssessment::Unknown(_)) => {
                panic!("expected a Ready assessment, got {other:?}")
            }
        }
    }

    /// The bead's done-when: the model-specific window limits while the
    /// lower-percentage window is reported separately.
    #[test]
    fn worked_example_selects_model_specific_limiting_window_and_reports_lower_percentage_window() {
        let (account, weekly) = worked_example_windows();
        let evaluated = worked_example_evaluated(&account, &weekly);
        let ready = ready_of(assess_can_run(
            &evaluated,
            &worked_example_task(),
            &labels_enabled(),
        ));

        assert_eq!(
            ready.limiting_window.semantic_key().as_str(),
            "model-x:weekly"
        );
        assert_eq!(
            ready.lowest_percentage_window.semantic_key().as_str(),
            "account:5h"
        );
        assert!(ready.windows_differ);

        // The planted negative: a percentage-driven selection would name
        // account:5h limiting; the calibrated margins say otherwise.
        let margins: Vec<(String, Interval<Credits>)> = ready
            .per_window
            .iter()
            .map(|assessment| {
                (
                    assessment.window.semantic_key().as_str().to_string(),
                    assessment.margin,
                )
            })
            .collect();
        assert_eq!(
            margins,
            vec![
                ("model-x:weekly".to_string(), interval_between(100, 630)),
                ("account:5h".to_string(), interval_between(2100, 2870)),
            ]
        );

        let basis = ready.label_basis.expect("labels are enabled");
        assert_eq!(basis.label, CanRunVerdictLabel::Marginal);
        assert_eq!(basis.ample_threshold, credits(3040));
        // 1,040 < 3,040 so not AMPLE; median 640 within headroom so not
        // INSUFFICIENT: the example's own stated assessment.
        assert_eq!(basis.limiting_headroom, interval_between(1040, 1180));

        let text = CanRunAssessment::Ready(ready).describe();
        assert!(
            text.contains("limiting calibrated window: model-x:weekly"),
            "{text}"
        );
        assert!(
            text.contains("lowest remaining percentage: account:5h"),
            "{text}"
        );
        assert!(text.contains("MARGINAL"), "{text}");
    }

    /// Increasing the historical-consumption interval cannot improve the
    /// limiting margin: both margin ends move down or stay.
    #[test]
    fn increasing_task_consumption_cannot_improve_the_limiting_margin_hand_picked() {
        let window = make_window("win", 500_000);
        let headroom = interval_between(1000, 1200);
        let evaluated = [WindowHeadroom::Known {
            window: &window,
            headroom,
        }];

        let task_at = |low: i64, high: i64| TaskReferenceInput {
            verdict: DistributionVerdict::Distribution {
                median: credits(low),
                central_range: interval_between(low, high),
                central_low_percentile: Percentile::new(25).unwrap(),
                central_high_percentile: Percentile::new(75).unwrap(),
                upper_reference: credits(high),
                upper_percentile: Percentile::new(90).unwrap(),
                quantile_method: QuantileMethod::NearestRank,
            },
            sample_count: 23,
        };

        let before = ready_of(assess_can_run(
            &evaluated,
            &task_at(100, 200),
            &labels_enabled(),
        ));
        let after = ready_of(assess_can_run(
            &evaluated,
            &task_at(300, 500),
            &labels_enabled(),
        ));
        let before_margin = before.per_window[0].margin;
        let after_margin = after.per_window[0].margin;
        // [1000-200, 1200-100] = [800, 1100] before;
        // [1000-500, 1200-300] = [500, 900] after.
        assert_eq!(before_margin, interval_between(800, 1100));
        assert_eq!(after_margin, interval_between(500, 900));
        assert!(after_margin.lower().micros() <= before_margin.lower().micros());
        assert!(after_margin.upper().micros() <= before_margin.upper().micros());
    }

    proptest! {
        /// Property: a component-wise larger task interval yields a
        /// component-wise no-better limiting margin, over generated intervals.
        #[test]
        fn prop_increasing_task_consumption_cannot_improve_limiting_margin(
            head_low in 0i64..1_000_000i64,
            head_width in 0i64..100_000i64,
            task_low in 0i64..1_000_000i64,
            task_width in 0i64..100_000i64,
            raise_low in 0i64..100_000i64,
            raise_high in 0i64..100_000i64,
        ) {
            let window = make_window("prop-win", 500_000);
            let headroom = Interval::new(
                Credits::from_micros(head_low),
                Credits::from_micros(head_low + head_width),
            )
            .unwrap();
            let evaluated = [WindowHeadroom::Known { window: &window, headroom }];

            let raised_low = task_low + raise_low;
            let raised_high = (task_low + task_width + raise_high).max(raised_low);
            let task_at = |low: i64, high: i64| TaskReferenceInput {
                verdict: DistributionVerdict::Distribution {
                    median: Credits::from_micros(low),
                    central_range: Interval::new(
                        Credits::from_micros(low),
                        Credits::from_micros(high),
                    )
                    .unwrap(),
                    central_low_percentile: Percentile::new(25).unwrap(),
                    central_high_percentile: Percentile::new(75).unwrap(),
                    upper_reference: Credits::from_micros(high),
                    upper_percentile: Percentile::new(90).unwrap(),
                    quantile_method: QuantileMethod::NearestRank,
                },
                sample_count: 23,
            };

            let config = labels_enabled();
            let before = assess_can_run(&evaluated, &task_at(task_low, task_low + task_width), &config);
            let after = assess_can_run(&evaluated, &task_at(raised_low, raised_high), &config);
            let (CanRunAssessment::Ready(before_ready), CanRunAssessment::Ready(after_ready)) =
                (before, after)
            else {
                prop_assert!(false, "both inputs are sufficient with known windows");
                return Ok(());
            };
            prop_assert!(
                after_ready.per_window[0].margin.lower().micros()
                    <= before_ready.per_window[0].margin.lower().micros(),
                "raising consumption improved the margin lower end"
            );
            prop_assert!(
                after_ready.per_window[0].margin.upper().micros()
                    <= before_ready.per_window[0].margin.upper().micros(),
                "raising consumption improved the margin upper end"
            );
        }
    }

    /// Insufficient history refuses with its missing facts and invents no task
    /// or margin interval.
    #[test]
    fn insufficient_evidence_reports_missing_facts_and_no_numeric_intervals() {
        let (account, weekly) = worked_example_windows();
        let evaluated = worked_example_evaluated(&account, &weekly);
        let task = TaskReferenceInput {
            verdict: DistributionVerdict::InsufficientEvidence { min_samples: 12 },
            sample_count: 3,
        };
        let assessment = assess_can_run(&evaluated, &task, &labels_enabled());
        let CanRunAssessment::InsufficientEvidence(refusal) = &assessment else {
            panic!("expected INSUFFICIENT_EVIDENCE, got {assessment:?}");
        };
        assert_eq!(refusal.sample_count, 3);
        assert_eq!(refusal.min_samples, 12);
        assert_eq!(refusal.missing.len(), 1);
        assert_eq!(refusal.missing[0].subject, "historical_task_evidence");

        let text = assessment.describe();
        assert!(text.contains("INSUFFICIENT_EVIDENCE"), "{text}");
        assert!(text.contains("historical_task_evidence"), "{text}");
        assert!(
            !text.contains("margin"),
            "no margin interval may be invented: {text}"
        );
        assert!(
            !text.contains("headroom"),
            "no headroom interval may be printed: {text}"
        );
        assert!(
            !text.contains("cr"),
            "no credit amount may be printed: {text}"
        );

        let json = assessment.to_json_value();
        assert_eq!(json["verdict"], "INSUFFICIENT_EVIDENCE");
        assert!(json.get("margin_micros").is_none());
        assert!(json.get("limiting_margin_micros").is_none());
        assert!(json.get("task").is_none());
    }

    /// An uncalibrated constraining window refuses, names the missing
    /// calibration, and prints no margin interval.
    #[test]
    fn missing_calibration_reports_unknown_names_the_window_and_prints_no_margin() {
        let (account, weekly) = worked_example_windows();
        let evaluated = vec![
            WindowHeadroom::Known {
                window: &account,
                headroom: interval_between(3040, 3420),
            },
            WindowHeadroom::Unknown { window: &weekly },
        ];
        let assessment = assess_can_run(&evaluated, &worked_example_task(), &labels_enabled());
        let CanRunAssessment::Unknown(refusal) = &assessment else {
            panic!("expected UNKNOWN, got {assessment:?}");
        };
        assert_eq!(refusal.missing.len(), 1);
        assert_eq!(refusal.missing[0].subject, "model-x:weekly");

        let text = assessment.describe();
        assert!(text.contains("UNKNOWN"), "{text}");
        assert!(text.contains("model-x:weekly"), "{text}");
        assert!(
            !text.contains("margin"),
            "no margin interval may be invented: {text}"
        );
        assert!(
            !text.contains("cr"),
            "no credit amount may be printed: {text}"
        );

        let json = assessment.to_json_value();
        assert_eq!(json["verdict"], "UNKNOWN");
        assert!(json.get("margin_micros").is_none());
        assert!(json.get("limiting_margin_micros").is_none());
    }

    #[test]
    fn no_evaluated_windows_is_unknown_with_a_named_missing_fact() {
        let assessment = assess_can_run(&[], &worked_example_task(), &labels_enabled());
        let CanRunAssessment::Unknown(refusal) = &assessment else {
            panic!("expected UNKNOWN, got {assessment:?}");
        };
        assert_eq!(refusal.missing.len(), 1);
        assert_eq!(refusal.missing[0].subject, "constraining_windows");
    }

    /// Label behavior follows the `aub-jsq` decision carried in configuration:
    /// with labels selected the configured thresholds decide, with labels
    /// rejected no label or threshold path exists anywhere in the output.
    #[test]
    fn label_behavior_follows_the_configured_jsq_decision() {
        let (account, weekly) = worked_example_windows();
        let evaluated = worked_example_evaluated(&account, &weekly);

        // Configured thresholds are exercised: the same evidence is MARGINAL
        // at the decided multiple and AMPLE at a looser one.
        let marginal = ready_of(assess_can_run(
            &evaluated,
            &worked_example_task(),
            &labels_enabled(),
        ));
        assert_eq!(
            marginal.label_basis.expect("labels on").label,
            CanRunVerdictLabel::Marginal
        );
        let loose = CanRunVerdictConfig {
            labels_enabled: true,
            ample_margin_multiple: AmpleMarginMultiple::new(0.5).unwrap(),
            headroom_bound: CanRunHeadroomBound::Low,
        };
        let ample = ready_of(assess_can_run(&evaluated, &worked_example_task(), &loose));
        assert_eq!(
            ample.label_basis.expect("labels on").label,
            CanRunVerdictLabel::Ample
        );

        // Labels rejected: the same margins, no label, no threshold.
        let unlabeled = ready_of(assess_can_run(
            &evaluated,
            &worked_example_task(),
            &labels_disabled(),
        ));
        assert!(unlabeled.label_basis.is_none());
        assert_eq!(unlabeled.per_window, marginal.per_window);
        let text = CanRunAssessment::Ready(unlabeled.clone()).describe();
        for token in ["AMPLE", "MARGINAL", "INSUFFICIENT", "UNKNOWN", "threshold"] {
            assert!(
                !text.contains(token),
                "labels-off text must not contain {token}: {text}"
            );
        }
        let json = CanRunAssessment::Ready(unlabeled).to_json_value();
        assert!(json.get("verdict").is_none());
        assert!(
            json.as_object()
                .expect("object")
                .keys()
                .all(|key| !key.contains("threshold")),
            "labels-off JSON must have no threshold path: {json}"
        );
    }

    /// Boundary pins: meeting the multiple exactly is AMPLE, and a median
    /// exactly at the headroom end still fits (MARGINAL, not INSUFFICIENT).
    #[test]
    fn classification_boundaries_stay_out_of_the_stronger_claim() {
        let window = make_window("win", 500_000);
        let task_at = |median: i64, upper: i64| TaskReferenceInput {
            verdict: DistributionVerdict::Distribution {
                median: credits(median),
                central_range: interval_between(median.min(upper), median.max(upper)),
                central_low_percentile: Percentile::new(25).unwrap(),
                central_high_percentile: Percentile::new(75).unwrap(),
                upper_reference: credits(upper),
                upper_percentile: Percentile::new(90).unwrap(),
                quantile_method: QuantileMethod::NearestRank,
            },
            sample_count: 23,
        };
        // Headroom low 1,000, upper reference 500 at multiple 2.0: threshold
        // exactly 1,000, met exactly, so AMPLE.
        let evaluated = [WindowHeadroom::Known {
            window: &window,
            headroom: interval_between(1000, 1200),
        }];
        let ample = ready_of(assess_can_run(
            &evaluated,
            &task_at(100, 500),
            &labels_enabled(),
        ));
        assert_eq!(
            ample.label_basis.expect("labels on").label,
            CanRunVerdictLabel::Ample
        );

        // Median exactly at the headroom low end still fits: MARGINAL.
        let marginal = ready_of(assess_can_run(
            &evaluated,
            &task_at(1000, 4000),
            &labels_enabled(),
        ));
        assert_eq!(
            marginal.label_basis.expect("labels on").label,
            CanRunVerdictLabel::Marginal
        );

        // Median one credit above the headroom low end no longer fits.
        let insufficient = ready_of(assess_can_run(
            &evaluated,
            &task_at(1001, 4000),
            &labels_enabled(),
        ));
        assert_eq!(
            insufficient.label_basis.expect("labels on").label,
            CanRunVerdictLabel::Insufficient
        );
    }

    /// A threshold-produced label never travels without its basis: one text
    /// line carries verdict, intervals and thresholds, and the JSON verdict
    /// sits beside interval and threshold fields of the same object.
    #[test]
    fn a_verdict_is_never_emitted_without_its_interval_and_threshold() {
        let (account, weekly) = worked_example_windows();
        let evaluated = worked_example_evaluated(&account, &weekly);
        let assessment = assess_can_run(&evaluated, &worked_example_task(), &labels_enabled());

        let text = assessment.describe();
        let verdict_lines: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("MARGINAL"))
            .collect();
        assert_eq!(
            verdict_lines.len(),
            1,
            "exactly one line carries the verdict: {text}"
        );
        let line = verdict_lines[0];
        assert!(
            line.contains("1040"),
            "the headroom interval travels: {line}"
        );
        assert!(line.contains("threshold"), "the threshold travels: {line}");
        assert!(line.contains("3040"), "the threshold value travels: {line}");

        let json = assessment.to_json_value();
        let object = json.as_object().expect("top-level object");
        assert_eq!(object["verdict"], "MARGINAL");
        assert!(object.contains_key("limiting_headroom_micros"), "{json}");
        assert!(object.contains_key("ample_threshold_micros"), "{json}");
        assert!(
            object.contains_key("insufficient_threshold_micros"),
            "{json}"
        );
    }

    /// Rendered outputs use quantitative nouns only: no safety or enforcement
    /// language in text or JSON, across every assessment shape.
    #[test]
    fn rendered_outputs_contain_no_safety_or_enforcement_language() {
        const FORBIDDEN: &[&str] = &[
            "safe",
            "enforce",
            "allow",
            "permit",
            "block",
            "deny",
            "denied",
            "approv",
            "authoriz",
            "forbid",
            "guarantee",
            "proceed",
            "halt",
            "go-ahead",
            "green light",
            "clear",
        ];
        let (account, weekly) = worked_example_windows();
        let evaluated = worked_example_evaluated(&account, &weekly);
        let unknown_evaluated = vec![
            WindowHeadroom::Known {
                window: &account,
                headroom: interval_between(3040, 3420),
            },
            WindowHeadroom::Unknown { window: &weekly },
        ];
        let insufficient_task = TaskReferenceInput {
            verdict: DistributionVerdict::InsufficientEvidence { min_samples: 12 },
            sample_count: 3,
        };

        let cases = vec![
            assess_can_run(&evaluated, &worked_example_task(), &labels_enabled()),
            assess_can_run(&evaluated, &worked_example_task(), &labels_disabled()),
            assess_can_run(&evaluated, &insufficient_task, &labels_enabled()),
            assess_can_run(
                &unknown_evaluated,
                &worked_example_task(),
                &labels_enabled(),
            ),
            assess_can_run(&[], &worked_example_task(), &labels_enabled()),
        ];
        for assessment in &cases {
            let text = assessment.describe().to_lowercase();
            let json = assessment.to_json_value().to_string().to_lowercase();
            for forbidden in FORBIDDEN {
                assert!(
                    !text.contains(forbidden),
                    "rendered text contains {forbidden:?}: {text}"
                );
                assert!(
                    !json.contains(forbidden),
                    "rendered JSON contains {forbidden:?}: {json}"
                );
            }
        }
    }

    #[test]
    fn ample_margin_multiple_rejects_non_positive_and_non_finite_values() {
        assert!(AmpleMarginMultiple::new(2.0).is_some());
        assert!(AmpleMarginMultiple::new(0.0).is_none());
        assert!(AmpleMarginMultiple::new(-1.0).is_none());
        assert!(AmpleMarginMultiple::new(f64::NAN).is_none());
        assert!(AmpleMarginMultiple::new(f64::INFINITY).is_none());
    }

    #[test]
    fn headroom_bound_round_trips_its_stable_name() {
        assert_eq!(CanRunHeadroomBound::Low.as_str(), "low");
        assert_eq!(
            CanRunHeadroomBound::parse("low"),
            Some(CanRunHeadroomBound::Low)
        );
        assert_eq!(CanRunHeadroomBound::parse("high"), None);
        assert_eq!(CanRunHeadroomBound::parse(""), None);
    }

    #[test]
    fn verdict_labels_render_under_their_documented_tokens() {
        assert_eq!(CanRunVerdictLabel::Ample.as_str(), "AMPLE");
        assert_eq!(CanRunVerdictLabel::Marginal.as_str(), "MARGINAL");
        assert_eq!(CanRunVerdictLabel::Insufficient.as_str(), "INSUFFICIENT");
        assert_eq!(
            CanRunVerdictLabel::InsufficientEvidence.as_str(),
            "INSUFFICIENT_EVIDENCE"
        );
        assert_eq!(CanRunVerdictLabel::Unknown.as_str(), "UNKNOWN");
    }
}
