# agent-usage-book: agent briefing

> Read before every interaction. Living spec: short, imperative. On every gotcha or
> decision, append one line here.

> **What it is:** one ledger for LLM consumption, joining token spend read from local
> agent transcripts with quota measured at the provider's own endpoints, so routing a
> model is decided from one number instead of five tools that disagree.
> **Calibration:** Tier 2 · Phase: work. External stakes are contained (a local binary,
> no server, no user data), personal stakes are high: this is the measurement layer that
> feeds model-routing decisions, so a wrong number here is acted on elsewhere.
> **Review gate:** standard. One independent opinion over the whole branch diff, once,
> pre-push. No per-commit reviews.

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
- **New worktree:** `bin/worktree new <type>/<kebab-desc>`. Never work in the main tree.

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

- Every commit on `main` passes `bin/ci` and is releasable. No broken commit fixed by the
  next one.
- Closed work is committed before switching tasks; flag it if it has not been.
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

## Post-implementation checklist (run before "done")

1. Commits small and well described.
2. Refactoring candidates listed, if the change was large.
3. Security risks flagged, if you touched a sensitive surface.
4. This spec updated if behavior, setup or release flow changed, and any hurdle it gained
   is classified rather than just appended.

## Common hurdles

| hurdle | class | gate |
|---|---|---|
| A bare `cargo test` or `cargo run` reads `build.target-dir` and is not covered by the per-worktree isolation `bin/ci` sets up, so a binary out of `target/` can be another branch's | tripwire | pass `CARGO_TARGET_DIR` yourself before trusting anything built outside `bin/ci` |
| `br init` creates `.beads` mode 0755, and `bd` then prints a warning ahead of its version string, which the version probe parses as the version | tripwire | `chmod 700 .beads` immediately after any `br init` |
| The prose gate refuses the em-dash in every tracked file, in the commit message and in the PR body. The commit message is the only one of the three nothing reads a second time | ci | `bin/ci` calls `bin/slop-guard`; the `commit-msg` hook covers the message |
| A clone has no hooks until `bin/install-hooks` runs, so the secret scan is off on a fresh checkout | prose | run it once after cloning |

**A hurdle promoted to a gate is deleted from this table, not duplicated.** The gate is the
instruction; a line here restating it only dilutes the ones still unguarded.
