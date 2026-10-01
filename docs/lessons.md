# Lessons from working in this repository

Short entries landed from operator memories in the 2026-09 split. Each one states
the fact, why it matters, and how to apply it. Read this file before starting a
task here; every entry below already cost someone a red gate or a lost hour.

## One global JSON schema version

Dated 2026-09-07.

`src/presentation/json.rs` holds a single `pub const SCHEMA_VERSION`, the
`schema` field of the `JsonEnvelope` every command shares. There is no
per-command schema version.

Why it matters: a bead scoped to one command's JSON still has a cross-command
tail. Bumping the constant forces updating every fixture that pins the literal:
the `tests/fixtures/presentation/*_v*.json` files, `tests/presentation_json.rs`,
`tests/exit_classes.rs`, the test module inside `json.rs`, and every e2e case
asserting a schema value.

How to apply: grep for the schema literal and for `assert_json_field.*schema`
across `tests/` and `src/` before assuming the change is contained. There is no
golden-regeneration path in this repo; every golden is hand-maintained.

## Migration-body mutations poison the schema template cache

Dated 2026-09-21.

`test_support::migrated_schema` caches the migrated template under the build
target directory, keyed by migration count and the commit the build script last
saw. A mutation that edits a migration body (for example a trigger's `WHEN`
clause) and then runs a test through `store::test_schema::open_migrated` builds
and caches the template from the mutant schema under the current commit's key.
Reverting the file does not rebuild it, so later runs read the mutant template
and a correct test fails for no visible reason.

Why it matters: the key is the commit, not the migration source, and the build
script only reruns when a package file changes. Committing an unchanged tree
does not move the revision either.

How to apply: run migration-body mutations only against tests that migrate
fresh (a new file, like each migration's own round-trip test), or run them
last; after one, make a real source change and commit before trusting any
`open_migrated` test again.

## Local time is a deliberate display-only exception

Dated 2026-09-07.

The binary is UTC-only by rule (`src/domain/time.rs` has no timezone facility).
`src/presentation/local_time.rs` is an intentional carve-out, not a violation:
`aub status` renders reset instants and a header stamp in the operator's zone,
while `--format json` stays UTC. The zone comes from `TZ` or the system zone
through one libc call, mirroring how presentation style owns its syscalls.

Why it matters: a boundary scan or a reviewer seeing timezone code can read it
as a defect and "fix" it away.

How to apply: leave the file alone. Tests that need a byte-stable stamp set
`TZ` to a DST-less zone before rendering. The local clock is re-read on every
call, so do not cache it.

## Case 007-spend goes red on its own near 00:00 UTC

Dated 2026-09-12.

The e2e case `007-spend` derives its expected day range from the shell clock
before invoking the binary. Around 00:00 UTC the two sides of the assertion can
sit on different days, failing the case on a tree that changed nothing in
spend. A rerun minutes later is green.

Why it matters: a single-case e2e red whose assertion is a date reads like a
regression in the command, and the gate lists it next to whatever else failed.

How to apply: when `007-spend` is the only red e2e case and the run started
within minutes of 00:00 UTC, rerun before reading anything into it. Two red
cases still need reading separately; one can hide behind the other.

## The e2e budget is spent by three cases, two of them benchmarks

Dated 2026-09-09.

The e2e gate fails a landing when the suite exceeds `E2E_BUDGET_SECONDS`. Two
of the three slowest cases are benchmarks, and a benchmark measures wall clock
by definition, so suite duration tracks whatever else the machine is doing. The
case count does not move; the variance is load.

Why it matters: raising the budget to get a landing through only moves the
failure to the runner, which runs the default. A landing forced through this
way turns the main branch red there.

How to apply: land on a genuinely idle machine instead. Clear the landing's
own temporary directory first; an accumulated one has cost close to a minute.
Read the gate's failure list before accepting an environment verdict: one name
means the clock, two means a real defect is hiding behind it.

## The e2e runner skips symlinked case directories

Dated 2026-09-21.

`tests/e2e/run.sh` discovers cases with `find ... -maxdepth 1 -type f -name
'*.sh'`. A symlink is not `-type f`, so a hand-built case directory of
symlinks (the obvious way to run a subset) runs zero cases, exits 0, and a
wrapper reading only the exit code prints PASS. The runner's header says "0 of
0 case files", which is the only signal.

Why it matters: a green e2e that ran nothing is indistinguishable from a real
green unless the case count is read.

How to apply: to run a subset, copy the case files into a directory under the
lane's temporary directory and check the header reads "N of N case files" with
N above zero, or count verdicts from the run's `summary.json`.

## The pane work-cycle check clones HEAD, not the working tree

Dated 2026-09-20.

`bin/checks/78-pane-work-cycle` clones the repository into a scratch directory
and asserts the pane subset still passes there. Because it clones the repo, a
file fixed in the working tree but not committed is not in the scratch at all,
and the check stays red with a message that reads as a defect in the document
it names rather than in the uncommitted file.

Why it matters: the reported symptom names the check's own subject, never the
file that caused it, so the obvious next move is to argue with the document.

How to apply: commit before running the gate or any single check. When a
check's message accuses a document, first ask whether the disk differs from
the last commit.

## Ledger corruption playbook

Dated 2026-09-12.

The live ledger corrupted twice in one night (sampling-lease pages, then page 1
overwritten by a leaf page). Silent symptoms came first: sampler ticks logging
due-lookup failures in the journal only, a driver re-reading a stale row after
a "successful" sample, and the doctor check counting disk errors hours early.
Recovery order: stop the writer timers and any driver; rename the database
trio aside, never delete; for a file whose page 1 is intact, recover into a
fresh file and vacuum; for one whose page 1 is gone, take the newest verified
backup and verify its checksum; either way copy into place, set owner-only
permissions, switch journal mode back to WAL, run read-only integrity and
foreign-key checks, run the doctor fix for the projection generation, restart
the timers, and log the incident.

Why it matters: none of the steps was documented and each was found at
midnight, in the middle of a wrong diagnosis caused by a shell alias shadowing
the binary.

How to apply: at the first malformed-database line, stop the writers before
diagnosing. Keep a verified backup daily; the last one here was almost a week
old. A read-only query tool is the safe way to inspect the live ledger.

## Closing a bead is a repository change

Dated 2026-09-04.

`docs/INVARIANTS.md` rows name the open bead owning each unenforced invariant,
and the audit test refuses a row naming a closed bead: an unenforced invariant
has to point at work someone can still pick up. Closing such a bead as
bookkeeping breaks the build on the next compile even though the close itself
touches no file.

Why it matters: a tracker verb passes no gate on its way in, so the red
arrives later and reads as the last landing's defect.

How to apply: before closing a bead, grep the tree for its id and fix what
names it in the same breath. When the invariant is now genuinely enforced,
flip the row to the file and test that enforce it and recount the summary
line. When it is not, leave the bead open. Gate the main branch itself at the
end of a run: every landing gating its own merge does not prove the final
state green, because the last change may not have been a landing at all.

## Compliance audit traps on this repository

Dated 2026-09-27.

Running the compliance-and-completion verification here hits three quiet
traps: the bootstrap script appends a duplicate ignore block to the project's
tracked ignore file because its exact-match test misses the form already
there; the coverage report step fails writing its output because trybuild
scratch crates under the target directory point at a temporary path that is
gone (the profile data survives, so move the scratch dir aside and rerun only
the report); and the polish scaffold reports "nothing to polish" when the
remediation file uses a table form it cannot parse.

Why it matters: each one fails quietly or looks like success: a dirty tree
nobody asked for, a coverage run that "completed", a scaffold that says there
is no work.

How to apply: on the next pass, diff the ignore file right after bootstrap,
rerun only the coverage report after moving the stale scratch dir, and have
the polish agent write its own log instead of relying on the scaffold.

## The local gate and the runner take different tracker branches

Dated 2026-09-06.

`bin/checks/30-test` branches on whether the gitignored tracker file exists.
That file is present on every developer machine and absent on the hosted
runner, so the local gate and the runner exercise different arms, and a green
local run is not evidence about the arm that runs there. Three commits once
reached the main branch red before anyone read the runner's log.

Why it matters: the landing tool reports its own local gate, never the push's
run, so nothing surfaces the difference.

How to apply: after landing to the main branch, check the recent runs on that
branch before calling it green. The rule behind it: a gate may branch on a
path it planted itself, never on one the repository merely happens to have. To
reproduce a runner-only branch locally, move the worktree's own tracker link
aside (restoring it from a trap) rather than touching the real file.

## Landing parallel beads on a main-only repository

Dated 2026-09-03.

Four things cost a red main or a wasted hour when many lanes land without pull
requests: a detached service does not inherit the shell's PATH, so the cargo
bin directory is missing and the toolchain check fails under the wrong
compiler (pass PATH and HOME explicitly); skipping the full gate at landing
must still run a whole-target check and the library tests, because a merge can
break compilation or a sibling's test with both lanes green alone; the
invariants file conflicts on almost every merge and its summary line goes
stale, so the resolver keeps one side and recounts from the rows, and a
landing that closes a bead must flip that bead's rows in the same push;
"keep both sides" on a both-added conflict can cut functions whose closing
lines the merge tool deduplicated into shared context, so rebuild each
function whole from both sides instead of splicing blind.

Why it matters: each failure reads as the lane's defect when it is the
landing's.

How to apply: run landings with an explicit environment, never skip the
compile-plus-library-tests floor, recount the invariants summary on every
merge resolution, and hand multi-file both-added conflicts to a lane with a
merge brief naming both sides.

## A memory ceiling on the landing throttles the gate silently

Dated 2026-09-07.

Detached landings once carried a memory high of 6G while a full gate of this
repository peaks near 9G, so the kernel reclaimed against the whole test stage
and four landings went red on wall-clock budgets with no error naming the
ceiling. Worse, the landing bisects a red batch and reports the first red
prefix, so a timing failure on the whole batch followed by a green first lane
lands that lane and names the second lane as culprit, with its gate never run.
Twice the named lane was innocent.

Why it matters: a red lane verdict with a peak at the ceiling is a throttled
batch, not a defective lane, and re-dispatching the blamed lane wastes the
run.

How to apply: when a batch is red, read the memory-peak line beside the
verdict before believing the isolated lane. A peak at the ceiling next to a
budget failure means relaunch the batch with a higher ceiling, not
re-dispatch the blamed lane. The land wrapper now defaults to 12G with an
environment variable to change it, and prints the peak beside every verdict.

## A live ledger ahead of the installed binary stops sampling

Dated 2026-09-12.

Once anything migrates the live ledger past the schema the installed binary
knows, the sampler timer fails every tick with a refusal ("schema version N is
newer than this binary's M") and no account is sampled until the binary is
reinstalled. The visible symptom is a sampling-cadence FAIL in the doctor
output; the refusal itself sits in the service journal, not on any screen.

Why it matters: the refusal is correct and silent where it counts, so a run
that depends on sampling reads as a null result instead of a broken meter.

How to apply: before a run that depends on sampling, read the doctor output
and the sampler service's recent journal. A cadence FAIL with a
newer-than-binary refusal means reinstall from the main branch. Before
installing a main that adds a migration, run the new binary against a
read-only backup copy of the ledger first.

## Any subcommand migrates the live ledger on open

Dated 2026-09-07.

Every subcommand opens the ledger through the same path, and that path applies
pending migrations first. Running a worktree binary that carries an unlanded
migration against the live state directory, even for a listing, attempts a
schema change on the production database. The attempt here rolled back
cleanly, but the intent was read-only and the effect was not.

Why it matters: a subcommand is not read-only for the schema. A wave whose
beads never touch the schema can share binaries freely; a wave that adds a
migration reverses that rule.

How to apply: read the live ledger with a read-only query tool or the
installed binary, never with a worktree build of a main that has a migration
the installed binary lacks. Before installing a main that adds a migration,
copy the live ledger to a scratch directory and run the new binary against the
copy. A migration's own test must run on a fixture holding every row shape the
live table holds, because the database validates new constraints against
existing rows when the column is added.

## Reserving a parked bead as in-progress hijacks the commit gate

Dated 2026-09-04.

The commit-message hook resolves the actor from the session environment and
refuses any message naming none of the beads in progress for that actor. Under
one shell user, assigning a parked bead to the human operator while it is
still in progress makes the hook believe the operator is working on it, and it
rejects the next commit even one that correctly names a different bead.

Why it matters: in-progress on a bead nobody is working on is false, and the
rejection names the hook's rule rather than the reservation that caused it.

How to apply: reserve with status open plus an assignee, never by leaving it
in progress. Open with an assignee keeps the bead out of the ready pool,
states honestly that no work is underway, and stops the hook attributing it to
whoever commits next. When committing from an orchestration session into a
dispatched worktree, set the agent-name variable to the actor that holds the
bead so the hook resolves the same identity the tracker did.

## A review must run the whole boundary-rules check

Dated 2026-09-07.

`bin/checks/45-boundary-rules` runs all twenty rules in about a second. A lane
reporting two rules green hides the other eighteen: a full landing gate once
went red on rule 10 (a two-variant alternation whose first half ends in `|`
rather than `=>`, which the rule reads as a construction) after the lane's
targeted suites, clippy, format and prose checks were all green in review.

Why it matters: review suites are module-scoped while the cheap whole-tree
checks are not, and a lane names only the rules it thought about.

How to apply: in the review of a lane, run the whole boundary-rules check and
the prose guard in the lane worktree before offering it to the landing. Fix a
rule-10 alternation by splitting it into two arms.

## Substring assertions degrade silently on numeric fields

Dated 2026-09-04.

An e2e assertion containing `"schema=1"` was written when the migration chain
produced schema version 1. It kept passing through version 19, because every
value from 10 to 19 contains that string, and surfaced only at version 20, the
first value in ten that does not. Four beads in flight were blocked by it at
once and it read as their defect.

Why it matters: a substring check over a number is a prefix check, so it
degrades into "the field exists" the moment the value gains a digit, and it
degrades silently: the test stays green, which is the state everyone reads as
verified.

How to apply: when a contains-style assertion pins a number, include the
following delimiter so a longer value cannot satisfy it (`"schema=46
generation="` rather than `"schema=46"`). The same shape applies to any
substring match over a count, a version, or an id: check what the assertion
still matches after the value grows, not only that it passes today.
