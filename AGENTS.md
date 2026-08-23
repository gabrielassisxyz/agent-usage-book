# agent-usage-book: agent briefing

> Read before every interaction. Living spec: short, imperative. On every gotcha or
> decision, append one line here.
>
> **After every context compaction, reread this file without waiting to be asked**, then
> reread the active bead, its comment thread and the current reservation state before
> resuming. Compaction drops the behavioural contract silently, and an agent that lost it
> keeps working as if it still had it.

> **What it is:** one ledger for LLM consumption, joining token spend read from local
> agent transcripts with quota measured at the provider's own endpoints, so routing a
> model is decided from one number instead of five tools that disagree.
> **Calibration:** Tier 2 · Phase: work. External stakes are contained (a local binary,
> no server, no user data), personal stakes are high: this is the measurement layer that
> feeds model-routing decisions, so a wrong number here is acted on elsewhere.
> **Review gate:** standard. One independent opinion over the whole branch diff, once,
> pre-push. No per-commit reviews.

## A direct instruction overrides this file

An instruction from the person running the work wins over every rule below. Follow it,
record any lasting technical decision on the bead it belongs to, and continue. This file
is the default, never a veto.

## Never destroy work you did not create

- **Deleting a file needs express permission**, including a file you created yourself in
  this session. Ask, and wait for the answer.
- `git reset --hard`, `git clean -fd`, `git checkout -- <path>`, `git push --force` and
  `rm -rf` are not yours to run. When something has to be undone, say exactly what would
  be removed and wait. "I think it is safe" is not a reason.
- Under one shared tree these are not merely risky. Every one of them destroys work
  belonging to a pane that is still running, and none of it is recoverable.

## Stack and commands

- **Stack:** Rust, edition 2024. No async runtime, by decision.
- **HTTP:** `ureq`, blocking, inside `std::thread::scope`. About three endpoints are
  called, concurrently and once. `reqwest` with the `blocking` feature is not the same
  thing: it starts tokio underneath, which is a runtime this binary has no other use for.
- **Persistence:** undecided. `rusqlite` with the `bundled` feature is the candidate; the
  decision belongs to the implementation plan and this line gets replaced when it lands.
- **Build:** `cargo build --release`
- **Run:** `cargo run -- <args>`
- **Test:** `cargo test`
- **Gate:** `bin/ci` (format, lint, test, dependency audit, prose guard)
- **After clone, once:** `bin/install-hooks`
- **Working tree:** one, shared. No pane creates a worktree or a branch of its own. See
  *One branch, no worktrees*.

## Scope (current)

- **Current scope:** read token spend from local agent transcripts, measure quota windows
  at provider endpoints, and report both from one command. Don't expand beyond it without
  a present need; if a change drifts past it, STOP and flag it.

## Correctness invariants

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

## How this project is built

Development follows the agent flywheel: a ladder of representations where each rung turns
the work into a form the next one can act on, and each has a gate. **A failed gate sends
the work back a phase; it never sends it forward with a note attached.**

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

**This project is at Planning.** No bead is created before the plan converges: a graph
translated from an unconverged plan inherits every hole in it, and after translation the
holes are invisible, because a hole in a plan is a bead that does not exist.

## One branch, no worktrees

Implementation runs as an `ntm` swarm: several agent panes, one shared working tree, one
shared branch. That branch is a feature branch off `main`, not `main` itself, and it
reaches `main` as a single pull request when the hardening gate passes. Nothing else about
the swarm changes if that boundary moves; what does not move is that no pane creates a
worktree and no pane creates a branch of its own.

**This is a deliberate departure from the baseline in the block below**, which makes a
worktree per session mandatory. That rule protects independent sessions, which cannot see
each other and therefore cannot coordinate. A swarm is the opposite case: the panes are
launched together and share a tracker and a reservation service, and worktree isolation
would remove the thing the swarm exists for, which is several agents converging on one
tree.

**The trade is explicit: the isolation is replaced, not dropped.** Under one shared tree
two panes editing one file clobber each other and nothing reports it, so the three
protections below are conditions of implementing at all. A pane that cannot verify all
three does not edit: it names the one that is down and stops.

| protection | what it covers | how a pane gets it |
| --- | --- | --- |
| bead claim | who owns a unit of work | `br update <id> --claim --actor <pane>` |
| file reservation | who owns an edit surface | reserve through Agent Mail before the first edit |
| reservation guard | a commit touching a path somebody else reserved | the pre-commit hook refuses it |

**The claim is not a lock.** `br update --claim` refuses only when a *different* actor
holds the bead, and the default actor is the shell user. Two panes claiming under one
identity therefore both succeed, with no error anywhere. Pass `--actor` with the pane's
own name, every time. What actually prevents a second dispatch onto the same work is that
a claimed bead leaves `br ready --unassigned`.

**Mail is an outbox, not a mailbox.** Panes announce and do not reliably read, so anything
that has to reach a working agent goes into its pane, and anything that has to survive
goes on the bead rather than into a message.

**No pane has a role.** Agents are interchangeable, and everything one needs to continue
lives in the bead graph, the reservations and this file, never in another agent's context.
An agent that stops is replaced, not recovered.

## Changes you did not make are normal

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

## Picking up work

1. Confirm the working tree and the branch. Do not create or switch to another.
2. Register an identity with Agent Mail and use that name as the actor everywhere below.
3. `br ready --unassigned`, and take one bead. A bead already in progress belongs to
   somebody else: pick another. The only exception is a brief that says the bead was
   already claimed for you, and only because it says so.
4. Claim it before opening a single file: `br update <id> --claim --actor <name>`.
5. Reserve the exact edit surface. Narrowest correct path set, and a TTL sized to the edit
   rather than to the day. A reservation call can return a lease and still report
   conflicts, so read the conflict list: a path somebody else holds is not yours because
   the API answered.
6. Implement, with the tests the bead asks for. One bead at a time.
7. `bin/ci` green, then commit and push. Work that is not pushed does not exist to the
   other panes.
8. Release the reservation, record the outcome on the bead, close it.

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

## Editing discipline

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

## The tools this coordination runs on

Named here because the rules above are useless to a reader who cannot find them.

| tool | what it provides | where |
| --- | --- | --- |
| `br` | the bead graph: ready work, dependencies, claims, comments | <https://github.com/Dicklesworthstone/beads_rust> |
| MCP Agent Mail | agent identity, threads, advisory file reservations, and the pre-commit reservation guard | <https://github.com/Dicklesworthstone/mcp_agent_mail> |
| `ntm` | the tmux swarm: spawning panes, supervising them, `ntm health` | <https://github.com/Dicklesworthstone/ntm> |

## The baseline below, and what this project overrides

The block that follows is the shared engineering baseline, stamped in verbatim from one
source so it travels in the clone. It is not edited here. This project overrides exactly
one rule in it, the mandatory worktree per session, for the reason given in *One branch,
no worktrees*. Everything else in it holds as written.

<!-- BEGIN universal-principles v3 -->
## Working principles

- **The human defines the WHAT; the agent decides the HOW.** Don't wait for line-by-line
  dictation. Plan first for non-trivial tasks: show the plan + to-do list, wait for approval.
- **Think before coding — don't assume, don't hide confusion.** State assumptions explicitly;
  if multiple interpretations exist, present them — don't pick silently. If a simpler approach
  exists, say so and push back. If a task is impossible under the stated constraints, or info
  is missing, say so — don't guess. (For trivial tasks, use judgment; this is bias, not ritual.)
- **Surgical changes — touch only what you must.** Every changed line traces to the task.
  Don't "improve" adjacent code, reformat, or refactor what isn't broken; match existing style
  even if you'd do it differently. Flag unrelated dead code — don't delete it. Remove only the
  imports / variables / functions your own change orphaned.
- **Chesterton's Fence — find the problem before undoing the decision.** A config, a flag, a
  workaround that looks arbitrary is a **fence**: someone put it there, probably to fix
  something that is invisible to you *because the fence is working*. You arrive with no
  history, so absence of a visible reason is evidence of your ignorance, not of its
  uselessness. When your fresh measurement contradicts what the human vaguely remembers
  ("I changed this once, because of some problem"), **your measurement is the suspect first**
  — it may be measuring the case that *isn't* failing. Go find the original problem, then
  decide. *(A CIFS share was benchmarked with a big sequential `dd`, looked fast, and the
  local-disk download dir was "fixed" away — while the actual failure was random writes:
  par2, unrar, torrent piece-writes. Two wrong commits.)*
- **Goal-driven execution — define the success check, then loop to it.** Turn the task into
  something verifiable before coding: "add validation" → write tests for invalid inputs, then
  pass them; "fix the bug" → write a failing repro test, then pass it; "refactor X" → tests
  green before and after. For multi-step work, state a brief plan with a verify step each.
- **"Flaky" is not a diagnosis — test in the environment the thing actually runs in.** A
  component that fails *consistently* under automation is being **mis-invoked**, not being
  unreliable; "it works when I run it by hand" is not evidence that it works. The shell you
  test in has a TTY, a `$HOME`, an `ssh-agent`, an interactive stdin — the systemd unit, the
  CI job and the scripted harness have none of those, so a passing manual run can be testing
  a different program. Reproduce it *there* (start the unit, `env -u SSH_AUTH_SOCK`,
  `</dev/null`, `--dry-run` to print the real command line) before accepting "unstable" as a
  cause. **When a fix doesn't change the symptom, stop fixing and go look at what is actually
  being executed.** *(An interactive-mode flag with no TTY made one harness fail every review
  panel for weeks, written off as "flaky"; it was the wrong flag.)*
- **KISS — don't solve a problem you don't have yet.** Simplicity isn't "write less code";
  it's not building for a need that doesn't exist. Let structure emerge from the code.
- **YAGNI & flat.** No preventive abstractions, no single-use interfaces. Interfaces for
  real boundaries only. Architecture is *extracted* once a pattern proves itself in real
  use — never designed up front for a user who doesn't exist yet. Need pulls architecture.
- **Development cost is not your cost — don't let it pick the design.** Choosing between
  technical options, weight quality, simplicity, robustness and long-term maintainability;
  don't weight how long the work takes. The estimate comes out in human units — days,
  weeks — because that is what the training data measured, and the cheaper option then
  wins on a cost the agent does not pay. This is **not** licence to over-build: KISS and
  YAGNI decide *whether* a thing is needed, and this decides *how well* it is built once
  it is. "That would take a week" is not an argument here; "nothing needs this yet" is.
- **Order: make it work → make it right → make it fast** (Kent Beck), in that order. Most
  over-engineering is doing "right"/"fast" before a working thing exists to justify it.
- **Flag scope creep — a standing duty, not a suggestion.** When a solo tool starts being
  framed as a public / multi-user / multi-tenant / plugin-system / configurable-N-backends
  platform before a real, present need exists, STOP and ask: "Is this needed now?" Justify
  future-proofing against a need that exists *today*.
- **No silent decisions (comprehension debt).** Never make a silent architectural or
  design call — state it and record the rationale, so the reasoning is recoverable later.
- **Real decisions are presented in the chat, in isolation — never via popup.** When a
  design/architecture/scope/trade-off decision arises, surface it on its own: the options,
  what each means, pros/cons/trade-offs, and a recommendation — then decide together.
  Don't bury it mid-text or bundle it with other topics, and don't compress it into a
  quick-pick widget (e.g. AskUserQuestion) — the widget skips the reasoning and overlays
  the explanation. Widgets are for trivial short-answer picks only.

## Git: branches, commits, PRs, comments

- **Ask the repo for its default branch; never assume one.** Repos differ — `master` and `main`
  are both common, often in the same person's account — and a wrong guess sends a PR to a branch
  that does not exist, or, worse, has you "fixing" a URL that was right all along.
  `git symbolic-ref --short refs/remotes/origin/HEAD | sed 's|^origin/||'`, or
  `gh repo view --json defaultBranchRef -q .defaultBranchRef.name`.
  Never commit directly to it: branch, then PR.
- **A new repo starts on `main`.** That is the preferred name, and `init.defaultBranch` is
  set to it, so `git init` produces it without anyone choosing. It settles new repos only:
  an existing one keeps the branch it has, because renaming breaks open PRs, CI filters,
  deploy hooks and every permalink into the tree, and buys nothing. The rule above still
  governs everything already in existence — ask, never assume.
- **Branches** — Conventional Branch (conventionalbranch.org): `<type>/<kebab-description>`,
  types `feature/`, `bugfix/`, `hotfix/`, `chore/`, `release/`, `docs/`.
- **Commits** — Conventional Commits (conventionalcommits.org): `<type>(scope): <description>`,
  types `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `ci`, `build`, `perf`, `style`.
  Breaking change → `!` after the type or a `BREAKING CHANGE:` footer.
- **Atomic commits** — one logical change per commit, each independently green and
  revertible. Never `git add .` blind; split unrelated changes.
- **Always work in your own worktree — mandatory, not conditional.** Parallel sessions
  are opened freely and nothing signals their existence to you, so a "check whether another
  session is here first" step can never be reliable — the honest answer is always "maybe".
  The only collision-proof arrangement is structural: keep the main working tree on the
  default branch as a clean reference and **never work in it** — before your first write
  (commit, branch, rebase, stash; read-only exploration is exempt), create your own worktree
  and do everything there. **When the repo ships a worktree tool, that tool is the only correct
  way to make one** — `bin/worktree new <type>/<kebab-desc>`, or whatever the repo calls it.
  A raw `git worktree add` materialises tracked files and nothing else, so every git-ignored
  path the repo links into a worktree is silently absent, and the tools that read those paths
  fail inside it without saying why: a tracker whose database is git-ignored reports itself
  uninitialised and offers to create a second one. Only where the repo ships no such tool is
  `git worktree add ../<repo>-<task> -b <your-branch> <origin>/<default-branch>` the right
  command. Do this **whether or not** you believe another agent is running — that belief is
  exactly what you cannot verify. Report which worktree/branch you used; remove it once merged.
  Only the human can see all the open sessions.
- **Pull requests** — describe **what + why**. *What*: a 1–3 line summary. *Why* (the bulk):
  decisions, trade-offs, rejected alternatives. The diff shows the what; the PR explains why.
- **Comments** — always **WHY, not WHAT**: explain intent, never restate the obvious
  mechanics. Keep existing comments; they carry intent.

## Code style (baseline)

- Functions: 4–40 lines, one thing each (SRP). Files: under ~500 lines, split by responsibility.
- Names specific and unique — avoid `data`, `handler`, `Manager`, `util`.
- Explicit types. Early returns over nested ifs; max ~2 levels of indentation.
- Inject dependencies; wrap third-party libs behind a thin interface this project owns.
- No duplication — but don't extract *too early*. Tolerate duplication while the pattern is
  still forming; extract the abstraction *from* proven, repeated code, never ahead of it.
- **Refactoring is not automatic.** After a large feature, list refactoring candidates
  (files > ~500 lines, duplicated logic, long functions, hardcoded config) and ask before
  pruning — the human decides, the tests are the safety net. Consolidate when the thing
  works and the seams are obvious, not before.
<!-- END universal-principles v3 -->

## Tests (TDD)

- Every feature is born with a test; every bugfix with a regression test.
- Tests run with one command, no manual setup and no real credential. A test that cannot
  run headless is wrong.
- **A parser is tested against a captured fixture, never against a live file.** Transcript
  formats and provider responses change without notice, so the fixture is what pins the
  shape the code was written for, and a real file is what proves the fixture is still true.
  Keep both, and keep them separate.
- **The invariants above are tested as invariants**, not implied by a happy path: a stale
  reading must be observable as stale in the output, and a unit mismatch must fail to
  compile. Where a guard cannot fail, it is not a guard.
- Before saying "done", run `bin/ci` and report the result.

## Small releases

- Every commit on the shared branch passes `bin/ci` before it is pushed. Under one tree a
  broken commit is not one agent's problem to fix next: it is the tree every other pane is
  working in, so the next pane's failure is inherited and reads as its own.
- `main` only ever receives a merged pull request whose checks are green, so every commit
  on it is releasable.
- Push as soon as a bead is green. An unpushed commit is invisible to the other panes,
  which is the same as not having done the work.
- A release is a `v*` tag. The workflow builds the platform matrix, emits checksums and
  publishes the GitHub Release. Nothing is uploaded by hand.

## Security (habit, not a phase)

- Provider credentials are read from the environment or from the machine's own credential
  store. They are never written to the state directory, never logged, and never included
  in an error message or a debug dump.
- A response body from a provider is untrusted input: parse it, do not interpolate it into
  a shell command or a path.
- Dependency CVEs are caught by `cargo audit` in `bin/ci` and in CI.

## Prose

- No em-dash. Use a comma, a colon, a semicolon or a full stop. This is checked by
  `bin/ci`, and it applies to Markdown, source comments, config, commit messages and PR
  text alike.
- Bold marks structure (a bullet lead-in, a table header), never emphasis in the middle
  of a sentence. Same for italics: a term being introduced, not a word being stressed.
- No process narration anywhere a stranger can read it: no task ids, no phase names, no
  review rounds, no mention of who or what reviewed a diff, no reference to a session or
  a conversation. Commit and PR text describe the problem and the change, never how the
  work was organised.
- No audience in the text. A README says what the software does, not who is going to
  read it or why they are reading it.
- Comment density is low by default: the non-obvious only, the why and not the what.
  Long reasoning belongs in an ADR, not in a header comment.

## Git and secrets

- Before any commit, show `git status` and `git diff --cached`; confirm no secret is
  staged. If you spot one, STOP and report it. The gitleaks pre-commit hook is the
  deterministic backstop; this habit is the probabilistic one.
- Real secrets stay out of git. Only `.env.example`, with fake values, is committed.

## Landing a session

Before ending a session, in this order. A session that skips a step leaves the swarm in a
state the next pane has to reconstruct.

1. **File a bead for anything left over.** Work that exists only in a session's memory
   does not exist.
2. **Run `bin/ci`** and report the result.
3. **Update bead status.** Close what is done; leave `in_progress` only on work somebody
   is still holding.
4. **Release your reservations.** One that outlives its agent blocks a pane with no way to
   find out why.
5. **Commit your own paths and push.** An unpushed commit is invisible to every other
   pane, which is indistinguishable from not having done the work.

## Post-implementation checklist (run before "done")

1. Commits small and well described.
2. Refactoring candidates listed, if the change was large.
3. Security risks flagged, if you touched a sensitive surface.
4. This spec updated if behavior, setup or release flow changed, and any hurdle it gained
   is classified rather than just appended.

## Common hurdles

| hurdle | class | gate |
|---|---|---|
| A bare `cargo test` or `cargo run` reads `build.target-dir` and is not covered by the isolation `bin/ci` sets up, so a binary out of `target/` can be another pane's build | tripwire | pass `CARGO_TARGET_DIR` yourself before trusting anything built outside `bin/ci` |
| A stray empty reservation database can appear in the repository root. An agent that queries it sees zero reservations and concludes every path is free, which is the worst failure an advisory system has | tripwire | before trusting an empty conflict list, confirm the store being read is the shared one and not a file in the tree |
| `br update --claim` guards on identity, not on a lock. Under the default actor two panes claim the same bead and neither is told | tripwire | always pass `--actor` with the pane's own name |
| A dispatch that dies before its agent starts leaves a bead held by nobody, and nothing expires it | prose | return it with `br update <id> --status open` and say what stopped it |
| `br init` creates `.beads` mode 0755, and `bd` then prints a warning ahead of its version string, which the version probe parses as the version | tripwire | `chmod 700 .beads` immediately after any `br init` |
| The prose gate refuses the em-dash in every tracked file, in the commit message and in the PR body. The commit message is the only one of the three nothing reads a second time | ci | `bin/ci` calls `bin/slop-guard`; the `commit-msg` hook covers the message |
| A clone has no hooks until `bin/install-hooks` runs, so the secret scan is off on a fresh checkout | prose | run it once after cloning |

**A hurdle promoted to a gate is deleted from this table, not duplicated.** The gate is the
instruction; a line here restating it only dilutes the ones still unguarded.
