# agent-usage-book

One ledger for LLM consumption. `aub` joins two numbers that are usually kept in
different places and in different units: **token spend**, read from the transcripts agent
CLIs leave on disk, and **quota**, measured against the providers' own endpoints. It
reports both from one command, so a decision about which model to send work to is made
from one source instead of from several tools that quietly disagree.

## Status

Implemented and in daily use on the machine it was built for, ahead of a first tag.

What is live:

- **Quota.** An external timer invokes `aub sample --due`, the provider adapters
  (Anthropic, Codex, agy, opencode, Ollama) record every attempt and every response as
  durable evidence, and `aub status` renders the recorded windows per account from the
  published projection: a bar per window, the percent used, the burn rate, the reset in
  local time, and how old the observation behind the reading is. It never touches the
  network unless asked with `--refresh`, and a stale or auth-required account renders as
  that, with exit 0.
- **Spend.** `aub spend` refreshes and reads the canonical ledger built from agy, Claude
  Code, Codex, opencode and pi transcripts, grouping token vectors by day, session,
  project, repository, harness, model, task or account, and preserving evidence
  qualification and provenance at every subtotal.
- **Task attribution.** `aub task ingest` reads every configured tracker's claim history
  read-only, `aub task report` totals one task across the sessions that contributed to
  it, and usage that belonged to no claim is reported under a named overhead bucket
  rather than folded into a neighbouring task.
- **Coverage, backup and diagnosis.** `aub coverage` answers whether the sampler
  attempted what the policy owed and whether those attempts observed; `aub backup` and
  `aub drill` create, verify and rehearse the restore of the state directory; `aub
  doctor` reports health, drift and integrity; `aub import` lands pre-`aub` history with
  explicit legacy provenance.
- **Advice.** `aub can-run` compares the empirical cost of comparable finished tasks
  against the headroom measured now, and refuses, naming every missing prerequisite at
  once, rather than guessing.

What is not live:

- **No active window calibration.** `aub calibrate fit` and `aub calibrate passive`
  write candidates from recorded observations and never promote one; activating a
  candidate is a deliberate operator step. Until a window carries a current active
  calibration, `aub can-run` and `aub spend --window-equivalent` price its headroom from
  the dated rate cards and label it `(estimated)`, or refuse when a recorded calibration
  is not current.
- **No release.** There is no tag and no published archive yet; the first one is gated on
  `bin/release-criteria` reporting every criterion in
  [docs/release-criteria.md](docs/release-criteria.md) as passing.

## Why it exists

Spend and quota were being answered by five overlapping tools, each with its own
assumptions. The failure that mattered was never one of them being down. It was a credit
ceiling copied by hand from one tool into another, which stayed correct until the day it
did not, and reported a confident wrong number in between. So the guarantees this project
buys are about **units and freshness**, not about speed:

- Quantities are separate types. A percentage cannot be added to a credit balance, and a
  token count cannot be printed where a cost belongs.
- Every reading says whether it is `fresh`, `stale`, or blocked on `auth_required`. There
  is no third state that renders as if it were the first.
- Constants have one definition. Nothing is copied between tools.
- Where a source cannot be reached, the output says so. It does not fall back to the last
  value, and it does not print a zero.

## Not in scope

- **A server, a daemon or a container.** This is a binary that runs, answers and exits.
- **An async runtime.** About three endpoints are called, concurrently and once. Blocking
  requests in scoped threads cover that, and a runtime would be carried for nothing.
- **Cost forecasting and budget enforcement.** Measuring what happened is a different
  problem from predicting what will, and mixing them would make the measurement layer
  answerable for a guess. `aub can-run` is not an exception to that: it reports the
  empirical range of comparable finished tasks against quota measured now, and forecasts
  neither a duration nor future spend (PLAN.md section 26, "no duration forecasting"). It
  also enforces nothing, since it advises and never refuses a run on the operator's
  behalf.
- **Provider coverage for its own sake.** An endpoint is added when work is actually
  being routed through it.

## Install

Once the first release is tagged, download the archive for your platform from the
Releases page and put the `aub` binary on your `PATH`.

From source:

```sh
cargo install --path .
```

## Configuration

Nothing that identifies a machine, an account or a person is compiled in. Transcript
paths, the accounts to measure and the state directory are configuration. The file is
`$HOME/.config/aub/config.toml`, or whatever `AUB_CONFIG_FILE` names; `aub config` prints
every resolved key with the source that won.

A transcript source names its root, the glob that finds its files beneath it, and the
format the parser reads:

```toml
[[transcripts]]
name = "claude-code"
root = "/path/to/.claude/projects"
pattern = "**/*.jsonl"
format = "claude-code"   # or "agy", "codex", "pi", "opencode"
```

An opencode source names the directory holding its session database; the
database itself is the one file the pattern matches:

```toml
[[transcripts]]
name = "opencode"
root = "/path/to/.local/share/opencode"
pattern = "opencode.db"
format = "opencode"
```

Every task tracker `aub task ingest` reads is a `[[trackers]]` entry, and each
carries the source name its events are keyed under, so a bead id from one
repository is never conflated with the same id in another:

```toml
[[trackers]]
name = "agent-usage-book"
kind = "local"
path = "/path/to/repositories/agent-usage-book/.beads"

[[trackers]]
name = "kernl"
kind = "local"
path = "/path/to/kernl/.beads"
```

`name` is the identity the ledger keys the tracker's history on, so it is
configured rather than derived from the path, and two entries must not share
one name. The full shape and the refusal rules are in
[docs/commands.md](docs/commands.md).

`aub spend` reports today by default; `--since YYYY-MM-DD` and `--days N` widen the
window. Repeat `--group-by day|session|project|repository` for nested subtotals and set
`--refresh auto|never|force` to control transcript ingest. `--format json` emits the
versioned envelope with a `{value, unit}` per token kind.

`--group-by account` attributes usage through the session markers, with two rules that
read the account off the model id instead: the built-in provider prefixes put
`opencode-go/*` under `opencode-go` and `opencode/*` under `opencode-free`, and a
`-k1`/`-k2`/`-k3` key slot on a mapped id becomes the account `<vendor>-<slot>`. Both are
described in [docs/commands.md](docs/commands.md).

## Scheduling

`aub` has no daemon: something external has to invoke `aub sample` on a cadence, and the
agent session that starts should mark which account it belongs to. Example systemd and
cron units, an example session-start hook, and the full reasoning are in
[docs/scheduling.md](docs/scheduling.md).

## Documentation

[docs/operations.md](docs/operations.md) is the operator's entry point: everything above,
plus the backup policy, the import of any pre-`aub` history, and the recovery procedure,
in the order a fresh machine needs them.

- [docs/commands.md](docs/commands.md): what each command answers, and what it refuses.
- [docs/scheduling.md](docs/scheduling.md): the scheduler and hook setup, with working
  examples.
- [docs/backup.md](docs/backup.md): the backup policy, as ordered steps.
- [docs/recovery.md](docs/recovery.md): the recovery procedure, as ordered steps.
- [docs/exit-classes.md](docs/exit-classes.md) and
  [docs/problem-codes.md](docs/problem-codes.md): the scripting contract, checked against
  their enums by the test suite.
- [docs/diagnostics.md](docs/diagnostics.md): the structured diagnostic event vocabulary
  on stderr.

## Development

```sh
bin/install-hooks   # once after clone: gitleaks secret scan, commit message gate
bin/ci              # format, lint, test, dependency audit, prose guard
```

`bin/ci` is the exact thing CI runs, so a green local run means a green PR.

## Licence

MIT. See [LICENSE](LICENSE).

This binary ports logic from [quota-axi](https://github.com/kunchenguid/quota-axi) and
[axi-sdk-js](https://github.com/kunchenguid/axi), both MIT. A port is a derivative work,
so their copyright and permission notices are preserved in [NOTICE](NOTICE) and ship with
every copy.
