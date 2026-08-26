# AGENTS.md - agent-usage-book

> Guidelines for AI coding agents working in this Rust codebase.
>
> **Reread this file after every context compaction, without waiting to be asked**, then
> reread the active bead, its comment thread and the current reservation state before
> resuming. Compaction drops the behavioural contract silently, and an agent that lost it
> keeps working as if it still had it.

---

## RULE 0: A DIRECT INSTRUCTION OVERRIDES THIS FILE

An instruction from the person running the work wins over every rule below. Follow it,
record any lasting technical decision on the bead it belongs to, and continue. This file
is the default, never a veto.

## RULE 1: NO FILE DELETION

Deleting a file needs express permission, including a file you created yourself in this
session. Say exactly what would be removed, and wait for the answer. Under one shared
tree a deletion also destroys work belonging to a pane that is still running, and none of
it is recoverable.

## RULE 2: NO WORKTREES, ONE SHARED TREE

No pane creates a worktree. No pane creates a branch of its own. Every agent works in the
same checkout, on the same branch, and coordinates through the three protections below
rather than through filesystem isolation. The reasoning, and what replaces the isolation,
is in *Coordination under one shared tree*.

`bin/worktree` is tracked here anyway, because the repo harness installs one copy of it in
every repository it manages. It is not for panes. Nothing in this file ever calls it, and a
pane that reaches for it because it is sitting there has broken this rule.

## Irreversible Git and Filesystem Actions - DO NOT EVER BREAK GLASS

`git reset --hard`, `git clean -fd`, `git checkout -- <path>`, `git push --force`,
rewriting a commit that is already pushed, and `rm -rf` are not yours to run. When
something has to be undone, restate the exact command, name every path it would touch, and
wait for explicit approval. "I think it is safe" is not a reason, and in a shared tree the thing
you would discard is usually somebody else's uncommitted work.

Before assuming a commit was lost, check rather than repair:
`git merge-base --is-ancestor <sha> HEAD`.

## Git Branch: ONLY `main`, NEVER `master`

`main` is the default branch and the only branch this repository has. Panes commit to it
and push, with no branch of their own and no pull request in the path. This is the method's
own position, adopted whole: branch-per-agent is merge hell, and a logical conflict between
two beads surfaces faster when both land on one branch than when they sit in two.

CI runs on every push to `main`, so the gate sits between a commit and the branch anyone
else reads either way.

**An earlier version of this section described a shared feature branch reaching `main` as
one reviewable pull request after the hardening gate.** That branch was never created. Every
commit of the first swarm landed on `main` directly, and the only pull requests this
repository has predate it and carry harness changes made by hand. The text was left here
long enough to be read by panes that would have gone looking for a branch that does not
exist, or created one, which Rule 2 forbids in the line above.

## Toolchain: Rust and Cargo

- **Stack:** Rust, edition 2024. No async runtime, by decision.
- **HTTP:** `ureq`, blocking, inside `std::thread::scope`. About three endpoints are
  called, concurrently and once. `reqwest` with the `blocking` feature is not the same
  thing: it starts tokio underneath, which is a runtime this binary has no other use for.
- **Persistence:** one bundled SQLite database in WAL mode on a local filesystem, with
  `synchronous = FULL` for the irreplaceable meter writes. The build is bundled so
  behaviour does not depend on the host's SQLite version. The state directory must be
  local; `doctor` rejects known network-filesystem cases.
- **Build:** `cargo build --release`
- **Run:** `cargo run -- <args>`
- **After clone, once:** `bin/install-hooks`
- **The toolchain is pinned in `rust-toolchain.toml` and must be resolved through rustup.**
  The pin only binds a rustup proxy, so a distribution's `/usr/bin/rustc` ignores it while
  reporting the same version number, and `bin/checks/05-toolchain` fails when that is what
  is on PATH. This is not pedantry: two builds of one release render diagnostics
  differently, and a compile-fail fixture captured against the wrong one made CI red for 38
  consecutive runs while `bin/ci` was green on the machine that produced every commit in
  them. Put `~/.cargo/bin` ahead of the distribution's directories on PATH.

## Compiler Checks (CRITICAL)

```bash
cargo check -p agent-usage-book        # the pane check subset, all three of it
cargo fmt --check
bin/slop-guard
```

`bin/ci` is the full gate: toolchain identity, format (`cargo fmt`), lint (`cargo clippy`),
test, dependency audit, boundary rules, prose guard (`bin/slop-guard`), e2e,
quantity inventory, commit protocol, pane work cycle, batch-verify close, and gate
coverage. It is the exact thing CI runs, so green locally means green in CI, which is
true only because the first check refuses to let the rest run under a different compiler.

**The pane runs a named subset, and the subset is chosen by measurement rather than by
category.** Those three commands are what a pane runs before moving a bead to
`batch_pending`. Measured on this machine, warm:

| command | cost | compiles? |
| --- | --- | --- |
| `cargo check -p agent-usage-book` | 2356 ms | yes |
| `cargo fmt --check` | 100 ms | no |
| `bin/slop-guard` | 831 ms | no |
| `cargo clippy --all-targets -- -D warnings` | 4780 ms | yes |
| the whole of `bin/ci` | 19 s | yes, repeatedly |

**Formatting and prose are in the subset because they are nearly free.** They add 931 ms to
a park that already costs 2356 ms, and neither compiles anything, so neither contends for
the build backend that the wave model exists to protect. An earlier revision of this section
put both with the orchestrator on a cost argument, citing the 19 seconds of the full gate.
That number is real and it is the wrong number: the 19 seconds are test, e2e, audit and the
conformance checks, none of which anyone proposed giving to a pane.

**Clippy is not in the subset, and that is a deliberate hole with an owner.** It compiles,
which is the one property the wave model cannot afford per pane, and it is the most expensive
single check a pane could run. Lint failures therefore belong to the orchestrator, who
repairs them centrally in Phase 2.

**A bare `cargo test` or `cargo run` reads `build.target-dir` and is not covered by the
isolation `bin/ci` sets up.** A binary out of `target/` can be another pane's build. Pass
`CARGO_TARGET_DIR` yourself before trusting anything built outside `bin/ci`.

**Beyond that subset, no pane builds, and it is enforced.** See *Swarm operations*: the
orchestrator kills per-agent test and full-build processes every tick, because N agents
building the same crate is the bottleneck the wave model exists to remove.

## Testing

Code and its tests ship in the same bead. A test-only follow-up bead exists only for a
cross-cutting integration suite, never as a way to close an implementation bead early.

Every bead pre-specifies its key behavioural assertions, including at least one negative
that a naive wrong implementation would fail. A planted negative is near-identical to its
positive and differs only in the forbidden dimension. A test that asserts the code does
whatever the code does is not a test.

A compile-fail capture is a property of the crate's whole trait graph, not of its
fixture: a bead that adds an impl anywhere can add a `help:` block to another bead's
capture, with no dependency between the two. Regenerate captures with
`cargo run --bin compile_fail_regenerate`, never with a bare `TRYBUILD=overwrite`. The
guard refuses when the error code changed, naming both codes, because a changed code
means the fixture fails for a different reason and blessing it destroys the test; it
proceeds when the code is unchanged. `--override` is the explicit override.

## agent-usage-book - This Project

### What it does

One ledger for LLM consumption: token spend read from local agent transcripts, joined with
quota measured at the provider's own endpoints, so routing a model is decided from one
number instead of five tools that disagree.

**Calibration:** Tier 2, phase work. External stakes are contained (a local binary, no
server, no user data). Personal stakes are high: this is the measurement layer that feeds
model-routing decisions, so a wrong number here is acted on elsewhere.

### Scope (current)

Read token spend from local agent transcripts, measure quota windows at provider
endpoints, and report both from one command. Do not expand beyond it without a present
need. If a change drifts past it, stop and flag it.

### Correctness invariants

The dominant failure of this project is not being unavailable, it is **a wrong number
reported as if it were right**: a stale reading rendered as fresh, or one unit printed in
the place of another. Availability is cheap to notice and a wrong number is not, so these
five rules outrank convenience everywhere they meet it.

1. **Quantities are newtypes, and the arithmetic between them is closed.** `Percent`,
   `Credits`, `Tokens` and `Cost` are distinct types. No cross-type operator, no `From`
   between them, no conversion that is not a named function stating what it assumes. A
   bare `f64` or `u64` crossing a module boundary is a defect, not a shortcut.
2. **Every reading carries its freshness in an exhaustive enum**, `fresh | stale |
   auth_required`. Not `Option`, which erases which of the three happened, and not a
   boolean, which cannot say that credentials expired. A caller that renders a reading
   matches all three arms.
3. **A constant is defined once and read, never copied.** A credit ceiling, a window
   length, a price: one definition. Hand-copying one between two tools is the defect that
   caused this project to exist, and it is silent by construction because both copies keep
   working until one is edited.
4. **Nothing identifying a machine, an account or a person is compiled in.** Transcript
   paths, account names and the state directory are configuration, and no default names a
   person or spells an absolute home directory.
5. **A number the code cannot justify is not printed.** Where a source is unreachable, the
   output says which one and why. It never falls back to the last value silently, and it
   never substitutes zero, because a zero is indistinguishable from a real measurement of
   nothing.

## How This Project Is Built

> **Read this, do not run it.** Deciding which stage the work is in, and whether a gate has
> been passed, belongs to whoever is running the swarm. A pane needs the shape to understand
> why its bead is written the way it is, and never acts on this section: its own instructions
> are *Picking up work* and Phase 1.

Development follows a ladder of representations. Each rung turns the work into a form the
next one can act on, and each has a gate. **A failed gate sends the work back a phase; it
never sends it forward with a note attached.**

| stage | in to out | what decides the gate here |
| --- | --- | --- |
| Intent shaping | goal to foundation bundle | `bin/ci` green and this file coherent |
| Planning | foundation to a markdown plan | no command exists. The exit signal is convergence: a refinement round that stops changing much, not an answer for every open question |
| Translation | plan to bead graph | no command exists. The audit is bidirectional and done by hand: every material plan element reaches a bead, and every bead traces back to the plan |
| Execution prep | raw beads to launch-ready beads | `br lint` clean, `br dep cycles` empty, and every bead self-contained about its test obligations |
| Swarm implementation | beads to code | `ntm health <session>` exits zero |
| Hardening | implementation to tests and scans | `bin/ci` green, and the work that is left captured as beads rather than as prose |
| Memory | real usage back into the tooling | no gate. It is the stage that makes the next project start better |

Three of those gates have no command, and saying so beats a table that implies coverage.
They are judgment, and the drop-back rule is the only thing holding them.

Most of the effort belongs in planning, because reasoning is cheapest while the whole
problem still fits in one context. The method's own illustrative figure is that rework
costs one unit in plan space, five in bead space and twenty five in code space.

### Which stage this project is in

**Execution prep.** The plan converged and lives at `docs/PLAN.md`; translation produced
the bead graph; what remains before the first wave is the launch readiness of the beads
and of the coordination layer.

**Do not read that sentence as current.** A stage written by hand goes stale the moment
the work moves, and this line said "Planning" for days after 187 beads existed. Derive it
instead, and correct the line when the two disagree:

```bash
br stats                  # a bead graph exists at all, and how much of it is closed
br lint && br dep cycles  # the Execution-prep gate, in two commands
br ready --unassigned     # what a wave would actually have to work on
ntm health <session>      # a swarm is up and healthy, which means Swarm implementation
```

**The gate that ends Execution prep here** is `br lint` clean, `br dep cycles` empty, every
executable leaf carrying frozen acceptance criteria, and the three protections below
verifiable by an agent that just booted. Until all four hold, no wave launches.

### When something breaks, go back to the cheapest representation that can fix it

| what broke | go back to |
| --- | --- |
| panes stepping on each other, or losing operational context | code space: stagger starts, force an explicit reservation, check claims |
| a vague bead that let agents improvise into inconsistent implementations | bead space: rewrite the bead |
| missing dependencies, or contradictory implementations from overlapping scope | bead space: fix the edges, add the missing bead, or revise bead boundaries |
| a plan-level gap surfacing mid-swarm | plan space: update `docs/PLAN.md`, then generate the beads it implies |
| an agent going in circles, usually after compaction dropped this file | force a reread of this file; restart the pane if it stays erratic |
| busy agents, many commits, and the real gap to the goal not closing | stop. Ask whether the open beads actually close the gap, then revise the graph |

**Heavy cognitive work during implementation is not a hard problem to push through.** It
is a symptom that bead polishing was insufficient. Pause and go back to bead space.

## Swarm Operations: Code-First Waves and Batch Verify

N agents sharing one repository and one build backend are bottlenecked on builds, not on
coding. Writing code is cheap and parallel; building and testing is expensive and
serialized. So they are separated: all panes write real code at full speed without
building, and the build runs once, centrally, over everyone's combined changes.

**Of everything under this heading, Phase 1 is the only part a pane performs.** The rest
belongs to whoever is running the swarm: when the wave flips, what gets verified, what gets
closed, what is killed, and which representation broken work drops back to. A pane reads
those to know what will happen to its bead and what its build may not do; it does not act on
them. This division is unusual and it is load-bearing here, because one crate built by N
panes at once is the bottleneck the whole model exists to remove.

### Phase 1: code-first wave (every pane, in parallel)

1. Claim the highest-priority ready bead. `br ready --unassigned`, then
   `br update <id> --claim --actor <your Agent Mail name>`.
2. Reserve the exact edit surface before opening a file.
3. Write the **real code and its real tests** in that same bead. No placeholders.
   `todo!()` and `unimplemented!()` are banned in committed code.
4. Run the pane check subset, which is these three and nothing else:

   ```bash
   cargo check -p agent-usage-book
   cargo fmt --check
   bin/slop-guard
   ```

   No `cargo test`, no full build, no waiting on proof. Costs and the reason the subset is
   these three are in *Compiler Checks*.

   **One class of gate failure is invisible to you, and it is lint.** `cargo clippy` is not
   in your subset because it compiles, and running it per pane is the bottleneck this whole
   model exists to remove. So following this work-cycle to the letter will not surface a
   clippy failure before `batch_pending`, and you are not held responsible for one. The
   orchestrator catches and repairs lint in Phase 2, and no rework note ever returns a bead
   for it.
5. Commit, naming the bead id and the touched scope. The form to use, and the test to run
   before committing a shared file, are in *Committing into a shared index*.

   **Check central verification before committing:** `bin/last-verify`. It reports in a
   single line whether the last central verification was green and at which commit (`green
   at HEAD`, `stale: green at <sha> (distance N)`, or `none: no central verification
   verdict recorded`). It is machine-local (`.beads/last-verify`) and does not reach CI or
   another clone. The three check commands above tell you about your own tree, while
   `bin/last-verify` tells you whether `main` was clean when central verification last ran
   and how many commits have landed since. Do not read a green subset as a clean `main` if
   `bin/last-verify` reports a stale verdict or no verdict. `br list --status rework` does
   not answer this either, because a bead only reaches `rework` after a verify has already
   run, which is the delay itself rather than a warning about it. Commit anyway: stacking
   onto a tree that is red from lint costs the orchestrator one central repair, and stopping
   the wave to guess costs more.
6. Move the bead to `batch_pending` **only when substantively complete**: code and tests
   written, commit linked, reserved paths respected, every acceptance checkbox mapped to a
   concrete test, no known defect. `batch_pending` earns no capability credit; it frees
   claim capacity.

   **Then read the status back.** `br update --status` takes free text and validates
   nothing: `--status batch_pendign` is accepted, prints a normal transition line, and
   leaves the bead in a state no query names. It is gone from `in_progress`, so nobody
   thinks it is being worked, and absent from `batch_pending`, so the verify pass never
   collects it. `br show <id>` is the whole check, and it costs one call.
7. Release the reservation and take the next bead.

Commit rate during a wave is a saturation signal for the orchestrator, never a per-agent
score. The moment agents are scored on commits you get commit pumping.

### Phase 2: batch verify and close (the orchestrator, once per wave)

> **Not yours if you are a pane.** Every step below is run once per wave by whoever is
> running the swarm, and step 6 closes beads, which *Honest Work and Anti-Ceremony* forbids
> anyone else to do. A pane that works through this list because it read the file top to
> bottom would close its own work, and that close is reverted with an incident comment. Your
> wave ends at Phase 1 step 7, with the bead in `batch_pending` and the reservation
> released. Read Phase 2 to know what will be done to your bead and what evidence it will be
> judged on; do not perform it.

1. Flush the swarm's commits so the tree is consistent, and record the clean HEAD.
2. Run **one** build and test pass over the union of touched scope, on a dedicated target
   directory. Touched scope is derived from the `wave_base..verified_head` diff plus its
   reverse dependents, never from what an agent declared.
3. **Fix compile errors first.** One test-target compile error makes `cargo test` abort
   early and report a misleadingly green prefix. Only a fully compiling run yields a true
   pass and fail count.
4. Cluster remaining failures by file and return each failing bead to `rework` for the
   same assignee, with the exact assertion and location. The verifier triages; it does not
   silently finish the work. **Lint failures are exempt:** `cargo clippy` is the one gate
   class outside the pane check subset, because it compiles, so the orchestrator repairs a
   lint defect directly during batch verify rather than returning the bead. No rework note
   ever returns a bead for lint. Formatting and prose are not exempt and are not repaired
   centrally: they are in the pane subset, so a failure in either means the work-cycle was
   not followed, and that bead goes back to `rework` like any other.

   A compile-fail failure on a bead whose diff does not touch the fixture is a trait-graph
   consequence, not that bead's rework: another bead's impl changed the compiler's output
   under the same error code. Regenerate the capture with `cargo run --bin
   compile_fail_regenerate`, which refuses if the code changed, and do not return the bead
   to rework.
5. Re-run until green. Every attempt is retained in the wave record; rerun-until-green is
   not proof that a failure was flaky.

   **Record the central verification verdict when green:**

   ```bash
   bin/last-verify --record
   ```

   The verdict is recorded at step 5 after a green verification run and written to
   `.beads/last-verify`. It is machine-local and does not reach CI or another clone, giving
   panes a zero-compilation way to query central verification freshness.
6. Close only green `batch_pending` beads, citing the verification run:

   ```bash
   br close <id> --reason "<evidence>" --transition-comment "<batch summary>"
   ```

   **Batch-verify evidence lives directly in `--reason` and `--transition-comment`.**
   The close reason captures the revision-bound proof (`commit:<sha> suites:<...>`),
   and `--transition-comment` records the batch summary atomically on the bead.
   Evidence is retrieved with `br show <id>` (or `br show --json <id>`, `br list --status closed`),
   and an auditor greps for `close_reason` or the commit SHA.

   **Why structured gate reporting (`br gate report`) was dropped instead of configured:**
   Enforcing a `batch_verify` gate through `.beads/policy.yaml` would change close semantics
   on a live tracker during an active unattended swarm run. Furthermore, external automated
   dispatchers (`bin/swarm-dispatch`) close beads without writing gate records today.
   Recording verification evidence directly in `--reason` and `--transition-comment` ensures every
   close carries durable, queryable evidence on the bead itself without risking tracker refusal
   under in-flight runs.

7. **Sequence the next wave by blast radius, not only by the dependency graph.** Closing a
   layer refills the ready pool, and what leaves it is the orchestrator's choice. The
   tracker models dependencies between beads and does not model file overlap, so two beads
   with no edge between them can still be dispatched onto one file. Before releasing a
   wave, read the blast radius of every bead in it and hold back the ones that collide on a
   shared file from the table in *Committing into a shared index*. Overlap that cannot be
   designed out is sequenced, never dispatched in parallel and repaired afterwards.

The verification record is revision-bound: HEAD, toolchain and lockfile identity, the
exact commands, the selected tests, and the exact bead list. Any movement of HEAD
invalidates it. Verification evidence recorded against an earlier revision does not satisfy a
later close after rework.

**A green union suite must map every closing bead to the exact tests that exercised its
behaviour.** Never close a wave off one broad green command.

### When Phase 1 flips to Phase 2

On the earliest of: the ready pool draining; the verification-debt ceiling being hit; an
articulation-point bead becoming verifiable while its dependents starve; the touched scope
growing past what is cheap to verify; an elapsed-time or risk bound. A dip in commit rate
is one signal among several and never the only one, because with a large graph the ready
pool may never drain.

### Why closing is what refills the work

A tracker unblocks a dependent only when its blocker is **closed**, not when it is
committed and pending. So the ready pool drains during Phase 1 and refills in a burst at
the Phase 2 close step. The loop is a pump: each verify pass closes a layer, which unblocks
the next layer, which feeds the next wave. Periodic cycles keep the swarm fed; one giant
pass at the end would starve it.

### Enforcement

Agents want to build, to prove their work. The model is enforced, not requested.

- The orchestrator kills per-agent test and full-build processes every tick, scoped by
  owned target directory. `cargo check` is exempt, as is the orchestrator's own verify
  target directory.
- Only the orchestrator closes a bead. A close by anyone else is reopened with an incident
  comment on the record.
- Genuinely incomplete work stays `in_progress` or `rework` with a comment. Never false
  closed, never parked as "ready for validation" and forgotten.

## Coordination Under One Shared Tree

Under one shared tree two panes editing one file clobber each other and nothing reports
it, so the three protections below are conditions of implementing at all. **A pane that
cannot verify all three does not edit: it names the one that is down and stops.**

| protection | what it covers | how a pane gets it |
| --- | --- | --- |
| bead claim | who owns a unit of work | `br update <id> --claim --actor <name>` |
| file reservation | who owns an edit surface | `file_reservation_paths` through Agent Mail, before the first edit |
| reservation guard | a commit touching a path somebody else holds | the `pre-commit` hook refuses it |

**The claim is not a lock.** `br update --claim` refuses only when a *different* actor
holds the bead, and the default actor is the shell user, so two panes claiming under one
identity both succeed with no error anywhere. Pass `--actor` with the pane's own name,
every time. What actually prevents a second dispatch onto the same work is that a claimed
bead leaves `br ready --unassigned`.

**The reservation is advisory and expires on purpose.** A crashed pane must not be able to
deadlock the tree, so a lease has a TTL and a dead agent's hold is reclaimed when it
lapses. Reserve the narrowest correct path set, with a TTL sized to the edit rather than
to the day, and put the bead id in the reason. A reservation call can return a lease and
still report conflicts: read the conflict list, because a path somebody else holds is not
yours just because the API answered. When a path is held, do not wait and do not escalate.
Take a bead with a different edit surface.

**Mail is an outbox, not a mailbox.** Panes announce and do not reliably read, so anything
that has to reach a working agent goes into its pane, and anything that has to survive
goes on the bead rather than into a message.

**No pane has a role.** Agents are interchangeable, and everything one needs to continue
lives in the bead graph, the reservations and this file, never in another agent's context.
An agent that stops is replaced, not recovered.

### Committing into a shared index

The reservation layer guards a file. It says nothing about the index, and one shared tree
has exactly one index, so anything a pane stages is visible to every other pane's commit.
This is the commit form, and it is not optional:

```bash
git add <your paths>
git commit -m "<subject>" -m "<bead id>" -- <your paths>
```

Naming the paths commits the working-tree version of exactly those paths and leaves every
other pane's staged work untouched. The `git add` is neither redundant nor optional:
`git commit -- <path>` refuses a path git does not already track, so the new file a bead
just wrote has to be staged before it can be named. Staging your own paths disturbs nobody,
since the index holds everyone's and the commit that names yours leaves theirs staged.

`git commit -a` sweeps every pane's uncommitted edits into your commit, and a bare
`git commit` sweeps whatever they have staged. Both are banned, and banning them is useless
without the form above: a rule shaped *never do X* leaves an agent with no move at all when
X is the only form it knows.

**A foreign staged path is not a reason to stop.** Six panes once deadlocked for thirty
minutes on exactly that, each correctly refusing to run a form it had been told never to
run, and none of them holding a third. Commit your own paths and carry on.

**Before committing a shared file, check that its diff is yours.**

```bash
git diff HEAD -- <file>
```

The path-scoped commit protects you per path, and a shared file is one path with two
authors, so the command that keeps another pane's *files* out of your commit will carry
another pane's *lines* in. Read that diff before you commit the file. A line you did not
write usually depends on a file that does not exist at this revision yet, and the gate then
goes red on a bead that did nothing wrong: `pub mod window;` committed without
`src/domain/window.rs` fails lint, test and rustfmt at once.

The test is about the diff, never about the file. `src/domain/mod.rs` is never entirely one
bead's work, because every bead in the layer appends a line to it. Once the other panes
commit their lines, those lines leave your diff, and the file that was unsafe five minutes
ago is safe now.

**When the diff is not entirely yours**, commit your other paths, record on the bead which
file is blocked and whose line is sitting in it, and take the next thing. Do not stand down
holding everything, and do not wait on the file.

**The shared files in this repository** are the ones that attract simultaneous editors:

| file | why more than one bead touches it |
| --- | --- |
| `src/lib.rs` | the crate root, one `pub mod` line per module |
| `src/config/mod.rs`, `src/domain/mod.rs`, `src/presentation/mod.rs`, `src/store/mod.rs` | the module declaration surface, one `pub mod` line per submodule |
| `src/error.rs`, `src/problem_code.rs` | registries, one variant per failure a bead introduces |
| `src/cli.rs` | the command enumeration and `Command::ALL` |
| `Cargo.toml` | one dependency line per bead that needs one |

That list is not closed. **A file is shared when a bead's normal work appends to it rather
than rewriting it**: a declaration list, an enumeration, a registry, a manifest. Finding one
that is not listed above is itself part of the work, so add it here in the commit that
found it.

### Setup, once per machine

None of the three protections is self-installing, and two of them fail silently when they
are not set up. Verify, do not assume.

1. **The mail server must be reachable from this repository.** `.mcp.json` at the root
   declares it, and the bearer token comes from the environment rather than from the file,
   because this repository is public:

   ```bash
   export AGENT_MAIL_TOKEN=<token>   # before launching any pane
   ```

   A pane with no `mcp-agent-mail` tools has no reservation layer. That is protection two
   down, and Rule 2's trade is off.

2. **Install the reservation guard into this checkout, once:**

   ```bash
   cd ~/.local/share/mcp_agent_mail
   GIT_IDENTITY_ENABLED=1 .venv/bin/python3 -m mcp_agent_mail.cli \
     guard install <project-key> /path/to/agent-usage-book
   ```

   It writes a generic chain runner to `.githooks/pre-commit`, preserves whatever hook was
   there as `.githooks/pre-commit.orig` (which it runs last, so the secret scan survives),
   and drops the project's guard plugin into
   `.githooks/hooks.d/pre-commit/50-agent-mail.py`. The plugin carries absolute paths for
   one machine and is therefore git-ignored: it is regenerated per machine by this command,
   never committed.

3. **Every pane exports its own identity before its first commit:**

   ```bash
   export GIT_IDENTITY_ENABLED=1
   export AGENT_NAME=<the name Agent Mail assigned you>
   ```

4. **Canary it before the wave, because a hook is a claim until it refuses something.**
   Reserve a path as one agent, stage that path, and attempt a commit as another. A guard
   that does not refuse is not installed.

### Picking up work

1. Confirm the working tree and the branch. Do not create or switch to another.
2. Register with Agent Mail and use the name it assigns as the actor everywhere below.
   The name is disposable by design; no agent's identity is load-bearing.
3. Export `GIT_IDENTITY_ENABLED=1` and `AGENT_NAME`, and confirm the guard refuses a
   foreign reserved path before trusting it.
4. `br ready --unassigned`, and take one bead. A bead already in progress belongs to
   somebody else: pick another. The only exception is a brief that says the bead was
   already claimed for you, and only because it says so.
5. Claim it before opening a single file: `br update <id> --claim --actor <name>`.
6. Reserve the exact edit surface.
7. Implement, with the tests the bead asks for. One bead at a time.
8. Run the pane check subset from *Compiler Checks*: `cargo check -p agent-usage-book`,
   `cargo fmt --check`, `bin/slop-guard`. Then commit and push, with the path-scoped form in
   *Committing into a shared index*. Work that is not pushed does not exist to the other
   panes.
9. Release the reservation, record the outcome on the bead, and move it to
   `batch_pending`. Do not close it.

Do not open a work cycle by broadcasting status. Coordination is what makes implementation
safe; it is not a substitute for implementing.

### One identifier, carried everywhere

The bead id is the only correlation key this project has, so it travels into every
artifact the work touches. Without it a reservation, a thread and a commit are three
unrelated facts about the same change.

| where | form |
| --- | --- |
| mail thread | the bead id, bare |
| message subject | `[<id>] <what changed>` |
| reservation reason | the bead id |
| commit message | the bead id in the body |

`.beads` is git-ignored here, so the tracker is machine state rather than repository
history and it does not travel with a clone. That is a deliberate departure from the usual
convention around this tracker, and it is why the id in the commit message is the only
durable link from a line of code back to the reason it exists. `br` never runs a git
command by itself, so nothing it writes reaches the history unless somebody commits it.

### Degraded coordination when Agent Mail is unavailable

Do not block on it, and do not pretend the protection is still there. Fall back to bead
assignee locking, and make the weaker state visible before touching code: claim with
`--actor`, log the intended file scope as a comment on the bead, and say in the comment
that the reservation layer was down. Treat the result as what it is, which is not a lock.

## Changes You Did Not Make Are Normal

`git status` in a shared tree shows other panes' work, and it changes while you are
reading it. That is the expected state of this repository during a swarm, not an anomaly
worth reporting.

- **Never stash, revert, overwrite or otherwise disturb a change you did not make.** Treat
  a modified file you do not recognise exactly as you would treat one of your own.
- **Do not stop to ask what an unexplained edit is.** The answer is always the same, and
  the question costs one pane a work cycle to ask and another one to answer.
- **Stage your own paths by name.** `git add -A` in a shared tree captures whatever is
  mid-edit elsewhere, and a commit holding half of somebody else's change is worse than no
  commit at all.
- **A build that breaks on a file you never touched means somebody is inside it right
  now.** Take a bead with a different edit surface, or wait. Do not fix it out from under
  them: your fix and their next write cannot both survive.

## Honest Work and Anti-Ceremony (binding for agents and humans alike)

The purpose of this swarm is working software delivered accretively in the shortest time
compatible with correctness. Process exists to serve that outcome and must never become
the product.

**A process artifact may exist only when it is a hard gate for a named capability.** At
creation it names a concrete consumer, the gate it enforces, the observed defect class
that justifies it, and its retirement condition. The boundary test: if running code
branches on it, it is product; if only humans and status reports read it, it is ceremony.
Process work earns zero capability credit regardless of quality. A process artifact that
gates nothing does not get created.

**Honesty is absolute.** Never fake a test, present a fixture or a retained capture as
live proof, weaken an assertion to make it pass, hard-code a success path, regenerate a
golden to match broken output, or close work that is not done. A false close is reopened
with an incident comment on the record, because silent reopening teaches nothing.

**No self-certification.** Work is closed by the batch-verify orchestrator citing evidence
bound to an exact revision. Peers never close each other's beads: closure is what unblocks
dependents, so unilateral closing prints currency.

**A typed refusal beats a fabricated result and is worth less than the real capability.**
Implementing only the guard or refusal path never closes a positive-capability bead; label
it and leave it open. The only exception is a bead whose contract *is* the refusal
boundary, and even then it pairs every forbidden case with a near-identical permitted
positive that proceeds.

### Named patterns, so they can be called by name

An agent that has read "commit pumping is forbidden and treated as reward hacking" behaves
differently from one that has not. Cite these ids in beads, dispatches and incident
comments.

| id | the exploit | the countermeasure |
| --- | --- | --- |
| RH-1 | gate self-weakening: editing a validator or test gate so a failing check passes | gate code is reviewed as its own change, never bundled as an incidental fix |
| RH-2 | proof-class inflation: a fixture, capture or mock presented as live proof | keep the hierarchy static, unit, capture, live, and let no lower class stand in for a higher one |
| RH-3 | golden regeneration reflex: regenerating goldens instead of fixing the output | a golden change is its own marked commit with a semantic diff |
| RH-4 | commit pumping: trivial or artificially split commits, or placeholder scaffolds that pass the syntax gate | placeholder macros are banned in committed code; commit rate is a saturation signal, never a score |
| RH-5 | tautological tests: asserting the code does whatever the code does | every bead pre-specifies a planted negative a naive wrong implementation would fail |
| RH-6 | easy-bead cherry-picking: claiming low-risk leaves while articulation points starve | claim the highest-priority ready bead; the orchestrator assigns critical-path work explicitly |
| RH-7 | close pumping: closing beads to flood the ready pool | only the orchestrator closes; violations reopened with an incident comment |
| RH-8 | scope splitting: types, then impl, then tests, as three closures | code and its tests ship in one bead |
| RH-9 | follow-up laundering: moving an unmet acceptance condition into a new bead and closing the original | if it was in scope, the original stays open or is blocked by the follow-up |
| RH-10 | spec editing as progress: weakening the plan instead of implementing it | plan edits never close an implementation bead |
| RH-11 | dependency smuggling: vendoring or shimming around a banned dependency | the verify pass enforces the deny list mechanically |
| RH-12 | demo-path hardcoding: special-casing the fixtures so the happy path passes | test subjects are selected at runtime and differ from development fixtures; sniffing for a test or CI environment is forbidden outright |

### Measurement integrity

This project's product **is** measurement, so a metric it reports about itself is held to
the standard it holds its own output to.

- **Every claimed metric predeclares its denominator and a countermetric.** No denominator
  is edited after the result is known.
- **A numeric quota is itself gameable.** "Under five percent process beads" invites
  relabelling a validator as a feature. Use the consumer, gate, defect and retirement test
  above instead, and report the process share as a diagnostic for the operator rather than
  as a target for the swarm.
- **Retries and correlated runs are not independent evidence.** Carry a cluster id and
  count clusters, not attempts.
- **Agreement between agents raises confidence, never authority.** Three panes repeating
  one upstream claim is one datum.

## Code Editing Discipline

- **Change code by hand, not by script.** A regex sweep over source is brittle in a way
  that surfaces later, somewhere else, as a bug nobody connects back to the sweep. Many
  similar edits are many edits.
- **Revise files in place.** No `parser_v2.rs`, no `client_improved.rs`, no
  `report_new.rs`. A new file is for functionality that genuinely fits in no existing one,
  and that bar is high. A swarm pulls harder against this rule than a lone agent does: a
  pane that finds its file reserved is tempted to write a copy beside it, and that is how
  one capability ends up with two implementations and no owner.
- **Pre-1.0, no compatibility shims.** No wrapper kept for an API nobody calls, no
  deprecated path living beside its replacement. Fix the call sites and remove the old
  shape.
- **Fix what you find.** A defect you trip over while working a bead gets diagnosed and
  fixed or written down as a bead. "It was already broken" is not a disposition.

## The Tools This Coordination Runs On

Named here because the rules above are useless to a reader who cannot find them.

| tool | what it provides | where |
| --- | --- | --- |
| `br` | the bead graph: ready work, dependencies, claims, gates, comments | <https://github.com/Dicklesworthstone/beads_rust> |
| MCP Agent Mail | agent identity, threads, advisory file reservations, and the pre-commit reservation guard | <https://github.com/Dicklesworthstone/mcp_agent_mail> |
| `ntm` | the tmux swarm: spawning panes, supervising them, `ntm health` | <https://github.com/Dicklesworthstone/ntm> |

## Common Hurdles

| hurdle | class | gate |
|---|---|---|
| A bare `cargo test` or `cargo run` reads `build.target-dir` and is not covered by the isolation `bin/ci` sets up, so a binary out of `target/` can be another pane's build | tripwire | pass `CARGO_TARGET_DIR` yourself before trusting anything built outside `bin/ci` |
| The reservation guard exits 0 and checks nothing when neither `GIT_IDENTITY_ENABLED` nor `WORKTREES_ENABLED` is set in the committing shell. An unset variable looks exactly like a clean commit | tripwire | export it per pane, and canary the guard by staging a path somebody else holds |
| A compile-fail capture breaks on a fixture nobody edited, because another bead's impl added a `help:` block to the error | tripwire | regenerate with `cargo run --bin compile_fail_regenerate`; a changed error code means the fixture fails for a different reason and the guard refuses |
| `guard install` returns an empty hook path and installs nothing when the same gate is unset, printing an informational line rather than an error | tripwire | set `GIT_IDENTITY_ENABLED=1` on the install command itself, then confirm `.githooks/hooks.d/pre-commit/` is non-empty |
| The mail server's database path is resolved relative to the current directory, so running its CLI from a repository creates an empty `storage.sqlite3` there. An agent that queries it sees zero reservations and concludes every path is free, which is the worst failure an advisory system has | tripwire | run that CLI only from its own install directory; before trusting an empty conflict list, confirm which store was read |
| The guard plugin under `.githooks/hooks.d/` carries absolute paths for one machine | prose | it is git-ignored and regenerated per machine; never commit it |
| `br update --claim` guards on identity, not on a lock. Under the default actor two panes claim the same bead and neither is told | tripwire | always pass `--actor` with the pane's own name |
| A dispatch that dies before its agent starts leaves a bead held by nobody, and nothing expires it | prose | return it with `br update <id> --status open` and say what stopped it |
| `br init` creates `.beads` mode 0755, and the version probe then parses the resulting warning as the version | tripwire | `chmod 700 .beads` immediately after any `br init` |
| The prose gate refuses the em-dash in every tracked file, in the commit message and in the PR body. The commit message is the only one of the three nothing reads a second time | ci | `bin/ci` calls `bin/slop-guard`; the `commit-msg` hook covers the message |
| A clone has no hooks until `bin/install-hooks` runs, so the secret scan is off on a fresh checkout | prose | run it once after cloning |
| An aggregated test command that aborts early on a compile error reports a misleadingly green prefix as if it were the total | tripwire | fix compile errors first; only a fully compiling run yields a true count |
| A rate-limit message persists in a pane buffer after the limit has lifted, and the CLI does not retry by itself | prose | nudge the pane and confirm before idling it |

**A hurdle promoted to a gate is deleted from this table, not duplicated.** The gate is the
instruction; a line here restating it only dilutes the ones still unguarded.

## Landing the Plane (Session Completion)

A session is not finished until the work is visible to everyone else.

1. File the remaining work as beads rather than as prose in a message.
2. Release every reservation you still hold.
3. Run the gate your phase allows. **During a wave that is `cargo check` and nothing more**,
   because the orchestrator kills per-agent builds every tick and a `bin/ci` started here
   dies mid-run; proof is the batch-verify pass's job, and a bead reaching `batch_pending`
   is what asks for it. `bin/ci` is the right closing gate only outside a wave, when no
   orchestrator is reaping and the tree is yours.
4. Commit with the path-scoped form from *Committing into a shared index*, then push.

   **If the push is rejected because the branch moved, merge. Never rebase.**

   ```bash
   git fetch origin && git merge origin/main
   ```

   `git rebase` and `git pull --rebase` refuse outright while any file is unstaged, even a
   file the incoming commits never touched, and in a shared tree some pane is always
   mid-edit: *"cannot rebase: You have unstaged changes."* The two exits git then offers are
   the two this file forbids, since committing sweeps another pane's work and stashing
   disturbs it. `git merge` takes the incoming commits and leaves every foreign edit exactly
   where it was. When the incoming commits touch the very file another pane has open, the
   merge aborts without changing anything, which is the moment to take a bead with a
   different edit surface rather than to force it.

   Rewriting a commit that is already pushed is not the normal path, and a force push never
   is.
5. Record the outcome on each bead you touched. A bead left `in_progress` with no comment
   is indistinguishable from an abandoned one.
