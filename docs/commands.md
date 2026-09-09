# Command reference

`aub --help` prints the mechanical contract for every shipping command: the
question it answers, which shared flags it refuses and why, and the output
formats it accepts. That block is generated from the same policy the parser
enforces, so it cannot drift from behaviour and this document does not repeat
it.

What `--help` does not carry is the behavioural boundary: not which flags a
command rejects, but what it will never do regardless of how it is called.
That boundary is what decides whether a command is safe to put in a timer, a
script, or a human's muscle memory, and it is what this document adds. Every
shipping command has a section below; a section that only restated `--help`
would not be worth a second document.

Test hooks (`__logging-fixture`, `__state-check`, `__exit-class`,
`__attempt-crash-hook`, `__projection-crash-hook`, `__cost-model-fixture`) are
not part of the shipping surface and have no section here, matching `--help`,
which does not list them either.

## `aub status`

**Answers:** how much quota does each configured account have left?

**Refuses:** the network, a write, and SQLite. `status` reads the last
published projection file and nothing else, so it never blocks on a provider
and never contends with a concurrent sampler for the store. It exits non-zero
only for an argument-parsing failure; a stale reading, an auth-required
account, or a missing projection are answers, not errors, and all render with
exit 0 so a status bar never treats degraded output as process failure.

Text output goes through one style layer (`src/presentation/style.rs`), which
also answers `--no-color` where every other command refuses it. Colour is on
only when stdout is a terminal, `NO_COLOR` is unset or empty, and
`--no-color` was not passed; `--format json` is never styled. A window row is
tinted by percent used, green below 60, yellow from 60, red from 85, idle grey
at nothing used, while the words stay the freshness answer, so the text is
never colour-dependent.

**Layout.** `status` prints a grouped grid: a `QUOTA` header with the current
local time right-aligned to the terminal width, then one block per provider
(accounts in the order the config lists them), each account a name-and-plan
line over one row per quota window. The row columns are a fixed width: the
window label (`5h`, `week`, or a model display name), a 30-cell bar that fills
as quota is *used*, the percent used, the burn rate (`1.0x` is on pace to hit
the cap exactly at the reset), then the reset in local time with any note.

```
QUOTA                                                       Mon 07 Sep 14:22 -03

anthropic ──────────────────────────────────────────────────────────────────────
  primary  anthropic
    5h       ━━━━━━━━━━━━━━━━━━━───────────   62%   1.19x Mon 17:00
    week     ━━━━━━━━━━━━──────────────────   41%   0.95x Fri 09:00
    fable    ━━━━━━━━━━━━━━━━━━━━━━━━━━────   88%   2.04x Fri 09:00

  gmail  anthropic
    5h       ━━━━──────────────────────────   12%   0.23x Mon 17:00
    week     ━━────────────────────────────    5%   0.12x Fri 09:00
```

A stale account dims its whole block and each row's note reads
`cached <age> ago  <reason>`; an auth-required account shows `auth!` in place
of the grid; an unreadable projection is the bare `aub ?`. `--format json`
carries every window under `accounts[].windows[]`, not only the limiting one
(schema v3). `--model NAME` narrows both the grid rows and the reading to the
account-wide and named-model windows.

## `aub now`

**Answers:** how much quota does each configured account have right now?

**Refuses:** to answer from a cache. Unlike `status`, `now` always forces a
fresh sampling attempt first and renders the result that attempt produced;
there is no flag that fetches and discards, and there is no mode that reads
the last published projection instead of sampling.

`--session-id SESSION` additionally asks whether that session is actively
spending right now. The answer is one of four typed states: an explicit
launcher-or-hook marker with a fresh heartbeat both covering this instant
(`explicit_marker_evidence`, the only state that names an account and prints
a `spending` line), nothing named or found (`no_evidence`), two explicit
markers naming different accounts for the exact same instant with no way to
order them (`conflicting_evidence`), or a marker whose session the heartbeat
policy no longer finds live (`inactive`). Neither a moving meter nor an
ambient credential ever substitutes for either half of that claim, and
without `--session-id` the state is always `no_evidence`.

## `aub spend`

**Answers:** how many canonical tokens were used, grouped by the requested
dimensions? `--group-by` takes `day`, `session`, `project`, `repository`,
`task` or `account` and is repeatable, so `--group-by account --group-by day`
nests days under each account.

Account grouping is the session-identifier join: the session id already appears
in every transcript and in every account marker, and `--group-by account` reads
both. Attribution is decided by the marker-interval segmentation, never by this
command; usage no marker can justify lands in the `unknown-account` group, which
is reported as its own partial group rather than merged or dropped. `--explain`
on an account group names the exact markers behind the attribution and their
evidence class.

**Refuses:** to guess at an unreadable transcript. A source that cannot be
normalized leaves the report `IngestIncomplete` rather than silently omitted
or extrapolated from what did parse. `spend` also refuses to answer a quota
question; `status`, `now` and `sample` own that, and refuses to forecast a
cost that has not happened yet.

## `aub task`

**Answers:** which task or named overhead bucket consumed this usage, by
temporal segmentation of the issue tracker's claim history? `ingest` lands the
tracker's claim and release events, `report TASK-ID` totals one task across
every session that contributed to it, and `overhead` reports the usage that
belonged to no claim, bucketed by the reason it belonged to none.

**Refuses:** to manage issues. The tracker database is opened read-only and is
never written to; `aub` reads a claim history it did not produce and has no way
to change a task's state. It also refuses to classify usage itself: all three
subcommands, and `aub spend --group-by task`, read one segmentation engine, so
a task total and a task-grouped spend row can never disagree. Usage outside
every claim window is not dropped and not folded into a neighbouring task: it
is reported under a named overhead bucket that says why it was unattributable.

## `aub can-run`

**Answers:** given a fresh or cached calibrated credit headroom and the
historical cost of `--task-kind TYPE`, can `--task-model MODEL` run now under
`--account NAME`? By default the command performs and persists one fresh
meter sample for the account first, the same sampler path `aub sample
--account` uses, before advising; `--cached` uses the newest persisted
reading instead, but only while it still satisfies the freshness policy.

**Refuses:** to guess. Any of seven missing prerequisites, a stale meter,
authentication required, a constraining window with no applicable current
calibration, a cost model missing a token class, a plan tier mismatch, too
few historical tasks, or mostly unattributable task records, produces a
refusal naming every one that applies in the same invocation rather than the
first one found, and a refusal exits `0` with the refusal rendered, not a
usage error. It never substitutes a global average task cost, a different
plan tier's calibration, an estimated-token session, a stale meter reading,
or an API-list-price conversion for a number it cannot justify.

## `aub config`

**Answers:** which configuration key resolved to which value, and from where?

One box titled with the config file path (`~` for the home directory), one
block per section with the section name as its title and the keys under it
without the section prefix (the full box for a two-account config):

```
┌─ config · ~/.config/aub/config.toml ─────────────────────────────────────────┐
│                                                                              │
│  accounts                                                                    │
│    work-primary    provider-a  file:/tmp/aub-golden/creds-primary.js…  file  │
│    work-secondary  provider-b  env:AUB_GOLDEN_TOKEN                    file  │
│      exclusivity_policy permit_passive                                       │
│                                                                              │
│  adapter_semantics                                                           │
│    max_comparison_age       30d                                     default  │
│                                                                              │
│  anthropic                                                                   │
│    refresh                  true                                    default  │
│                                                                              │
│  antigravity                                                                 │
│    refresh                  true                                    default  │
│                                                                              │
│  attribution                                                                 │
│    recent_window            30d                                     default  │
│                                                                              │
│  backup                                                                      │
│    destination              /tmp/aub-golden/backups                    file  │
│    keep_daily               7                                       default  │
│    keep_monthly             6                                       default  │
│    keep_weekly              4                                       default  │
│    keep_yearly              2                                       default  │
│    review_after             36h                                        file  │
│                                                                              │
│  can_run                                                                     │
│    ample_margin_multiple    2                                       default  │
│    headroom_bound           low                                     default  │
│    labels                   true                                    default  │
│                                                                              │
│  coverage                                                                    │
│    attempt_floor            0.98                                    default  │
│    measurement_floor        0.95                                    default  │
│                                                                              │
│  doctor                                                                      │
│    meter_anomaly_horizon    15m                                     default  │
│                                                                              │
│  drill                                                                       │
│    max_age                  30d                                     default  │
│                                                                              │
│  freshness                                                                   │
│    meter                    12m                                     default  │
│                                                                              │
│  ingest                                                                      │
│    max_batch_events         5000                                    default  │
│    max_batch_files          200                                     default  │
│    max_batch_seconds        2s                                      default  │
│                                                                              │
│  reconciliation                                                              │
│    residual_min_eligible    5                                       default  │
│    residual_window          30d                                     default  │
│                                                                              │
│  sampling                                                                    │
│    busy_timeout             10s                                     default  │
│    command_budget           8s                                      default  │
│    default_interval         5m                                      default  │
│    max_concurrent_requests  2                                       default  │
│    request_timeout          5s                                      default  │
│    reset_edge_lead          2m                                      default  │
│    retry_after_cap          1h                                      default  │
│    scheduler_tick           1m                                      default  │
│                                                                              │
│  state                                                                       │
│    dir                      ~/.local/state/aub                      default  │
│                                                                              │
│  task_distribution                                                           │
│    attribution_floor        0.8                                     default  │
│    central_high             75                                      default  │
│    central_low              25                                      default  │
│    min_samples              12                                      default  │
│    quantile_method          nearest-rank                            default  │
│    upper                    90                                      default  │
│                                                                              │
│  transcripts                                                                 │
│    cli-a  claude-code  /tmp/aub-golden/cli-a                           file  │
│      pattern **/*.jsonl                                                      │
│    cli-b  codex        /tmp/aub-golden/cli-b                           file  │
│      pattern **/*.md                                                         │
│      usage_evidence measured                                                 │
└──────────────────────────────────────────────────────────────────────────────┘
```

`accounts` prints one line per account (`name  provider  credential  source`)
with a second dim line only when the account's `exclusivity_policy` differs
from the default; `transcripts` prints one line per source with the `pattern`
(and any `usage_evidence`) on dim lines under it. The source column reads
`default` in dim and `file`, `override` or `environment` in body text, so the
keys the operator set stand out from the ones they did not. Values longer
than the room to the source column end in `…`. A `--set key=value` override
prints with source `override`.

**Refuses:** to invent a value for a key nobody set. An unset key prints with
source `default`, never a value that looks like it came from a file. It also
never prints credential material, only the kind and reference (`file:<path>`,
`env:<NAME>`, `none`) of the key that names one.

An account's `credential` table takes one of three kinds:

```toml
[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "~/.config/provider/creds.json" }

[[accounts]]
name = "research"
provider = "anthropic"
credential = { kind = "env", name = "OPENCODE_SESSION_COOKIE" }

[[accounts]]
name = "transcript-only"
provider = "codex"
```

The `file` kind reads the credential file at `path`; the `env` kind reads the
environment variable at `name` and uses its value as the credential material.
An account with no `credential` table, or `kind = "none"`, reads its meter
from the transcript. A credential that is missing, unreadable or empty is an
authentication-required outcome naming the account and the source, never a
crash, and the material is never printed, logged or persisted; the context id
stored with each attempt identifies the credential revision without exposing
its bytes and changes when the value is replaced.

For scheduled runs, note that the sampler runs from `aub-sample.service`,
which has no shell: a variable exported in an interactive profile does not
reach it. Give the service an `EnvironmentFile=` of its own (for example
`~/.config/aub/env`, mode 0600, one `NAME=value` line per secret; systemd
does not read `export` lines) and wire it into the unit with
`EnvironmentFile=%h/.config/aub/env`.

## `aub export`

**Answers:** which usage did each session or run consume, as a versioned
JSONL ledger for an external join?

**Refuses:** to run without a chosen join key. `--key session-id|run-id` is
required; `export` does not guess which key a downstream consumer wants.

## `aub rate-card`

**Answers:** what do the immutable dated vendor rate cards contain?

**Refuses:** to edit history. A rate book is imported into a new, immutable,
versioned record; correcting a stale price means importing a new version,
never mutating one already on record.

## `aub backup`

**Answers:** is there a consistent, verified archive of the durable state,
and does it restore?

Usage: `aub backup [DESTINATION]` writes a new dated archive under the
destination root (the explicit argument wins, otherwise
`backup.destination`); `aub backup verify DESTINATION` re-checks one
archive; `aub backup restore ARCHIVE DEST` recovers from one archive.
Each run keeps a series of dated archives under tiered retention
(`backup.keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly`); an
archive is retained when any bucket keeps it, and the most recent verified
archive is never pruned.

**Refuses:** to report verified without checking. Creating and verifying an
archive both run the same checksum, manifest and SQLite integrity checks, so
neither a fresh backup nor a re-check can claim `verified=true` on faith.
An individual archive directory is never written over. Pruning is refused
when no verified archive exists, and a failed verification prunes nothing
and advances no pointer.
`backup restore` refuses a destination that already exists or that resolves
to the configured state directory: a restore only ever writes into a new
directory, and the damaged state directory is never a valid destination.

## `aub clear-diagnostics`

**Answers:** how many retained diagnostic bodies were cleared?

**Refuses:** to clear anything but diagnostic material. Provider response bodies retained
for diagnosis are rebuildable; the meter evidence beside them is not, and this verb never
reaches it. It also refuses `--explain`, because clearing derives no quantity, and
`--account`, because retention is scoped by provider rather than by account.
## `aub drill`

**Answers:** does the documented recovery procedure actually recover a damaged state
directory, and is that still true today?

**Refuses:** to drill against the live state directory. Every case runs against a scratch
destination given on the command line, because a drill that damages the thing it is meant
to prove recoverable has proved the opposite. It also refuses `--account` and `--model`: a
drill exercises the whole state directory, not one slice of it.

## `aub compare`

**Answers:** does the adapter's stored reading of one window agree with what the
provider's own authoritative surface showed for it?

**Refuses:** to accept a verdict from the caller. `compare record` always computes the
verdict from the stored adapter reading, the surface value and the documented
granularity given on the command line, through the same function the validation
procedure (`docs/adapter-semantics-validation.md`) describes; there is no flag that
sets agreement or mismatch directly. It also refuses to write a second comparison for
a window that already carries one, naming the existing record instead of overwriting
it, and refuses `--format`, `--explain`, `--account` and `--model`: a comparison is
scoped by observation and window, not by any of those dimensions.

## `aub calibrate`

**Answers:** what is the state of the controlled calibration experiment, and
what quota window capacity is fitted from recorded meter observations?

**Refuses:** to run a resident process. `begin`, `status` and `end` each open
the ledger, do their read or write, and exit; the experiment survives in the
database between them, including across a reboot. Sampling cadence during an
experiment is tightened by invoking `sample --due` more often through the
external scheduler, never by a loop inside `aub`. `begin` refuses when the
named cost model covers none of the expected token kinds, when the account has
no sampled baseline yet, and when the account already runs an experiment;
`end` records the end of controlled work and never declares the meter
settled. `fit` and `passive` refuse to activate candidate calibrations automatically:
candidates are written immutably and never promoted to active status by the
fitter (Invariant 14). Subcommand `passive` generates candidates from
uncontrolled, recorded observations across clean intervals under strict
eligibility rules. Intervals are eligible only when the account's configured
exclusivity policy explicitly permits passive fitting (`exclusivity_policy = "permit_passive"`
under `[[accounts]]`). Absent key defaults to `"forbid_passive"` (conservative/fail-closed),
and unknown values fail configuration loading with an error naming `accounts[].exclusivity_policy`.
The command also refuses `--explain`, `--model` and `--no-color`: a
calibration experiment or fit is scoped by account, window or evidence set,
never by those dimensions.

## `aub ingest`

**Answers:** have the transcript-derived tables been refreshed from the
transcripts on disk, under one generation?

**Reads:** the configured `[[transcripts]]` sources, each with a `format` of
`claude-code`, `codex`, `opencode` or `pi`. An `opencode` source names the
directory holding its session database with the pattern `opencode.db`; the
database is opened read-only and parsed whole, never sliced by lines.

**Refuses:** to touch anything but rebuildable, transcript-derived rows. It
never writes or deletes a meter attempt, a response, an observation, a
calibration, or any other irreplaceable evidence.

## `aub rebuild`

**Answers:** can the transcript-derived materializations be rebuilt from
scratch while every irreplaceable record is left untouched?

**Refuses:** the same evidence `ingest` refuses to touch, structurally rather
than by convention: `rebuild` can only address rebuildable materialization
groups, so it has no code path that could delete a meter attempt, an attempt
result, response evidence, an observation, or a calibration even if asked to.

## `aub doctor`

**Answers:** is the recorded evidence healthy, and does the transcript
corpus still match its parsers?

**Refuses:** to repair anything unless `--fix` is given, and even then it
refuses anything outside the four permitted repairs; the rest of the check
registry only reports. `--fix` also refuses combination with
`--transcript-format-drift` or `--rate-card-staleness`: those are read-only
detail views of one check's own evidence, not a repair mode.

## `aub coverage`

**Answers:** did the sampler attempt what the policy owed, and did those
attempts observe?

One box: a row per configured account, that account's findings indented
under its own row, the ledger's retired accounts on one `not in config`
line, and the threshold verdict's next action as the footer. Each finding
names the provider error classification the interval's failed attempts
stored, largest count first (`1 attempt refused with rate_limit_error`);
the classification is what the provider itself said about the refusal,
sanitized, and the full stored value, message included, is on the attempt
result row in the ledger. Rows written before the classification column
was populated read as `unclassified`. An account added part-way through the
window divides attempts made by attempts owed since its first sampling-policy
snapshot, and the attempts cell names the covered span beside the fraction
(`98.7% of 12h 46m`); a fully covered window shows the fraction alone. The
detail block then reads `policy known for 12h 46m of 24h`, and only for a
partial span. `unknown` is reserved for an account with no applicable
snapshot anywhere in the window.

```
┌─ coverage · last 24h ────────────────────────────────────────────────────────┐
│                                                                              │
│  account  attempts  measurements  longest gap  resets unobserved             │
│  ───────────────────────────────────────────────────────────────────             │
│  primary  88.9%     100.0%        6m           3                             │
│         attempt coverage below the 98% floor                                 │
│         3 resets inside a sampling hole                                      │
│  gmail    100.0%    76.6%         6m           3                             │
│         45 attempts refused with rate_limit_error                            │
│         14 attempts refused with authentication_error                        │
│         3 resets inside a sampling hole                                      │
│                                                                              │
│  not in config: primary-2026-09-04 (last observed 2026-09-04)                │
│  next: run coverage again once the floor condition changes                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Refuses:** the network. `coverage` reads only local ledger history, so it
tells a dead scheduler apart from a live one that is failing on credentials
by what the ledger recorded, never by asking a provider directly.

## `aub import`

**Answers:** which legacy evidence is safe to import into the ledger?

**Refuses:** to import without a verified backup path named first with
`--backup VERIFIED_ARCHIVE`, and refuses a blanket scan: only the explicitly
named source is imported, never everything a directory happens to contain.

## `aub statusline`

**Answers:** what did the status line's payload say each account's meter
windows were, without the renderer knowing?

**Refuses:** to fail the status line, ever, and to attribute by anything but
the environment. The verb is the tee the Claude Code `statusLine` command
pipes its payload through: it reads stdin once, writes those exact bytes to
stdout unchanged, exits 0 in every case, and records the payload's
rate-limit windows in the background. A missing config, a profile no
configured anthropic account answers to, malformed JSON, or an unwritable
state directory all leave the renderer exactly what it would have received
without this verb in the pipeline, with nothing written.

Attribution reads `SHALLOW_PROFILE` from the environment (the variable the
caam launcher sets on every session it spawns) and matches it, exactly,
against a configured `provider = "anthropic"` account. A payload with no
`SHALLOW_PROFILE`, or with one matching no such account, is passed through
and not recorded: an account guessed from `$HOME` or a credential file is
the one attribution this tee will not make.

**The record.** `<state.dir>/statusline/<account>.jsonl`, one JSON object
per line, appended only when a window's `used_percentage` or `resets_at`
moved for that session (renders arrive many times per turn; a small
`session-<id>.last` file per session holds the last recorded state):

```json
{"received_at":"2026-09-08T02:34:56Z","session_id":"...","cwd":"...","windows":{"five_hour":{"used_percentage":40,"resets_at":1786834200},"seven_day":{"used_percentage":12,"resets_at":1786920000}}}
```

`received_at` is the tee's own clock (RFC 3339 UTC); `resets_at` lands as an
integer epoch second whatever form the payload spelled it in. Windows are
admitted by shape, any object under `rate_limits` carrying a numeric
`used_percentage`, so a new model-scoped window appears under its own name
without a release. Nothing else from the payload is kept, the payload itself
is never written, and the file is append-only; rotation is the reader's
decision, not the tee's.

**Installation.** The `statusLine` command in `~/.claude/settings.json`
becomes `aub statusline | ~/.claude/statusline-with-usage.sh`, with an
absolute path to the binary, since a status line does not source a shell:

```json
{"type": "command", "command": "/abs/path/to/aub statusline | /abs/path/to/statusline-with-usage.sh"}
```

## `aub sample`

**Answers:** are configured accounts due for meter sampling, and what did
the endpoints observe?

**Refuses:** to fail merely because a remote call came back with an
authentication or transport failure. In its scheduled shape, durably
recording that outcome is success; the command exits non-zero only when it
could not persist or operate at all. A caller that wants the ordinary
live-source exit classes instead asks for them explicitly with
`--require-success`, which still records the same evidence first.

## `aub account`

**Answers:** which accounts has the ledger recorded, and how do I rename one
without losing its history?

**Refuses:** to re-key a row. `rename` changes only the text a `(provider,
name)` pair spells; the account id `list` prints and every evidence row keys
on is untouched, so a rename can never sever an observation from its history.
It also refuses to run while the configuration still names the old value,
because a running timer or hook resolves accounts by that exact pair and
would otherwise keep sampling under a name the ledger no longer has, refuses
a new name already taken by another row for the same provider, and refuses
while the sampler holds a live lease on the old name, naming the holder,
rather than racing an attempt already in flight against it. When the name
being retired is one an older row already holds (a name reused after a
config change), rename that older row out of the way first; nothing here
deletes a row, so both stay in `list` under their own names.

