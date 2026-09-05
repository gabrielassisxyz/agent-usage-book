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
| 1 | Provider quota attempts and observations are irreplaceable evidence. | tests/backup.rs | a_backup_taken_while_a_writer_is_active_verifies_with_both_checks_passing |
| 2 | Transcript-derived spend is reconstructible. | tests/rebuild_reproducibility.rs | tests::rebuild_then_reingest_reproduces_the_canonical_events_and_materializations |
| 3 | No semantically meaningful raw numeric primitive crosses a domain boundary. | src/domain/tokens.rs | tests::each_newtype_round_trips_its_value_including_the_maximum |
| 4 | Freshness is an exhaustive three-way state. | src/domain/freshness.rs | tests::expanding_stale_reason_does_not_change_the_freshness_variant_count |
| 5 | Sampling-attempt outcome is not the same dimension as freshness. | src/domain/attempt.rs | tests::started_and_result_are_separate_types_correlated_by_attempt_id |
| 6 | A failed source never produces zero. | src/domain/freshness.rs | tests::a_failure_before_any_success_yields_stale_with_no_value |
| 7 | A historical value is never presented without its actual observation time. | src/domain/freshness.rs | tests::a_failure_after_a_good_observation_yields_stale_with_last_good_and_a_named_reason |
| 8 | Provider credentials are resolved from named-account configuration, never accidental ambient state. | src/auth.rs | tests::file_kind_resolves_to_material_and_context_id |
| 9 | All replay deduplication passes through one semantic event-identity framework and database uniqueness constraint. | src/store/usage_occurrence.rs | tests::a_duplicate_strong_identity_fails_the_direct_insert |
| 10 | Unknown token components block complete conversions. | tests/cost_model.rs | tests::conversion_fails_closed_on_unknown_components |
| 11 | Cost-model coefficients and window calibrations are distinct versioned witnesses. | src/domain/provenance.rs | tests::witness_identifier_types_are_distinct |
| 12 | Calibrated values have immutable IDs and no consumer-side literals. | tests/calibration_single_source.rs | calibration_supersession_moves_calibrate_show_and_spend_window_equivalent_together |
| 13 | Calibration cannot cross incompatible plan tiers, reset boundaries, or provider semantics. | tests/calibration_recovery_and_rejection.rs | test_eleven_rejection_conditions_table_driven |
| 14 | Passive calibration is evidence and candidate generation, not automatic truth. | src/calibration/passive.rs | tests::passive_calibration_produces_candidate_and_never_activates |
| 15 | `status` never opens SQLite, performs network I/O, or writes. | src/cli.rs | tests::the_status_function_performs_only_the_status_contract |
| 16 | The projection is disposable and contains no stored freshness boolean. | src/projection.rs | tests::deleting_the_projection_and_republishing_reproduces_it_byte_for_byte |
| 17 | Task ambiguity becomes explicit overhead. | src/attribution/segment.rs | tests::task_attributed_plus_overhead_equals_total_input_usage |
| 18 | Dollar valuation and subscription credits are distinct dimensions. | tests/valuation.rs | tests::golden_hand_computed_exact_decimal_fixtures |
| 19 | Meter residual is diagnostic and is not automatically called hidden token spend. | tests/reconciliation.rs | unit_diagnostic_patterns_reported_as_patterns_not_causes |
| 20 | Estimated transcript usage is never silently promoted to measured usage. | src/evidence.rs | tests::quality_combine_never_recovers_measured_from_estimated |
| 21 | The friction ledger remains external and joins only through stable IDs. | tests/export.rs | tests::both_key_modes_produce_one_object_per_line_with_version_and_generations |
| 22 | Irreplaceable meter history is backed up and never automatically pruned. | tests/backup.rs | a_backup_is_not_a_blind_file_copy_of_a_live_wal_database |
| 23 | An outbound meter request is never begun before its attempt identity is durable. | tests/meter_attempt_crash.rs | tests::killed_between_start_and_result_leaves_exactly_one_start_with_no_result |
| 24 | An attempt without a terminal result is evidence of collector interruption, not evidence that no attempt occurred. | src/domain/freshness.rs | tests::started_attempt_past_command_horizon_yields_collector_interrupted |
| 25 | Provider response evidence and its normalized interpretation are separate records; a corrected adapter reinterprets retained evidence and never overwrites the earlier interpretation. | src/store/meter_evidence.rs | tests::switching_the_preference_keeps_both_interpretations_immutable |
| 26 | Workload feasibility is evaluated against every constraining window in calibrated credits, never against the lowest remaining percentage alone. | src/domain/window.rs | tests::display_and_advice_select_different_windows_when_calibrations_diverge |
| 27 | Coverage denominators come from the sampling policy that was in force over the interval, never from current configuration. | src/coverage.rs | tests::a_cadence_change_mid_interval_follows_the_historical_policy |

## Enforcement status

Of the 27 invariants above, 27 are enforced by mechanical checks present at HEAD (file paths and tests), and 0 are unenforced and tracked by open beads in the tracker.

## Maintaining this document

Each row is satisfied one of two ways: it names the file path and test that
enforce the invariant, or it names an open tracker bead that will. The audit
test `tests::every_invariant_names_existing_file_and_test_or_open_tracker_bead`
in `src/lib.rs` holds that rule, and its failure names every stale row number
with the bead id that row points at, so the repair is one commit rather than
an investigation.

The change that makes an invariant enforced repoints its row in the same
commit series: the Enforcing path column takes the enforcing file path, the
Test or constraint column takes the test name, and both enforcement-summary
counts are updated to match the table. Start the bead from
`docs/INVARIANT_BEAD_TEMPLATE.md`, which carries that update as an acceptance
criterion. No gate running before the close can observe a status the close
has not yet produced, so a row left pointing at a bead turns the audit red on
`main` only after that bead closes, in a run that touched none of it.
