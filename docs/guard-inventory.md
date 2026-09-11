# Guard inventory

One row per guard: what it protects, the test that asserts it, and the exact
mutation that must make that test fail. `bin/guard-mutations` reads the
machine-readable table below as its single source and replays every row.

## Why this exists

A passing suite proves nothing about whether a guard discriminates. Several
beads in this tracker carry acceptance criteria of the form "verified to fail
when the rule is removed" precisely because a test that cannot fail is not a
test. Those verifications used to happen once, by hand, in the session that
wrote the guard, and were never repeated. A later refactor that quietly
defeats a guard leaves the suite green, and the guard is then a comment.

This inventory plus its runner is what repeats them.

## Scope discipline

This is not general mutation testing. It is a fixed list of named guards with
named mutations: cheap to run and impossible to misread. A row is added here
exactly when a bead carries a verify-it-can-fail criterion, and for no other
reason. The consistency mode of the runner enforces that direction: every
bead whose acceptance text matches the marker list below must appear in at
least one row, or in the exclusion log with a stated reason.

## Membership rule

A guard is listed when its owning bead explicitly names a guard, a concrete
breaking mutation, and the expected failure signal. For the boundary-rule
registry the standing re-proof is `bin/boundary-rules-selftest`, which plants
each rule's violation on every run; rule rows cite the registry proof owner
(`aub-vcx.4`) together with the rule-owning bead. Compile-fail fixtures share
one harness, so the two harness rows prove the mechanism while the
fixture-to-bead map underneath names every covered bead.

Can-fail markers (matched against the Acceptance criteria and Tests sections
of each bead; the runner carries this same list):

- proved to fail, proven to fail, verified to fail, demonstrated to fail,
  made to fail, forced to fail, shown to fail, shown by mutation
- deliberately wrong, deliberate omission, broken deliberately, broken once,
  broken deliberately once, violated on purpose, on purpose once
- plant, planted, neutralise, neutralised, swapping them once, verified by
  swapping
- does not compile, fails to compile, breaks compilation
- must fail, must break, must go red, fails naming, goes red, observed to
  fail, observed to succeed, observing the failure, verified once
- fails when, fails if, fails the test (with an added, missing, changed,
  replaced, removed, edited, disabled condition), makes the scan fail
- exit 0 at the top of a check

## Wall-clock budget

`GUARD_BUDGET_SECONDS`, default 3600. The runner fails the run when the
budget is exceeded rather than silently truncating the list. The figure is
provisional: it covers warm-target runs (the dedicated mutation target
directory persists between runs the way a CI cache does; a cold build is a
one-time cost, not steady state). The review owns the measured figure and
adjusts the constant with its provenance, the way the e2e budget carries its
history in `bin/checks/60-e2e`. No lane reports wall-clock numbers; the full
run below is recorded as pass or fail only.

## Last successful full run

Last successful full run: none yet (inventory created with aub-71j.7, awaiting
the first recorded full pass).

The runner rewrites this line itself on a full-scope pass.

## Machine-readable inventory

Tab-separated, one guard per line after the header. Columns:

1. `guard`: stable id (`sh-`, `c-`, `e-` prefixes group shell, cargo and e2e rows)
2. `beads`: owning bead first, then mechanism beads, comma-separated
3. `scope`: `shell`, `cargo` or `e2e`
4. `command`: shell snippet run with the scratch copy as working directory
5. `mut`: `neuter`, `sed`, `append`, `plant`, `write`, `delete`, `replace`,
   `python`, `none` or `oc56` (see `bin/guard-mutations` for semantics)
6. `target`: relative path the mutation touches (empty when `none`/`oc56`)
7. `mut_arg`: program, line, content or old string (`%b`-expanded)
8. `mut_arg2`: new string for `replace` (empty otherwise)
9. `expect_re`: extended regex the mutated run must print
10. `sibling`: snippet that must keep its result in the same scratch copy
    (`run-e2e-sibling` for `e2e` rows: the verdict comparison is built in)
11. `sib_expect`: `0` or `nonzero`
12. `sib_re`: extended regex the sibling run must print (empty means exit
    status only); for `e2e` rows this names the sibling case file instead

```tsv
guard	beads	scope	command	mut	target	mut_arg	mut_arg2	expect_re	sibling	sib_expect	sib_re
sh-01	aub-vcx.4	shell	bin/checks/boundary-rules/01-presentation-no-provider-adapters	append	src/presentation/render.rs	use crate::meter;		presentation imports provider adapters	printf 'use crate::calibration;\n' >> src/transcripts.rs; bin/checks/boundary-rules/02-transcripts-no-calibration	nonzero	transcripts imports calibration
sh-02	aub-vcx.4	shell	bin/checks/boundary-rules/02-transcripts-no-calibration	append	src/transcripts.rs	use crate::calibration;		transcripts imports calibration	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-03	aub-vcx.4	shell	bin/checks/boundary-rules/03-meter-no-sqlite	append	src/meter.rs	use rusqlite::Connection;		meter references a SQLite crate directly	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-04	aub-vcx.4	shell	bin/checks/boundary-rules/04-cost-model-no-config	append	src/cost_model.rs	use crate::config;		cost_model reads configuration files directly	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-05	aub-vcx.4	shell	bin/checks/boundary-rules/05-advice-no-calibration	append	src/advice.rs	use crate::calibration;		advice constructs calibration constants	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-06	aub-vcx.4	shell	bin/checks/boundary-rules/06-status-no-http-transport	append	src/projection.rs	use ureq::get;		status references the HTTP transport layer	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-07	aub-vcx.4	shell	bin/checks/boundary-rules/07-provider-adapters-no-credential-paths	append	src/meter.rs	use crate::auth;		provider adapters resolve credential paths	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-08	aub-vcx.4,aub-rif.10	shell	bin/checks/boundary-rules/08-window-selection-ownership	append	src/advice.rs	lowest_remaining_fraction_window(windows, model);		window selection is recomputed outside	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-09	aub-vcx.4,aub-xus.1	shell	bin/checks/boundary-rules/09-presentation-no-store-or-calibration	append	src/presentation/render.rs	use crate::store;		presentation imports store or calibration	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-10	aub-vcx.4,aub-me5.2	shell	bin/checks/boundary-rules/10-freshness-constructed-only-in-domain	append	src/advice.rs	pub fn bad() { let _ = Freshness::AuthRequired { last_good: None, latest_attempt: crate::domain::attempt::AttemptId::new(1) }; }		Freshness value constructed outside	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-11	aub-vcx.4,aub-sth.3	shell	bin/checks/boundary-rules/11-store-connections-through-one-function	append	src/store/account.rs	rusqlite::Connection::open("foo");		a SQLite connection is opened outside	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-12	aub-vcx.4,aub-eun.2	shell	bin/checks/boundary-rules/12-transport-no-ureq-outside-transport	append	src/advice.rs	use ureq::Agent;		ureq referenced outside	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-13	aub-vcx.4,aub-lqe.3	shell	bin/checks/boundary-rules/14-parser-no-cost-or-meter	append	src/transcripts.rs	use crate::cost_model;		transcript parsers reach a cost model	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-14	aub-vcx.4,aub-sth.4	shell	bin/checks/boundary-rules/15-status-no-migration	append	src/projection.rs	use crate::store::migrate;		status path references the migration framework	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-15	aub-6gco	shell	bin/checks/boundary-rules/18-no-system-clock-outside-time	append	src/advice.rs	pub fn bad() { let _ = std::time::SystemTime::now(); }		system clock read outside	printf 'use crate::meter;\n' >> src/presentation/render.rs; bin/checks/boundary-rules/01-presentation-no-provider-adapters	nonzero	presentation imports provider adapters
sh-16	aub-me5.7	shell	bin/checks/boundary-rules/19-status-no-store-connection	replace	src/cli.rs	fn status(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {	fn status(clock: &impl Clock, level: Level, invocation: &Invocation) -> Result<(), Error> {\n    let _planted_store_edge = crate::store::connection::open;	the status workflow references the store	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-17	aub-eun.2	shell	bin/checks/boundary-rules/13-no-async-runtime-dependency	python	Cargo.toml	import sys\nt = open("Cargo.toml").read()\nold = "[dependencies]\n"\nassert t.count(old) == 1, "dependencies header not unique"\nt = t.replace(old, old + 'tokio = "1.99.0"\n', 1)\nopen("Cargo.toml", "w").write(t)\nl = open("Cargo.lock").read()\nold2 = ' "toml",\n'\nassert old2 in l, "lock anchor not found"\nl = l.replace(old2, old2 + ' "tokio",\n', 1)\npkg = '[[package]]\nname = "tokio"\nversion = "1.99.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "0000000000000000000000000000000000000000000000000000000000000000"\n\n'\nl = l.rstrip("\n") + "\n\n" + pkg\nopen("Cargo.lock", "w").write(l)		Forbidden async runtime detected	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-18	aub-f7gt	shell	bin/checks/boundary-rules/16-sql-only-in-store	append	src/meter.rs	SELECT 1		SQL statement keyword found outside src/store/	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-19	aub-lveh	shell	bin/checks/boundary-rules/17-provider-adapters-no-file-writes	append	src/meter/adapter.rs	std::fs::write("/tmp/guard_probe", "probe");		provider adapters write files	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-20	aub-vcx.4	shell	bin/checks/boundary-rules/00-declared-policy	plant	src/zz_guard_probe_00.rs	pub fn probe() {}		module .* declares no dependency policy	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-21	aub-hao6	shell	bin/checks/boundary-rules/03-meter-no-sqlite	plant	src/meter/zz_guard_probe.rs	use rusqlite::Connection;		src/meter/zz_guard_probe.rs	printf 'use crate::calibration;\n' >> src/transcripts.rs; bin/checks/boundary-rules/02-transcripts-no-calibration	nonzero	02-transcripts-no-calibration
sh-22	aub-vcx.4	shell	bin/boundary-rules-selftest	replace	bin/checks/45-boundary-rules	    if [ ! -r "$rule" ]; then\n        echo "==> boundary-rule $name: FAIL (unreadable)"\n        fail=1\n        continue\n    fi		FAIL: unreadable reason	printf 'use crate::calibration;\n' >> src/transcripts.rs; bin/checks/45-boundary-rules	nonzero	02-transcripts-no-calibration
sh-23	aub-fon.4	shell	bin/checks/55-coefficient-tombstone	append	src/meter.rs	const LEGACY: u64 = 564_577;		the legacy coefficient 564,577 must not be hardcoded:	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-24	aub-8kq0	shell	bin/checks/80-gate-coverage "$SCRATCH"	neuter	bin/checks/10-format			10-format is neutered with exit 0	bin/checks/70-quantity-inventory	0	every pub struct/enum
sh-25	aub-rlfn	shell	tests/e2e/run.sh --self-test	sed	tests/e2e/run.sh	s|<"/dev/null"||		1 of 2	"$LIVE/tests/e2e/run.sh" --check-consistency	0	consistency: every command has a case
sh-26	aub-71j.4	shell	mkdir -p "$SCRATCH/opstate" "$SCRATCH/tcases" && printf 'CASE_ID="trivial"\nCASE_DESCRIPTION="t"\ncase_steps() {\n step "t" /bin/true\n}\ncase_assertions() {\n assert_exit 0 1\n}\n' > "$SCRATCH/tcases/001-t.sh" && AUB_BIN=/bin/true AUB_STATE_DIR="$SCRATCH/opstate" tests/e2e/run.sh --state-dir "$SCRATCH/opstate" --cases-dir "$SCRATCH/tcases" --runs-dir "$SCRATCH/runs-refuse"	none				refusing to run: state directory	mkdir -p "$SCRATCH/okstate" && AUB_BIN=/bin/true AUB_STATE_DIR="$SCRATCH/unused-operator-dir" tests/e2e/run.sh --state-dir "$SCRATCH/okstate" --cases-dir "$SCRATCH/tcases" --runs-dir "$SCRATCH/runs-ok" >/dev/null && jq -r '.cases[] | "\(.id)=\(.verdict)"' "$SCRATCH"/runs-ok/*/summary.json	0	trivial=pass
sh-27	aub-71j.4	shell	tests/e2e/run.sh --check-consistency	append	tests/e2e/command-surface.txt	zz-nonexistent-command		no end-to-end case for command 'zz-nonexistent-command'	SURFACE_FILE="$LIVE/tests/e2e/command-surface.txt" tests/e2e/run.sh --check-consistency	0	consistency: every command has a case
sh-28	aub-oc56	shell	oc56-shape	oc56		30000000		status_p99_budget_ns is not 15000000	oc56-shape-ok	0	status-latency-budget: artifact shape ok
sh-29	aub-vcx.2	shell	bin/ci-selftest	replace	bin/ci	    echo "==> bin/ci FAILED: ${failed_checks[*]}"\n    exit 1	    echo "==> bin/ci FAILED: ${failed_checks[*]}"\n    exit 0	FAIL: one-fail exit code	D="$SCRATCH/stub-ok"; mkdir -p "$D"; printf '#!/usr/bin/env bash\nexit 0\n' > "$D/10-ok"; chmod +x "$D/10-ok"; CI_CHECKS_DIR="$D" bin/ci	0	CI green.
sh-30	aub-mz3u,aub-hhdo	shell	bin/checks/80-batch-verify-close	replace	bin/checks/80-batch-verify-close	wave_close_allows_bead() { # $1 = bead id, $2 = optional repo_dir or INVARIANTS path	wave_close_allows_bead() { # $1 = bead id, $2 = optional repo_dir or INVARIANTS path\n    return 0	unit: wave_close_allows_bead allowed close of	bin/checks/75-commit-protocol	0	commit-protocol
sh-31	aub-rif.12,aub-qgng	shell	bin/checks/70-quantity-inventory	append	src/domain/rows.rs	pub struct ZzGuardProbeQuantity;		not in docs/domain-quantity-inventory.md: ZzGuardProbeQuantity	bin/checks/45-boundary-rules	0	rules pass
c-01	aub-vcx.5,aub-71j.1,aub-rif.12,aub-rif.2,aub-rif.8,aub-rif.3,aub-rif.4,aub-rif.9,aub-ai3.2,aub-sth.15,aub-eun.1	cargo	cargo test --test compile_fail compile_fail	write	tests/compile_fail/domain_quantities_no_default.rs	fn main() {}		fail to compile, but it succeeded	cargo test --test compile_fail every_fixture_has_captured_output	0	test result: ok
c-02	aub-rif.14,aub-ai3.4	cargo	cargo test --test compile_fail compile_fail	append	tests/compile_fail/quota_used_plus_money.rs	\nfn probe_reason() { let _x: () = 1u32; }		EXPECTED	cargo test --test compile_fail every_fixture_has_captured_output	0	test result: ok
c-03	aub-xus.4	cargo	cargo test --lib problem_code	sed	docs/problem-codes.md	/| DNS_FAILURE |/d		has no row matching	cargo test --lib error	0	test result: ok
c-04	aub-vcx.7	cargo	cargo test --lib documented_exit_class_table_matches_the_enum	sed	docs/exit-classes.md	s/^| 5 | Store |/| 9 | Store |/		has no row matching	cargo test --lib remote_and_local_failures_are_distinct_classes	0	test result: ok
c-05	aub-knw7	cargo	cargo test --test exit_classes exit_class_hook_covers_every_class	sed	docs/PLAN.md	/^[|] 8 [|] Local ingest/a \| 9 \| Local test hook only \|		class 9	cargo test --test exit_classes out_of_range_class_is_a_usage_error	0	test result: ok
c-06	aub-knw7	cargo	cargo test --lib domain::freshness	python	src/domain/freshness.rs	import sys\np = "src/domain/freshness.rs"\nlines = open(p).read().split("\n")\nidxs = [i for i, l in enumerate(lines) if "AttemptOutcome::Unreachable(failure_class) => {" in l]\nassert len(idxs) == 1, "unreachable arm opener not found exactly once"\nindent = "            "\nend = None\nfor j in range(idxs[0] + 1, len(lines)):\n    if lines[j] == indent + "}":\n        end = j\n        break\nassert end is not None, "arm close not found"\nnew_arm = indent + "AttemptOutcome::Unreachable(_) => Freshness::AuthRequired { last_good: input.last_good.clone(), latest_attempt: latest_attempt_id }, // probe: unreachable collapses to auth-required"\nlines[idxs[0]:end + 1] = [new_arm]\nopen(p, "w").write("\n".join(lines))		a_503_yields_stale|AuthRequired.*Stale|left.*right|FAILED	cargo test --lib domain::failure	0	test result: ok
c-07	aub-sth.8	cargo	cargo test --test schema_audit the_live_schema_meets_the_contract	sed	src/store/migrations/0001_account_sample_run_policy_snapshot.rs	s/ CHECK (last_observed_at >= first_observed_at)//		schema audit found regressions	cargo test --test schema_audit every_user_table_is_strict	0	test result: ok
c-08	aub-lqe.14	cargo	cargo test --test fixture_corpus_audit corpus_audit_is_complete	delete	tests/fixtures/transcripts/native/claude-no-usage.jsonl			no fixture|missing|incomplete|FAILED	cargo test --test fixture_corpus_audit sanitization_scan_finds_no_forbidden_patterns_in_the_corpus	0	test result: ok
c-09	aub-n27.4	cargo	cargo test --test fixture_corpus_audit sanitization_scan_finds_no_forbidden_patterns_in_the_corpus	append	tests/fixtures/transcripts/native/claude-cache.jsonl	sk-ant-probe-guard-inventory-000		matches forbidden patterns	cargo test --test fixture_corpus_audit corpus_audit_is_complete	0	test result: ok
c-10	aub-n27.7	cargo	cargo test --test doctor registry_contains_no_not_yet_available_entry	sed	src/doctor/checks.rs	/unmapped_accounts(ctx),/d		assertion .left == right. failed	cargo test --test doctor the_tripwire_transport_fires_when_invoked	0	test result: ok
c-11	aub-n27.10	cargo	cargo test --test doctor check_fails_unmapped_accounts	sed	src/doctor/checks.rs	s/CheckName::UnmappedAccounts => "attribution",/CheckName::UnmappedAccounts => "doctor",/		left: "doctor"	cargo test --test doctor registry_contains_no_not_yet_available_entry	0	test result: ok
c-12	aub-71j.5	cargo	cargo test --test failure_semantics_matrix class_swap_verification_for_rows_differing_only_in_persisted_failure_class row_04_http_rate_limit	sed	src/meter/anthropic.rs	s/FailureClass::RateLimited { retry_after }/FailureClass::HttpStatus(HttpStatusClass::ServerError)/		rows 03-06 must persist pairwise distinct	cargo test --test failure_semantics_matrix row_03_endpoint_unreachable row_06_malformed_payload_200	0	test result: ok
c-13	aub-71j.6	cargo	cargo test --test zero_is_data_and_no_silent_fallback cost_model_missing_term_refuses_and_never_returns_zero_credits human_and_json_agree_that_credits_are_unavailable_not_zero	sed	src/cost_model.rs	s/let term_missing = missing_terms(model, usage);/let term_missing: std::collections::BTreeSet<RequiredFact> = std::collections::BTreeSet::new();/		never returns zero|refuses	cargo test --test zero_is_data_and_no_silent_fallback provider_zero_used_is_a_real_fresh_zero_percent	0	test result: ok
c-14	aub-cab.5	cargo	cargo test --test advisory_metamorphic test_law_5_divergence_higher_percentage_window_can_be_limiting	replace	src/advice/verdict.rs	    let limiting_window = per_window\n        .first()	    let limiting_window = per_window\n        .last()	must be chosen as limiting	cargo test --test advisory_metamorphic test_law_1_more_current_remaining_quota_cannot_worsen_margin	0	test result: ok
c-15	aub-c0b.11	cargo	cargo test --test calibration_recovery_and_rejection test_disabling_targeted_rejection_guards_fails_cases	sed	src/calibration/passive.rs	s/    if interval.plan_tier_start != interval.plan_tier_end {/    if false {}/		len..1|assertion|left.*right	cargo test --test calibration_recovery_and_rejection test_generator_deterministic_from_seed	0	test result: ok
c-16	aub-dpn.1	cargo	cargo test --test reconciliation integration_synthetic_hidden_traffic_produces_positive_residual	sed	src/reconciliation.rs	s/let unexplained_residual = observed_meter_credits - locally_explained_credits;/let unexplained_residual = locally_explained_credits - observed_meter_credits;/		positive residual|assertion|left.*right	cargo test --test reconciliation unit_eligibility_condition_same_account_and_window_fails_in_isolation	0	test result: ok
c-17	aub-iatc	cargo	cargo test --test reconciliation unit_eligibility_condition_same_account_and_window_fails_in_isolation	sed	crates/test-support/src/migrated_schema.rs	s/            &registry(),/            &Vec::new(),/		no such table|missing	cargo test --test migration_matrix	0	test result: ok
c-18	aub-gmms	cargo	cargo test --lib evidence	sed	src/evidence.rs	s/            (Self::Measured, Self::Measured) => Self::Measured,/            (Self::Measured, Self::Measured) => Self::Mixed { methods, uncertainty },/		assertion|FAILED|panicked	cargo test --lib domain::tokens	0	test result: ok
c-19	aub-88b3	cargo	cargo test --lib meter::sampler::tests::bounded_concurrency_is_respected_and_reached	sed	src/meter/sampler.rs	s/        let worker_count = leased.len().min(self.max_concurrent_requests);/        let worker_count = leased.len();/		bound|concurrency|assertion|left.*right	cargo test --lib meter::sampler::tests::no_thread_outlives_the_command	0	test result: ok
c-20	aub-yr9c	cargo	cargo test -p test-support the_cache_path_changes_with_either_key_component	sed	crates/test-support/src/migrated_schema.rs	s/        .join(format!("migrated-schema-v{max_version}-{revision}"))/        .join(format!("migrated-schema-v{max_version}"))/		different source revision must produce a different cache path	cargo test -p test-support an_in_memory_clone_is_complete_and_independent	0	test result: ok
c-21	aub-yxsw	cargo	cargo test --test module_skeleton manifest_declares_no_forbidden_crate	replace	tests/module_skeleton.rs	        && deps.contains_key(name)	        && deps.get(name).and_then(toml::Value::as_str).is_some()	sub-table|manifest|assertion|left.*right	cargo test --test module_skeleton lowest_layers_reference_no_forbidden_crate	0	test result: ok
c-22	aub-2eda	cargo	cargo test --test build_script alternating_worktree_builds_with_a_shared_target_dir_never_fail	replace	build.rs	    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")\n        .expect("cargo must set CARGO_MANIFEST_DIR when running a build script");	    let manifest_dir = env!("CARGO_MANIFEST_DIR").to_string(); // probe: baked at compile time	build script must fail without a toolchain file|second build|cached|FAILED	cargo test --test build_script toolchain_file_resolves_under_the_given_manifest_dir	0	test result: ok
c-23	aub-vcx.9	cargo	cargo test --lib build_info::tests::crate_version_matches_the_manifest_on_disk	sed	src/build_info.rs	s/pub fn crate_version() -> &'static str {/pub fn crate_version() -> &'static str {\n    return "0.0.0";/		crate_version|assertion|left.*right	cargo test --lib build_info::tests::source_revision_is_a_full_lowercase_sha	0	test result: ok
c-24	aub-71j.2	cargo	cargo test --lib logging::tests::documented_event_vocabulary_matches_typed_enum	sed	docs/diagnostics.md	/^\| run_started |/d		has no row matching	cargo test --lib logging::tests	0	
c-25	aub-me5.7	cargo	cargo test --test status_syscall_trace trace_violations_catches_a_socket_call	sed	tests/status_syscall_trace.rs	s/starts_with("socket(")/starts_with("zz-no-such-call(")/		\[\]	cargo test --test status_syscall_trace trace_violations_permits_a_read_only_open_of_the_projection	0	test result: ok
c-26	aub-xus.6,aub-xus.9	cargo	cargo test --test provenance_graph every_seeded_command_field_resolves_by_typed_identifier	sed	src/report/models.rs	s/credits.into_iter().map(|credit| {/credits.into_iter().filter(|_| false).map(|credit| {/		has no provenance node	cargo test --test provenance_graph registry_covers_every_report_field_kind	0	test result: ok
c-27	aub-xus.8	cargo	cargo test --lib cli::tests::every_command_declares_a_shared_flag_policy	sed	src/cli.rs	/Command::Spend => FlagPolicy {/,/verbosity: FlagSupport::Accepted,/ s/verbosity: FlagSupport::Accepted,/verbosity: FlagSupport::Rejected { reason: "" },/		must accept verbosity	cargo test --lib cli::tests::every_command_declares_an_explain_policy	0	test result: ok
c-28	aub-eqpk	cargo	cargo test --lib invariant::tests::validation_skips_the_tracker_check_when_no_beads_directory_exists_at_all	replace	src/lib.rs	                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {\n                    return Ok(InvariantRowValidation::TrackerMembershipSkipped);\n                }	                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {\n                    return Err(format!("probe: tracker check disabled"));\n                }	TrackerMembershipSkipped|left == right	cargo test --lib invariant::tests::validation_fails_when_a_named_unenforced_bead_is_absent_from_a_present_tracker	0	test result: ok
c-29	aub-lh5r	cargo	cargo test --lib invariant::tests::every_invariant_names_existing_file_and_test_or_open_tracker_bead	sed	docs/INVARIANTS.md	32s|tests/backup.rs|tests/zz_no_such_file.rs|		zz_no_such_file|does not exist	cargo test --lib invariant::tests::invariants_document_matches_plan_section_42	0	test result: ok
c-30	aub-tjwn	cargo	bin/checks/30-test	python	src/lib.rs	import sys\np = "src/lib.rs"\nlines = open(p).read().split("\n")\nidxs = [i for i, l in enumerate(lines) if "INVARIANT_TRACKER_MEMBERSHIP_SKIPPED_MARKER" in l]\nassert len(idxs) >= 2, "marker const and print not found"\ndel lines[idxs[1]]\nopen(p, "w").write("\n".join(lines))		skip marker was not surfaced	grep -qF 'INVARIANT_TRACKER_MEMBERSHIP_SKIPPED' bin/checks/30-test	0	
c-31	aub-eun.11	cargo	cargo test --test sampler_batch named_accounts_are_isolated_from_ambient_credentials	sed	src/meter/anthropic.rs	s/    let raw = credential.expose().trim();/    let raw = ""; \/\/ probe: explicit credential wiring removed/		must carry explicit authorization|ambient credential must not reach	cargo test --test module_skeleton manifest_declares_no_forbidden_crate	0	test result: ok
c-32	aub-eu7.2	cargo	cargo test --lib a_deliberately_dropped_event_fails_the_conservation_assertion	replace	src/attribution/account_segment.rs	fn debug_assert_conserves(inputs: &AccountSegmentationInputs, result: &AccountSegmentationResult) {	fn debug_assert_conserves(inputs: &AccountSegmentationInputs, result: &AccountSegmentationResult) {\n    return; // probe: conservation assertion disabled	must fail the conservation assertion	cargo test --lib per_account_usage_plus_unknown_account_equals_total_input_usage	0	test result: ok
c-33	aub-eu7.3	cargo	cargo test --lib a_deliberately_dropped_window_fails_the_conservation_assertion	replace	src/attribution/segment.rs	fn debug_assert_conserves(inputs: &SegmentationInputs, result: &SegmentationResult) {	fn debug_assert_conserves(inputs: &SegmentationInputs, result: &SegmentationResult) {\n    return; // probe: conservation assertion disabled	must fail the conservation assertion	cargo test --lib rebuilding_with_different_tracker_data_changes_the_attribution	0	test result: ok
c-34	aub-wyu.2	cargo	cargo check -p agent-usage-book	replace	src/domain/tokens.rs	    CacheWrite,\n}	    CacheWrite,\n    ZzProbeKind,\n}	non-exhaustive patterns	cargo metadata --format-version 1 --no-deps >/dev/null && echo metadata-ok	0	metadata-ok
e-01	aub-vcx.7	e2e	run-e2e	sed	tests/e2e/cases/003-exit-classes.sh	s/assert_exit 4 3/assert_exit 5 3/		class 4|exit-class|assertion|FAIL	run-e2e-sibling	0	002-status.sh
e-02	aub-71j.7	e2e	run-e2e	sed	tests/e2e/cases/002-status.sh	s/? · stale · no successful sample/? · fresh · sample/		stale|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-03	aub-6fuo	e2e	run-e2e	sed	tests/e2e/cases/036-opencode-reset-precision.sh	s/assert_stdout_contains 4 "anomaly_count=0"/assert_stdout_contains 4 "anomaly_count=1"/		anomaly_count|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-04	aub-6fuo	e2e	run-e2e	sed	tests/e2e/cases/aub-z09n-codex-unstarted-window.sh	s/assert_stdout_contains 12 "primary||not_started|0"/assert_stdout_contains 12 "primary||not_started|1"/		not_started|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-05	aub-71j.5	e2e	run-e2e	sed	tests/e2e/cases/027-failure-semantics.sh	s/assert_exit 4 5/assert_exit 0 5/		class 4|exit-class|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-06	aub-71j.8	e2e	run-e2e	sed	tests/e2e/cases/013-coverage.sh	s/assert_exit 6 1/assert_exit 0 1/		assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-07	aub-71j.8	e2e	run-e2e	sed	tests/e2e/cases/017-now.sh	s/aub work-primary ? · stale/aub work-primary ? · fresh/		stale|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-08	aub-71j.8	e2e	run-e2e	sed	tests/e2e/cases/013-status-projection.sh	s/assert_stdout_contains 1 "62%"/assert_stdout_contains 1 "63%"/		62%|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-09	aub-71j.8	e2e	run-e2e	sed	tests/e2e/cases/013-restore-drill.sh	s/drill: passed=true/drill: passed=false/		passed|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-10	aub-71j.9	e2e	run-e2e	sed	tests/e2e/cases/007-spend.sh	s/input 4701 tokens/input 4702 tokens/		4701|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-11	aub-71j.9	e2e	run-e2e	sed	tests/e2e/cases/018-task.sh	s/events_inserted=1/events_inserted=2/		events_inserted|assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-12	aub-71j.9	e2e	run-e2e	sed	tests/e2e/cases/023-calibrate-controlled-experiment.sh	141s/assert_exit 0 3/assert_exit 1 3/		assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
e-13	aub-71j.9	e2e	run-e2e	sed	tests/e2e/cases/026-can-run.sh	244s/assert_exit 0 1/assert_exit 1 1/		assertion|FAIL	run-e2e-sibling	0	003-exit-classes.sh
```

## Fixture-to-bead map for the shared compile-fail harness

Rows `c-01` and `c-02` prove the harness discriminates: a fixture that
compiles makes the run fail, and a fixture that fails for another reason
mismatches its capture. Every fixture below is covered by that mechanism;
the bead named each fixture's can-fail proof at landing.

| Fixture | Bead |
|---|---|
| `domain_quantities_no_default.rs` | aub-rif.12 |
| `domain_quantities_no_display.rs` | aub-rif.12 |
| `domain_quantities_unwrap_or_default.rs` | aub-rif.12 |
| `domain_quantity_direct_construction.rs` | aub-rif.12 |
| `credits_default.rs` | aub-rif.2 |
| `credits_per_token_construction_outside_boundary.rs` | aub-rif.12 |
| `credits_per_percentage_point_construction_outside_boundary.rs` | aub-rif.2 |
| `credits_forbidden_coefficient_combinations.rs` | aub-rif.12 |
| `credits_where_tokens_expected.rs` | aub-rif.12 |
| `cross_type_arithmetic.rs` | aub-rif.12 |
| `quota_used_plus_money.rs` | aub-rif.14 |
| `quota_level_addition.rs` | aub-rif.3 |
| `money_cross_currency.rs` | aub-rif.4 |
| `derivation_unavailable_value.rs` | aub-rif.8 |
| `secret_print.rs` | aub-eun.1 |
| `secret_log_field.rs` | aub-71j.2 |
| `resolved_log_field.rs` | aub-71j.2 |
| `log_field_not_approved.rs` | aub-71j.2 |
| `token_kind_mismatch.rs` | aub-ai3.4 |
| `cost_model_exhaustive_construction.rs` | aub-ai3.2 |
| `prune_target_cannot_name_irreplaceable_class.rs` | aub-sth.15 |
| `rebuild_target_cannot_name_irreplaceable_class.rs` | aub-sth.15 |

## Exclusion log

Beads whose text matched a marker but carry no standing break-proof, with
the reason. A matched bead must have a row above or an entry here; the
consistency check enforces exactly that.

- `aub-lqe.3`: the parser-side mutation criteria run the other way (the
  parser must survive generated mutations, not fail under a removed rule).
  Covered by the parser contract suites, not by a break-proof.
- `aub-k9ip`: behavior specs over real-shape fixtures; the shared mutation
  suite it cites is owned by the corpus audit rows `c-08`/`c-09`.
- `aub-n5wb`: a one-time forced reproduction of a live race, recorded on the
  bead; no standing guard to re-prove.
- `aub-nqer`: the clock-check mutation is timing-sensitive and owned by the
  review on a quiet machine, for the same reason lanes never report
  wall-clock figures.
- `aub-lqe.6`: behavior specs (partial report naming the missing component);
  no break-proof criterion.
- `aub-c6vz`: behavior specs over the limits array; no break-proof criterion.
- `aub-xpfl`: behavior specs (resolution rules, goldens); no break-proof
  criterion.
- `aub-ycyn`: timing medians only; measurement owned by the review.
- `aub-2r0n`, `aub-1r3m`, `aub-6qay`: one-time mutation records in review
  comments, not acceptance criteria; nothing standing to re-run.
- `aub-ju53`: behavior specs for the skip marker; the marker mechanism itself
  is row `c-30`.
- `aub-rif.3`, `aub-rif.4`, `aub-rif.9`: negative-trait cases (guards) whose
  beads state the property without a break-proof; the standing re-proof is
  the harness rows `c-01`/`c-02` via the fixture map above.
- `aub-cab.4`, `aub-eun.15`, `aub-maop`, `aub-sth.3`, `aub-va6s`,
  `aub-71j.3`, `aub-2r3`: refusal and behavior specs; no break-proof
  criterion.
- `aub-omtm`: the restore-must-run-on-partial-failure case is a behavior
  spec over a seeded drill, not a break-proof of a guard.
- `aub-x5bo`: a one-time proof that following the written pane work-cycle
  surfaces planted lint and format violations; the subset commands it
  exercises are covered by the gate-coverage neutering row `sh-24`.
