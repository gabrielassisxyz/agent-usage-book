# agent-usage-book

One ledger for LLM consumption. `aub` joins two numbers that are usually kept in
different places and in different units: **token spend**, read from the transcripts agent
CLIs leave on disk, and **quota**, measured against the providers' own endpoints. It
reports both from one command, so a decision about which model to send work to is made
from one source instead of from several tools that quietly disagree.

## Status

Early. The harness, the licence obligations and the correctness rules are in place; the
measurement itself is being built. There is no release yet.

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

## Install

Once the first release is tagged, download the archive for your platform from the
Releases page and put the `aub` binary on your `PATH`.

From source:

```sh
cargo install --path .
```

## Configuration

Nothing that identifies a machine, an account or a person is compiled in. Transcript
paths, the accounts to measure and the state directory are configuration.

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
