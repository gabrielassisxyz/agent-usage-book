# Invariants

This document repeats the 27 invariants from the design (PLAN.md section 42) in
the source tree, each with the module that enforces it and the test, lint or
database constraint that would catch a violation. A rule in a 5,000 line design
document is read once by whoever converts it into beads; a rule here is read by
whoever is about to break it.

## The five correctness dimensions

The system has five independent correctness dimensions, and none substitutes
for another:

1. **Unit**: what physical quantity is this?
2. **Freshness**: how current is this remote reading?
3. **Coverage**: does this value include everything its name implies?
4. **Evidence quality**: were the included values measured or reconstructed?
5. **Provenance**: what evidence and conversion witnesses justify it?

An exact but stale meter reading is stale. A fresh provider reading may still
be incomplete if the provider omits a required window. A current token estimate
reconstructed from characters is still an estimate, not measured usage. A
numerically plausible result without a provenance chain is not publishable.

## The invariants

Numbering matches PLAN.md section 42, so a reference to invariant 23 means the
same thing in both documents.

| # | Invariant | Enforcing path | Test or constraint |
|---:|---|---|---|
| 1 | Provider quota attempts and observations are irreplaceable evidence. | unenforced aub-sth.12 | (backup restore test) |
| 2 | Transcript-derived spend is reconstructible. | unenforced aub-lqe.11 | (rebuild determinism test) |
| 3 | No semantically meaningful raw numeric primitive crosses a domain boundary. | src/domain/tokens.rs | tests::each_newtype_round_trips_its_value_including_the_maximum |
| 4 | Freshness is an exhaustive three-way state. | src/domain/freshness.rs | tests::expanding_stale_reason_does_not_change_the_freshness_variant_count |
| 5 | Sampling-attempt outcome is not the same dimension as freshness. | src/domain/attempt.rs | tests::started_and_result_are_separate_types_correlated_by_attempt_id |
| 6 | A failed source never produces zero. | src/domain/freshness.rs | tests::a_failure_before_any_success_yields_stale_with_no_value |
| 7 | A historical value is never presented without its actual observation time. | src/domain/freshness.rs | tests::a_failure_after_a_good_observation_yields_stale_with_last_good_and_a_named_reason |
| 8 | Provider credentials are resolved from named-account configuration, never accidental ambient state. | src/auth.rs | tests::file_kind_resolves_to_material_and_context_id |
| 9 | All replay deduplication passes through one semantic event-identity framework and database uniqueness constraint. | src/store/usage_occurrence.rs | tests::a_duplicate_strong_identity_fails_the_direct_insert |
| 10 | Unknown token components block complete conversions. | unenforced aub-ai3.2 | (fail-closed conversion test) |
| 11 | Cost-model coefficients and window calibrations are distinct versioned witnesses. | src/domain/provenance.rs | tests::witness_identifier_types_are_distinct |
| 12 | Calibrated values have immutable IDs and no consumer-side literals. | unenforced aub-c0b.13 | (single-source calibration proof) |
| 13 | Calibration cannot cross incompatible plan tiers, reset boundaries, or provider semantics. | unenforced aub-c0b.11 | (calibration rejection test) |
| 14 | Passive calibration is evidence and candidate generation, not automatic truth. | unenforced aub-c0b.7 | (passive candidate test) |
| 15 | `status` never opens SQLite, performs network I/O, or writes. | unenforced aub-me5.7 | (status no-store / no-network test) |
| 16 | The projection is disposable and contains no stored freshness boolean. | unenforced aub-me5.5 | (disposable projection test) |
| 17 | Task ambiguity becomes explicit overhead. | unenforced aub-eu7.2 | (temporal task segmentation test) |
| 18 | Dollar valuation and subscription credits are distinct dimensions. | unenforced aub-wyu.2 | (exact-money valuation test) |
| 19 | Meter residual is diagnostic and is not automatically called hidden token spend. | unenforced aub-dpn | (unexplained residual test) |
| 20 | Estimated transcript usage is never silently promoted to measured usage. | src/evidence.rs | tests::quality_combine_never_recovers_measured_from_estimated |
| 21 | The friction ledger remains external and joins only through stable IDs. | unenforced aub-xus.7 | (external friction ledger join test) |
| 22 | Irreplaceable meter history is backed up and never automatically pruned. | unenforced aub-sth.12 | (retention and restore test) |
| 23 | An outbound meter request is never begun before its attempt identity is durable. | tests/meter_attempt_crash.rs | tests::killed_between_start_and_result_leaves_exactly_one_start_with_no_result |
| 24 | An attempt without a terminal result is evidence of collector interruption, not evidence that no attempt occurred. | src/domain/freshness.rs | tests::started_attempt_past_command_horizon_yields_collector_interrupted |
| 25 | Provider response evidence and its normalized interpretation are separate records; a corrected adapter reinterprets retained evidence and never overwrites the earlier interpretation. | unenforced aub-sth.7 | (response evidence capsule test) |
| 26 | Workload feasibility is evaluated against every constraining window in calibrated credits, never against the lowest remaining percentage alone. | src/domain/window.rs | tests::display_and_advice_select_different_windows_when_calibrations_diverge |
| 27 | Coverage denominators come from the sampling policy that was in force over the interval, never from current configuration. | unenforced aub-me5.8 | (policy snapshot denominator test) |

## Enforcement status

Of the 27 invariants above, 12 are enforced by mechanical checks present at HEAD (file paths and tests), and 15 are unenforced and tracked by open beads in the tracker.
