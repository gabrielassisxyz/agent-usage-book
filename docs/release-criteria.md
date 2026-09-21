# Trustworthy first release criteria

PLAN.md section 44 defines the first useful release as seventeen criteria. This
document maps each one to the checks that discharge it, and `bin/release-criteria`
reads the two tables below as its single source: it runs every check and prints one
JSON line per criterion, then a summary line, and exits nonzero while any criterion
is unmet. The release workflow (`aub-n27.9`) refuses to publish on that exit status.

## Why this exists

The list in section 44 had no owner, which meant it would have been satisfied by
opinion. As a table of checks, a partially satisfied release is visible rather than
arguable: a criterion is `pass` only when every check mapped to it passed, `fail`
when any failed, `blocked` when a row names the bead that must land first, and
`unchecked` when no row maps it at all. Dropping a row therefore turns a criterion
`unchecked` instead of making it disappear.

Most criteria are discharged by a test another bead wrote; the row names that test by
file path and function, and `bin/release-criteria --list` fails when either no longer
exists, so a rename or a deletion is caught without building anything. The checks
owned by this document are in `tests/release_criteria_accumulated_series.rs`:

- criterion 14 is read from the ledger while the synthetic server holds the request
  open, so the attempt start is shown committed while its request is provably in
  flight, not only after the process is gone;
- criterion 15 replays every capsule a six-sample series left in the ledger through
  the adapter and requires it to re-derive both the stored windows and the response
  that was served;
- criterion 16 crosses a cadence change inside that series and requires every attempt
  to carry the snapshot in force when it started, and the coverage engine, fed the
  ledger's own snapshots, to owe the new cadence only after the change.

Criterion 6 depends on `strace`: the syscall-trace test returns early without it, so
the table carries an explicit row that fails when `strace` is absent rather than
letting that early return read as a pass.

## Running it

```bash
bin/release-criteria            # run every check; exit 0 only on 17 of 17
bin/release-criteria --list     # trace every row without running anything
bin/release-criteria --only 15  # one criterion's checks; the rest read `skipped`
bin/release-criteria --record   # run, then rewrite the line below
bin/release-criteria-selftest   # the checker's own mechanics, no toolchain needed
```

## Last run

Last run: never

## Criteria

Tab-separated: the criterion number, its stable slug, and the text of PLAN.md
section 44.

```tsv
number	slug	criterion
1	sample-without-ambient-credentials	every actively used named account can be sampled without depending on ambient current credentials
2	external-timer-invokes-sample-due	an external timer can invoke `aub sample --due`
3	attempts-persisted	successful and failed attempts are persisted
4	sampling-gaps-detectable	sampling gaps can be detected
5	stale-and-auth-required-survive	stale and auth-required states survive storage and reporting
6	status-reads-only-projection	`aub status` reads only the projection and never waits on remote I/O
7	provider-failure-isolated	one provider's failure does not suppress another account's sample
8	legacy-series-imported-honestly	the existing meter series has been imported without pretending its old samples are fresher than they are
9	projection-consistent-under-crash	the status projection and its SQLite source stay semantically consistent under crashes
10	dead-timer-identifiable	attempt coverage can identify a dead timer
11	spool-survives-writer-conflict	a successful network result survives a SQLite writer conflict via the pending spool
12	backup-verified-and-restored	a daily backup can be verified and restored
13	adapter-fixtures-real-shapes	provider adapter fixtures cover real response and error shapes
14	attempt-durable-before-request	an attempt is durable before its request leaves the process
15	response-evidence-retained	the sanitized provider response evidence behind each observation is retained
16	denominators-from-policy-in-force	coverage denominators are reconstructed from the sampling policy that was in force
17	no-hardcoded-cache-write-constant	no calibrated spend conversion depends on the old cache-write-incomplete constant
```

## Checks

Tab-separated: the criterion number, the row kind (`cargo`, `shell`, `e2e` or
`blocked`), the target (a test function, a shell command, or the unblocking bead) and
the path of the file that holds it (`-` for an inline shell command). The kinds are
described in the header of `bin/release-criteria`.

```tsv
number	kind	target	path
1	cargo	named_accounts_are_isolated_from_ambient_credentials	tests/sampler_batch.rs
2	e2e	-	tests/e2e/cases/019-fresh-machine-walkthrough.sh
2	e2e	-	tests/e2e/cases/018-scheduler-hook-integration.sh
2	cargo	every_example_uses_an_absolute_path_to_the_binary	tests/scheduler_examples.rs
3	cargo	one_success_one_auth_failure_one_timeout_persists_three_attempts_one_observation_and_one_projection	tests/sampler_batch.rs
3	cargo	matrix_positive_control_complete_writes_all_facts_and_exits_cleanly	tests/meter_attempt_crash.rs
4	cargo	simulated_sleep	tests/coverage.rs
4	e2e	-	tests/e2e/cases/013-coverage.sh
5	e2e	-	tests/e2e/cases/013-status-projection.sh
5	cargo	*	tests/freshness_fake_clock_and_property_tests.rs
6	shell	command -v strace >/dev/null	-
6	cargo	status_opens_no_socket_and_writes_nothing_to_the_state_directory	tests/status_syscall_trace.rs
6	shell	bin/checks/45-boundary-rules	bin/checks/45-boundary-rules
7	cargo	a_provider_hanging_until_the_budget_expires_does_not_block_another_accounts_observation	tests/sampler_batch.rs
8	cargo	integration_coverage_distinguishes_legacy_evidence_from_live_sampling	tests/legacy_meter_import.rs
8	e2e	-	tests/e2e/cases/016-legacy-meter-import.sh
9	cargo	killed_between_commit_and_publication_leaves_the_projection_older_and_never_ahead	tests/projection_publication.rs
10	cargo	timer_never_ran	tests/coverage.rs
10	e2e	-	tests/e2e/cases/013-coverage.sh
11	cargo	ingest_first_refuses_the_meter_commit_which_spools_and_drains_exactly_once	tests/ingest_meter_contention.rs
12	cargo	a_failed_verification_leaves_the_pointer_and_every_archive_untouched	tests/backup.rs
12	e2e	-	tests/e2e/cases/012-backup.sh
12	e2e	-	tests/e2e/cases/013-restore-drill.sh
13	cargo	*	tests/provider_adapter_anthropic.rs
13	cargo	*	tests/fixture_corpus_audit.rs
14	cargo	release_criterion_14_the_attempt_is_durable_while_its_request_is_in_flight	tests/release_criteria_accumulated_series.rs
14	cargo	matrix_point_2_killed_after_start_commit_before_request_leaves_interrupted_attempt	tests/meter_attempt_crash.rs
15	cargo	release_criterion_15_every_accumulated_observation_replays_from_its_retained_evidence	tests/release_criteria_accumulated_series.rs
15	cargo	*	tests/evidence_capsule.rs
16	cargo	release_criterion_16_accumulated_attempts_carry_the_policy_in_force_and_denominators_follow_it	tests/release_criteria_accumulated_series.rs
16	cargo	a_prospective_cadence_change_leaves_earlier_denominators_alone	tests/coverage.rs
17	shell	bin/checks/55-coefficient-tombstone	bin/checks/55-coefficient-tombstone
17	shell	bin/coefficient-tombstone-selftest	bin/coefficient-tombstone-selftest
17	cargo	integration_fit_evidence_imports_as_history_and_hardcoded_copy_is_refused	tests/legacy_calibration_import.rs
```
