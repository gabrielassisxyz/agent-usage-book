# agent-usage-book

One ledger for LLM consumption. `aub` joins two numbers that are usually kept in
different places and in different units: **token spend**, read from the transcripts agent
CLIs leave on disk, and **quota**, measured against the providers' own endpoints. It
reports both from one command, so a decision about which model to send work to is made
from one source instead of from several tools that quietly disagree.

## Status

Early. The harness, the licence obligations and the correctness rules are in place, and
the first half of the measurement exists: `aub spend` refreshes and reads the canonical
ledger built from Claude Code, Codex and pi transcripts. It groups token vectors by day,
session, project and repository, preserving evidence qualification and provenance at
every subtotal. Quota is not measured yet: `aub status` renders every configured account
as never observed until the sampler lands. There is no release yet.

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
  answerable for a guess.
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
format = "claude-code"   # or "codex", "pi"
```

`aub spend` reports today by default; `--since YYYY-MM-DD` and `--days N` widen the
window. Repeat `--group-by day|session|project|repository` for nested subtotals and set
`--refresh auto|never|force` to control transcript ingest. `--format json` emits the
versioned envelope with a `{value, unit}` per token kind.

## Scheduling

`aub` has no daemon: something external has to invoke `aub sample` on a cadence, and the
agent session that starts should mark which account it belongs to. Example systemd and
cron units, an example session-start hook, and the full reasoning are in
[docs/scheduling.md](docs/scheduling.md).

## Documentation

[docs/operations.md](docs/operations.md) is the operator's entry point: everything above,
plus the backup policy and the recovery procedure, in the order a fresh machine needs
them.

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
