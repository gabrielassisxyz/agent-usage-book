# Command reference

`aub --help` prints the mechanical contract for every shipping command: the
question it answers, which shared flags it refuses and why, and the output
formats it accepts. That block is generated from the same policy the parser
enforces, so it cannot drift from behaviour and this document does not repeat
it.

Every command also accepts `--help` or `-h` anywhere after its name, and `aub
help <command>` prints the same thing: the command's usage line, its summary
and its block from `aub --help`. The help flag is checked before any argument
reaches the command, so the command never runs and nothing is read or written.

A path the command writes to (the `backup` destination, the `backup restore`
archive and destination, the `drill` archive and scratch destination) is
refused when it starts with `-`, with a usage error naming the token, so a
mistyped flag cannot become a directory on disk. A path that really starts
with `-` is spelled `./-name`.

What `--help` does not carry is the behavioural boundary: not which flags a
command rejects, but what it will never do regardless of how it is called.
That boundary is what decides whether a command is safe to put in a timer, a
script, or a human's muscle memory, and it is what this document adds. Every
shipping command has a section below; a section that only restated `--help`
would not be worth a second document.

Test hooks (`__logging-fixture`, `__state-check`, `__exit-class`,
`__attempt-crash-hook`, `__projection-crash-hook`) are
not part of the shipping surface and have no section here, matching `--help`,
which does not list them either.

## `aub status`

**Answers:** how much quota does each configured account have left?

**Refuses:** the network, a write, and SQLite by default. `status` reads the
last published projection file and nothing else, so it never blocks on a
provider and never contends with a concurrent sampler for the store. It exits
non-zero only for an argument-parsing failure; a stale reading, an
auth-required account, or a missing projection are answers, not errors, and
all render with exit 0 so a status bar never treats degraded output as process
failure.

`--refresh` asks the command to take one forced sampling attempt per selected
account before rendering, through the forced sampling path, and
then to render the grid from the projection that pass published. Without the
flag no attempt is ever taken, so the default stays a read of the ledger. An
account whose refresh attempt fails is not an error: it renders its last known
reading with its age, which is the fact an operator comparing against a live
tool needs.

`--session-id SESSION` additionally reads that session's explicit account
markers and heartbeat without taking a sampling attempt. When the evidence
supports a live claim, status prints the same `spending` line as the former
live command; conflicting, absent or inactive evidence prints the corresponding
typed activity state. Without `--session-id`, the JSON document has no
`activity` key and text output has no activity line.

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
line that also names how old the observation behind the reading is, over one
row per quota window. The row columns are a fixed width: the
window label (`5h`, `week`, or a model display name), a 30-cell bar that fills
as quota is *used*, the percent used, the burn rate (`1.0x` is on pace to hit
the cap exactly at the reset), then the reset in local time with any note.

```
QUOTA                                                       Mon 07 Sep 14:22 -03

anthropic ──────────────────────────────────────────────────────────────────────
  primary  anthropic · observed 2m ago
    5h       ━━━━━━━━━━━━━━━━━━━───────────   62%   1.19x Mon 17:00
    week     ━━━━━━━━━━━━──────────────────   41%   0.95x Fri 09:00
    fable    ━━━━━━━━━━━━━━━━━━━━━━━━━━────   88%   2.04x Fri 09:00

  gmail  anthropic · observed 2m ago
    5h       ━━━━──────────────────────────   12%   0.23x Mon 17:00
    week     ━━────────────────────────────    5%   0.12x Fri 09:00
```

A stale account dims its whole block and each row's note reads
`cached <age> ago  <reason>`, the same age the header names; an
auth-required account shows `auth!` in place of the grid; an unreadable
projection is the bare `aub ?`. `--format json` carries every window under
`accounts[].windows[]`, not only the limiting one, and the reading
observation's age as the machine-readable `observation_age_nanos` beside the
freshness variant (schema v5; a reading with no observation behind it omits
the field). `--model NAME` narrows both the grid rows and the reading to the
account-wide and named-model windows.

## `aub now`

**Deprecated:** `aub now` is a deprecated alias and will be removed in a later
release. It writes
`aub: 'now' is deprecated and will be removed; use 'aub status --refresh' instead`
to stderr, then runs `aub status --refresh` with the same arguments. Its stdout
and exit code are the status command's output and result.

**Refuses:** no independent report or sampling path; use `aub status` for the
cached projection and `aub status --refresh` for a forced sample.

## `aub spend`

**Answers:** how many canonical tokens were used, grouped by the requested
dimensions? `--group-by` takes `day`, `session`, `project`, `repository`,
`harness`, `model`, `task` or `account` and is repeatable, so
`--group-by account --group-by day` nests days under each account. `harness`
is the transcript namespace the config named (`agy`, `claude-code`, `codex`, `pi`,
`opencode`) and `model` is the model id the transcript stored; usage neither
field names lands in the `unknown-harness` or `unknown-model` bucket.

Text output is a bordered table. Counts use compact human units in the table;
`--format json` and `--value api-list` print exact integers.

```text
┌─ spend · 2026-09-01 → 2026-09-07 · 7 days UTC · by day ──────────────────────┐
│                                                                              │
│  day         input  output  cache read  cache write  reasoning               │
│  ────────────────────────────────────────────────────────────────            │
│  2026-09-01   2.1M  310.0k       38.2M       900.0k      12.0k               │
│  2026-09-02   3.7M  373.6k       75.1M         1.4M      23.2k               │
│  2026-09-03   3.7M  373.6k       75.1M         1.4M      23.2k               │
│  2026-09-04   3.7M  373.6k       75.1M         1.4M      23.2k               │
│  2026-09-05   3.7M  373.6k       75.1M         1.4M      23.2k               │
│  2026-09-06   3.7M  373.6k       75.1M         1.4M      23.2k               │
│  2026-09-07  11.0M  722.0k      198.5M         3.3M      82.0k  ◐            │
│  ────────────────────────────────────────────────────────────────            │
│  total       31.4M    2.9M      612.0M        11.2M     210.0k               │
│                                                                              │
│  ◐ partial: coverage incomplete for this row                                 │
│  1085 events · 8 files read · 0 quarantined · generation 148                 │
└──────────────────────────────────────────────────────────────────────────────┘
```

A count under 1k is the exact integer, under 1M it is thousands with one
decimal, and above that millions with one decimal; a row and the total print
the same value the same way. The box uses the terminal width from the style
layer, with an 80-column minimum and a 120-column maximum. When the table is
wider than the narrower of the terminal and the box, it drops `reasoning` first
and then `cache write`, says which columns were hidden in the footer, and cuts
a key that still does not fit with `…`. Nested groupings get an accent section
title naming the first dimension and a table per outer group whose rows join
the remaining dimensions' values with ` · `. Detail lines and footer notes wrap
inside the box. `◐` marks a row whose coverage is incomplete.

Every dimension except `day` has a filter flag of the same name: `--harness`,
`--account`, `--project`, `--repo`, `--task`, `--model` and `--session`, where
`--repo` is the flag for the repository dimension and the `--group-by
repository` spelling keeps `repo` as its alias. Each flag is repeatable and its
values are OR-ed; different flags combine with AND. A filter that would hide an
`unknown-*` bucket does not hide it silently: every active filter reports, in
the footer, how many sessions and events it excluded and how many of those were
in the dimension's `unknown-*` bucket, one line per filter in the order the
flags were given, for example `excluded: 3 sessions, of which 2
unknown-account (via --account)`. The JSON carries the same record per filter
under `filters[]`.

The window is read the way people say it (UTC days, end exclusive):

| flags | window |
| --- | --- |
| none, `--today` | today |
| `--yesterday` | yesterday |
| `--days N` | the N days ending today, today included |
| `--since D` | D up to and including today |
| `--since D --until E` | D up to E exclusive; `E <= D` is a usage error |
| `--since D --days N` | D forward N days, the reading an explicit start keeps |
| `--until` without `--since` | usage error naming both flags |

`--yesterday` stands alone and does not combine with the other window flags,
and the window line states the resolved dates every time.

Account grouping is the session-identifier join: the session id already appears
in every transcript and in every account marker, and `--group-by account` reads
both. Attribution is decided by the marker-interval segmentation, never by this
command; usage no marker can justify lands in the `unknown-account` group, which
is reported as its own partial group rather than merged or dropped. `--explain`
on an account group names the exact markers behind the attribution and their
evidence class.

Two rules read the account off an event's stored model id instead, and each
replaces the marker attribution for the events it matches:

- **The provider prefix.** Two prefixes ship built in: `opencode-go/*` is
  account `opencode-go`, the paid OpenCode Go plan, and `opencode/*` is account
  `opencode-free`, the free Zen models. A marker cannot make this split,
  because one opencode session can switch provider between two messages, and a
  marker only covers the events after it.
- **The key slot.** An id whose `[[models]]` rule (or built-in vendor) still
  matches once a trailing `-k1`, `-k2` or `-k3` is stripped reports account
  `<vendor>-<slot>`, so `glm-5.3-flash-max-k2` under a `glm-5.3-flash*` rule
  for vendor `ollama` is account `ollama-k2`. `kimi-k3` is not a slot, because
  its stripped `kimi` matches no `kimi-k3*` rule.

The prefix is consulted first, so an opencode id ending in `-k2` stays under its
plan. Both rules are applied when the report runs, over ids already in the
ledger, so they cover events recorded before they existed. Ids neither rule
matches keep their marker attribution.

Valuation is keyed by the vendor and model the `[[models]]` table resolves from
each event's stored model id, never by the harness the transcript came from.
`--explain` names that pair per group, and a footer line names any id nothing
priced; both are documented under `aub config`.

A card can carry a time-of-day schedule (`schedule = { days = [...],
hours_utc = "12:00-18:00" }`, weekdays as `mon` through `sun`, the window
inside one UTC day), and each event values at the card in force at its own
instant: a weekday afternoon lands on the peak row, the same hour on Saturday
on the default. `--explain` names each card a group used with its schedule
(`peak mon-fri 12:00-18:00 UTC` or `default`). An event whose timestamp is a
heuristic values at the default card and its group reads estimated with the
schedule-unresolved method, so an unknown hour never silently becomes a peak.

`--window-equivalent five_hour` (or `seven_day`) adds each group's usage as
percentage points of that quota window. A current calibration for the
provider and window always answers first, and its line names the calibration.
When no calibration is recorded for that provider and window at all, a
percent-of-window rate card (see `aub rate-card`) answers instead, and the
figure is followed immediately by `(estimated)` and the ids of the cards
used:

```text
window equivalent [0.85, 0.85] percentage points (estimated) from rate card 7
```

In JSON the estimated figure carries `evidence_quality: "estimated"`,
`methods: ["rate-card-estimate"]` and `basis: {"kind": "rate_card_estimate",
"rate_card_ids": [...]}` in place of `calibration_id`. A calibration that is
recorded but not current (`review_due`, `suspect`) does not fall back: the
command refuses naming the health, because a stale measurement is still
evidence and an estimate must not paper over its review. A calibration is
`review_due` once `calibration.review_after` has passed since it was fitted,
whether it is a scalar calibration or a per-kind one. Every refusal for a
stored calibration that is not current exits 6 (`InsufficientEvidence`) after
the report is printed, whatever made it not current: a passed review, a
superseded cost model, a calibration whose semantics do not match the active
cost model (`inapplicable`), or a per-kind calibration past review. `aub can-run`
does the same when a constraining window's calibration, scalar or per-kind, is
not current, and its error names every such window. `aub calibrate show` and
`aub calibrate history` label a calibration with the same health these
commands refuse by. With neither a calibration nor an estimate card the refusal
names the missing calibration, and the exit stays 0.

**Refuses:** to guess at an unreadable transcript. A source that cannot be
normalized leaves the report `IngestIncomplete` rather than silently omitted
or extrapolated from what did parse. `spend` also refuses to answer a quota
question; `status`, `now` and `sample` own that, and refuses to forecast a
cost that has not happened yet. It also refuses to price a model id nothing
maps: an unmapped id is reported in the footer, never valued against whatever
card would otherwise have matched.

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

Headroom follows the same precedence as `aub spend --window-equivalent`: a
current calibration answers, a window with no calibration recorded at all
takes the rate-card estimate and its headroom line ends in `(estimated)`
with the cards named, and a calibration that is recorded but not current
refuses naming its health. The JSON window carries `basis.rate_card_ids` and
`evidence_quality: "estimated"` in place of `calibration_id`.

**Refuses:** to guess. Any of seven missing prerequisites, a stale meter,
authentication required, a constraining window with no applicable current
calibration, a cost model missing a token class, a plan tier mismatch, too
few historical tasks, or mostly unattributable task records, produces a
refusal naming every one that applies in the same invocation rather than the
first one found, and a refusal exits `0` with the refusal rendered, not a
usage error, except that a refusal a not-current stored calibration took part
in exits `6` (`InsufficientEvidence`) after the report. It never substitutes a global average task cost, a different
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
│      opencode_workspace wrk_golden                                           │
│      plan_tier max-20x                                                       │
│    work-secondary  codex       env:AUB_GOLDEN_TOKEN                    file  │
│      exclusivity_policy permit_passive                                       │
│      codex_home /tmp/aub-golden/codex-home                                   │
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
│    scheduled                true                                    default  │
│                                                                              │
│  calibration                                                                 │
│    review_after             30d                                     default  │
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
│  export                                                                      │
│    clipboard_command        wl-copy                                 default  │
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
│    auth_backoff_cap         6h                                      default  │
│    auth_backoff_threshold   3                                       default  │
│    busy_timeout             10s                                     default  │
│    command_budget           30s                                     default  │
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
from the default, followed by dim sub-rows for any configured optional keys
(`opencode_workspace`, `codex_home`, `plan_tier`); `transcripts` prints one
line per source with the `pattern` (and any `usage_evidence`) on dim lines
under it. The source column reads `default` in dim and `file`, `override` or
`environment` in body text, so the keys the operator set stand out from the
ones they did not. Values longer than the room to the source column end in
`…`. A `--set key=value` override prints with source `override`.

**Refuses:** to invent a value for a key nobody set. An unset key prints with
source `default`, never a value that looks like it came from a file. It also
never prints credential material, only the kind and reference (`file:<path>`,
`env:<NAME>`, `none`) of the key that names one.

`calibration.review_after` (default `30d`) sets how long a window calibration
stays current after it was fitted: the review instant is the fit time plus
this horizon, and a calibration past it becomes `ReviewDue` instead of
`Current`. It takes the same duration syntax as `backup.review_after`.

#### The sampling schedule, and why its defaults are what they are

Four keys describe one schedule, and they constrain each other. `aub config`
refuses to resolve a combination the sampler could not deliver, naming both
keys and both values:

| rule | why a violation is impossible rather than merely tight |
| --- | --- |
| `sampling.reset_edge_lead` > `sampling.scheduler_tick` | the lead is the window a reset-edge attempt is owed in, and due-ness is only evaluated once per tick, so a lead no longer than a tick has a window two consecutive ticks can straddle. The pre-reset reading is lost with no failure recorded |
| `sampling.default_interval` >= `sampling.scheduler_tick` | a scheduler that wakes on the tick cannot deliver a shorter cadence. It degrades to the tick silently, so every coverage denominator computed from the recorded cadence over-counts what was ever owed |
| `freshness.meter` > `sampling.default_interval` + `sampling.scheduler_tick` | a reading goes stale after the horizon, and the soonest a replacement can arrive is one cadence plus up to one tick. A shorter horizon reports a stale meter in steady state while the sampler is working |

The defaults clear all three with room: a 1m tick, a 5m cadence, a 2m lead and
a 12m horizon against a 6m minimum.

Each of those values, and the two coverage floors, was re-examined against the
unattended burn-in recorded on `aub-eun.10` (2026-09-07 to 2026-09-14, ten real
accounts) plus the series that has run since. Every one was retained, and the
evidence for each is on its field in `src/config/mod.rs`. The short form:

- **`scheduler_tick` 1m, `default_interval` 5m.** Nine accounts with working
  credentials delivered 288 to 293 attempts a day each over twelve days, and
  only four to ten inter-attempt gaps per account exceeded 12 minutes in the
  whole series. Nothing asks for a faster cadence, and a faster one spends
  provider requests against rate limits the burn-in already saw bite.
- **`reset_edge_lead` 2m.** On the three accounts whose reported reset instants
  are stable enough to judge a lead by, the newest observation preceding a reset
  was inside 2 minutes for 3,646 of 3,647, 1,935 of 1,937 and 6,976 of 7,016
  reset instants. The accounts that miss that bound report reset instants faster
  than any cadence can attend to, which no lead can fix.
- **`freshness.meter` 12m.** The share of wall-clock time with no reading newer
  than the horizon moves only from 0.53% at 10m to 0.49% at 12m to 0.42% at 15m:
  staleness comes from rare hour-long provider outages, not from cadence jitter,
  so nothing in the plausible range distinguishes itself on frequency. 12m is
  kept because it is the value that absorbs exactly one fully missed cadence.
- **`coverage.attempt_floor` 0.98 and `coverage.measurement_floor` 0.95.** Over
  the six 168h windows ending 2026-09-14 through 2026-09-19, ordinary operation
  measured between 98.6% and 99.8%, while the one genuine provider episode in
  the series drove a single account to 92.3%. The floors sit in the empty band
  between those two modes rather than under the worst run.
- **`request_timeout` 5s and `command_budget` 30s** were settled on `aub-fhh9`
  and `aub-rqh2` out of this same burn-in. `request_timeout` bounds the ledger
  connection and the archive verifier, never a provider request. `command_budget`
  matches the 30s budget the adapters build: of 13,404 successful attempts after
  the change landed, four exceeded the 8s horizon it used to carry, the slowest
  at 12,037 ms.

A changed value applies prospectively. It is written into a new
`sampling_policy_snapshot` row with its own effective instant, and the coverage
engine reconstructs each interval's denominator from the snapshots in force
inside that interval, so no earlier expectation is rewritten.

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

### `[[models]]`: which vendor and model prices an event

The vendor of a usage event is a property of the model, never of the harness
that recorded it: one harness runs models from several vendors, and a litellm
alias such as `deepseek-v4-pro-high-k1` reaches Ollama Cloud, which the string
`pi` cannot say. `[[models]]` maps the model id a transcript stores onto the
vendor and model the rate book is keyed by:

```toml
[[models]]
pattern = "deepseek-v4-pro*"
vendor = "ollama"
model = "deepseek-v4-pro"

[[models]]
pattern = "glm-5.3-flash*"
vendor = "ollama"
model = "glm-5.3-flash"

[[models]]
pattern = "glm-5.3*"
vendor = "ollama"
model = "glm-5.3"
```

`pattern` is a glob over the stored id, where `*` matches any run of characters
and `?` exactly one. Matching is case sensitive and there are no character
classes, braces or escapes.

**The first rule that matches wins, and the order is the file's own.** The
example above works only because `glm-5.3-flash*` is listed before `glm-5.3*`,
which also matches `glm-5.3-flash-max-k2`. A rule an earlier one already covers
entirely can never fire, so it is rejected naming both patterns rather than
sitting in the file looking effective. This is why the section is an array of
tables rather than a keyed table: TOML guarantees no order among a table's
keys, so an order written that way is not an order at all.

`model` is what the rate book is looked up under, and it is not always the
stored id: the alias encodes the reasoning effort (`-high`, `-max`, `-xhigh`)
and the upstream account (`-k1`, `-k2`, `-k3`), and neither changes the price.
Reports keep naming the stored id; the resolved pair is a second field,
`priced_as`, shown under `--explain` and in the JSON of each spend group.

After the configured rules come the built-in vendors, which every id identifies
on its own and which carry the id through unchanged:

| pattern | vendor |
| --- | --- |
| `claude*` | `anthropic` |
| `gpt*`, `o?`, `o?-*` | `openai` |
| `opencode/*`, `opencode-go/*` | `opencode` |

A configured rule outranks a built-in, so a model proxied somewhere else is not
overruled by its prefix.

An id that matches nothing at all is unmapped. It is never valued against a
card that happened to sort first: no rate matches, and `aub spend` prints a
footer line naming the ids and how many events carried each.

```
unmapped models: 412 events (kimi-k2.7, minimax-m3-max-k3)
```

That line is how a new alias becomes visible the day it first appears. An event
whose transcript recorded no model id at all is counted under `(no model id)`.

### `[layout]`: which repository a checkout belongs to

Two roots describe the whole checkout layout instead of one alias per
checkout:

```toml
[layout]
repositories = "/home/user/repositories"
worktrees = "/home/user/repositories/.worktrees"
ignore = ["scratch"]
```

`repositories` makes every immediate child directory a repository named after
that directory (`/home/user/repositories/aub/src` resolves to `aub`);
`worktrees` makes `<dir>/<repo>/<anything>` resolve to `<repo>`
(`/home/user/repositories/.worktrees/aub/bugfix-x/src` resolves to `aub`);
`ignore` sends the named repositories to the unknown bucket on purpose (a
`scratch` checkout stays inside totals as `unknown-repository` and
`unknown-project` rather than disappearing from them).

Precedence for a working directory, in order:

1. An explicit alias with an exact key wins (`[repositories]` for the
   repository, `[projects]` for the project).
2. The `worktrees` root, checked before the repositories root because it sits
   inside it on this machine.
3. The `repositories` root.
4. Unknown.

Under a root the identity is the first path segment after it; a directory
equal to a root itself resolves to unknown. A project is its repository
unless an explicit `[projects]` entry with an exact key says otherwise, which
is how a project spanning repositories is expressed. A dot-named segment
(such as `.worktrees` from a misconfigured root) is refused at resolution and
reported once per ingest in the summary, so a misconfigured root is visible
rather than silently producing dot-named repositories. Both roots are
optional; with neither set, behaviour is exactly today's exact-match aliases:
the project resolves through `[projects]` alone, and a `[repositories]` alias
names only the repository.

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

`aub export transcript <id>` renders one session's transcript as markdown
from the ledger's own knowledge of where every transcript lives: the id
resolves against the `session` table (full or prefix, case-insensitive,
across every harness unless `--harness` narrows it), the files are read from
the `usage_occurrence.source_file` paths recorded at ingest, and the
conversation comes out as `## User` / `## Assistant` sections with, on
request, tool calls and results untruncated (`--include-tools`) and thinking
blocks (`--include-thinking`).

A prefix matching more than one session fails, listing every candidate with
harness, start time and project, and `--latest` renders the most recent of
them; choosing silently was rejected because the wrong transcript reads as a
plausible session. A claude-code subagent transcript
(`<session>/subagents/agent-*.jsonl`) renders after the parent under a
`## Subagent <file>` heading. Claude-code, codex and pi sessions render; a
harness with no renderer yet (opencode) fails with `no transcript renderer
for harness '<name>'`, never with an empty document. Lines the codex or pi
renderer has no case for are counted and reported once per type on stderr as
`skipped: N lines of type X`, never dropped silently and never fatal.

Four destinations: `-o` with no path writes
`~/agent-transcripts/<YYYY-MM-DD>-<project>-<uuid8>.md` and prints the path;
`-o <dir>` uses that directory with the same name; `-o <file>` writes that
file; `-p` writes only the markdown to stdout. `-c` pipes the markdown to
the command named by `export.clipboard_command` (default `wl-copy`).

## `aub cost-model`

**Answers:** which published cost model prices usage into credits, and which
one is active right now?

`cost-model list` prints one line per published model:
`<model-id> active since <RFC 3339 instant>` for the one an activation
event names, `<model-id> inactive` for the rest. Two models are published:
`anthropic-claude-messages-v1`, the complete rate structure, and
`anthropic-claude-messages-incomplete-v1`, which is published with its
cache-write term deliberately removed so the missing-rate refusal is
reachable and testable without editing anything.

`cost-model activate <model-id>` records the lifecycle event that makes one
published model active: an `activation` when nothing was active before, a
`supersession` naming the displaced model otherwise. Activating the model
that is already active prints `already active`, exits 0, and writes
nothing, so the command is safe to re-run in a script. The underscore
spelling of a model id (`anthropic_claude_messages_v1`) names the same
stored model as its dashed id.

**Refuses:** any id that is not a published model, naming the known ones;
and any subcommand other than `list` or `activate <model-id>`. Activation
is never inferred: `spend --credits` refuses to price usage while no model
is active, and `aub doctor`'s `cost-model-active` check warns with this
command's name when rate cards are imported but none is, so the one missing
step is always stated, never guessed.

## `aub rate-card`

**Answers:** what do the immutable dated vendor rate cards contain?

The book to import is `rate-book/rates.toml`, the sourced dated book this
repository ships: `aub rate-card import rate-book/rates.toml`. Every row there
names the page it was read from and the day it was read, and the file's header
carries the rules it was written under. The book under
`tests/fixtures/rate-book/` is test data for the import contract and is
imported by the test suite alone; it is not a price this machine is billed at.

A book with two cards that could price the same instant is refused at import,
naming both card indexes: at most one unscheduled card per vendor, model,
class and date, and no two scheduled cards whose day sets intersect and
whose hour ranges overlap. A default beside its peak rows always passes.

A card can also state a subscription price as percentage points of a quota
window per million tokens, and only as a declared estimate:

```toml
[[card]]
vendor = "anthropic"
model = "claude-fable-5"
token_class = "input"
billing_basis = "percent_of_window_per_million_tokens"
window = "five_hour"          # or "seven_day"
rate = "0.85"                 # 1M input tokens move the window 0.85 points
unit = "percentage_points"    # in place of currency
quality = "estimate"          # the only value this basis accepts
source = "where the figure came from"
effective_start = "2026-09-01"
```

The importer refuses such a card when it carries a `currency`, lacks
`window`, lacks `quality` or states anything but `estimate`, or lacks
`source`, and refuses `window`, `unit` or `quality` on a
`per_million_tokens` card; each refusal names the card index and the field.
These cards are never money and never pass through the cost model. They are
read only by `aub spend --window-equivalent` and `aub can-run`, and only while
no calibration is recorded for that provider and window; every figure they
produce is labelled estimated. Retiring one is an `effective_end`, like any
card.

**Refuses:** to edit history. A rate book is imported into a new, immutable,
versioned record; correcting a stale price means importing a new version,
never mutating one already on record.

## `aub backup`

**Answers:** is there a consistent, verified archive of the durable state,
and does it restore?

Usage: `aub backup [--scheduled] [DESTINATION]` writes a new dated archive under the
destination root (the explicit argument wins, otherwise
`backup.destination`); `aub backup verify DESTINATION` re-checks one
archive; `aub backup restore ARCHIVE DEST` recovers from one archive.
Each run keeps a series of dated archives under tiered retention
(`backup.keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly`); an
archive is retained when any bucket keeps it, and the most recent verified
archive is never pruned.

`aub backup --scheduled` is the form the shipped timer and cron entry call
(`docs/scheduling.md`). It reads `backup.scheduled` (default `true`): with
the key set to `false` it prints one line naming the key and exits zero
without writing, and with no `backup.destination` configured it prints one
line saying backups are not configured and exits zero. A manual `aub backup`
ignores the key. Every other failure keeps its usual exit class, so the
unit's `OnFailure=` hook fires.

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
external scheduler, never by a loop inside `aub`. `begin` refuses a one-kind
premise with no cost model or with a named cost model that carries no term for
the expected kind, a `--cost-model` naming no stored model, an account with
no sampled baseline yet, and an account that already runs an experiment;
`end` records the end of controlled work and never declares the meter
settled. `fit` and `passive` refuse to activate candidate calibrations automatically:
candidates are written immutably and never promoted to active status by the
fitter (Invariant 14).

`begin` takes the run's plan tier from the account's configuration: the
`plan_tier` key on the named `[[accounts]]` entry. `--plan-tier` is optional:
omitted, the configured tier is recorded and printed; given with the same
value it passes as a cross-check. Two mismatches are refused with the usage
exit class before anything is recorded: a flag that disagrees with the
configured tier, naming the account and both tiers, and an omitted flag with
no tier configured for the account, naming the account and the `plan_tier`
key. With no configured tier the flag records its value as before. Both the
configured tier and the flag are trimmed, so surrounding whitespace never makes
two identical tiers disagree.

`begin` requires `--cost-model` only for a one-kind premise. A premise naming
two or more token kinds fits jointly straight from the recorded usage counts
with no rate book in between, so it begins with no `--cost-model` and stores
`none`; a model named on such a premise is recorded as given without the
expected-terms check. A premise naming one kind fits univariately through the
rate book and still requires `--cost-model`, and a named model missing a term
for the expected kind is refused as before. The `begin` report line prints
`expect_kinds` alongside `cost_model`, so the recorded premise is visible.

`fit` follows the experiment's premise. `fit --experiment ID` naming a controlled
experiment whose `--expect-kinds` premise names two or more token kinds fits them
jointly, one coefficient per named kind, regressed directly on the recorded
`usage_component` counts with no rate book in between; a premise naming one kind,
or an experiment with no controlled premise, takes the univariate path and its
output is unchanged. The joint fit reads the observations in settled blocks: the
readings between one usage event and the next form one block whose quota delta is
taken at the last reading before the next spend, so a meter that lags is read after
it caught up rather than on the reading that followed the spend. A design whose
kinds cannot be separated is refused before anything is written: the message names
the collinear pair, its correlation, and the condition number against the bound
(30, the Belsley, Kuh and Welsch threshold), the exit status is the
insufficient-evidence class, and `calibrate show` reports no new candidate. Either
outcome closes a controlled burst; the refusal is itself the answer to whether the
arms separated. Two more outcomes sit beside those. A kind whose count was the same
in every usable block is refused before any coefficient is computed, naming the kind
and its token count and saying its column never varied, because through the origin
such a column is an intercept and fits to an arbitrary number of either sign. A kind
that costs nothing is recorded, not refused: its coefficient is reported as fitted,
at or a hair below zero, and a fit is refused for a coefficient's sign only when its
estimate plus two standard errors is below zero, a message naming the kind, the
estimate and the error. The joint fit reads only the usage the session account
markers place on the run's own account; usage from other accounts' sessions in the
same window never enters a block, and usage no marker places on any account is left
out and listed under `excluded_samples` by session. A run must have recorded `end`
before it can be fitted. The last block has no next spend to close it, so the fit
reads no reading taken after the settlement bound: `end` plus the post-settlement
grace recorded at `begin` (one hour by default), or, when the markers place usage on
the run's own account after `end` and inside that grace, the instant before the
first such event. Readings between `end` and the bound still settle the last block;
anything past it measures the window after the run and changes neither the blocks
nor the inputs digest. Usage no marker places on any account does not move the
bound. With
`--format json` the joint fit carries `fit_kind` (`"multivariate"`), `token_kinds`
(the premise, in stable order), `coefficients` (one object per kind with
`token_kind`, `estimate_ppm_per_token`, `std_error_ppm_per_token`,
`interval_low_ppm_per_token`, `interval_high_ppm_per_token`), `condition_number`,
`condition_number_threshold`, `condition_number_micros` (the figure as stored),
`pairwise_correlations` (`first`, `second`, `correlation`), `fit_residual_ppm` (the
mean absolute block residual), `residual_percentage_points`, `statistical_method`,
`statistical_parameters`, `phase_design`, `usable_observations`, `sample_count`,
`inputs_digest`, `inputs_count`, `excluded_samples`, `contamination` (below), and
`activated` (always `false`). The candidate and its coefficients live in
`window_calibration_multivariate_candidate` and
`window_calibration_multivariate_coefficient`, immutable like every other
calibration record (Invariant 29). `activate` takes `--max-condition-micros` and
refuses a recorded condition number over it, naming both figures.
`promote CANDIDATE --training E,... --validation E,...` is the step between a
fitted candidate and an active calibration: it records a `window_calibration_result`
from the candidate, supplying the validation half a candidate does not carry. The
coefficient, its uncertainty and its sample count are the candidate's own, and
promotion never refits them; what it adds is the held-out residual computed over
the validation evidence, the two evidence fingerprints `activate` reproduces, the
validation method and version, the settling policy of the source experiment, and
the activation policy version (`promote-v1` unless `--policy-version` names
another), so `activate` with no `--policy-version` judges the result under the
policy the promotion recorded. The result's id is `promoted-<candidate-id>`,
derived rather than generated, because one fit has one identity.

`--training` names the evidence the candidate was fitted from, and the command
refuses a set whose digest is not the candidate's own, naming both digests: the
row would otherwise claim to have been fitted from evidence it was not. The
validation evidence is held out of the fit, so it lies outside the source
experiment's validity window; `--validation` is refused when it is empty, when it
overlaps the training set (the overlap is named), and when the ledger holds no
observation of the experiment's provider and window for one of its ids. Promoting
one candidate twice is refused naming the result already recorded, since a result
is immutable and a second row would be a second identity for one fit.

A joint multivariate candidate is promoted to a per-kind result in
`window_calibration_multivariate_result`, with one row per kind the premise named
in `window_calibration_multivariate_result_coefficient`. Its coefficients are
never reduced to one credits-per-point scalar, because pricing them through a
cost model would record as truth the assumption the joint fit exists to test. The
result carries the candidate's coefficients with their standard errors and
intervals, its condition number and the threshold it was accepted under, its fit
residual, and a held-out residual: the validation readings are folded into
settled blocks over the run account's own usage, exactly as the fit folds its
readings, and the result records the mean absolute distance between each block's
movement and the movement the coefficients predict, in ppm of quota. Validation
readings that hold no block of the run account's usage in a fitted kind are
refused, since there is no movement to predict. The training, overlap,
missing-evidence and second-promotion refusals are the scalar ones. With
`--format json` the report carries `calibration_id`, `coefficient_shape`
(`"per_kind"`), `candidate_id`, `experiment_id`, `provider`, `plan_tier`,
`window_semantic_key`, `coefficients` (one object per kind with `token_kind`,
`estimate` and `std_error` in `micro_ppm_per_token`, and `interval`),
`condition_number`, `condition_number_threshold` (both in `micros`),
`fit_residual`, `held_out_residual` (both in `ppm`), `validation_observations`,
`sample_count`, `statistical_method`, `statistical_parameters`, `phase_design`,
`validation_method` (`held-out-block-residual`), `validation_version`,
`inputs_digest`, `inputs_count`, `fitting_evidence_digest`,
`validation_evidence_digest`, `fit_timestamp_nanos`, `activation_policy_version`,
`fitter_version`, `source_revision`, and `activated` (always `false`). There is
no `fitted` field.

Promotion records evidence and never activates (Invariant 14): it writes no
`calibration_lifecycle` row, `calibrate history` lists the new result with no
lifecycle event, and the operator activates it explicitly afterwards. With
`--format json` the report carries `result_id`, `candidate_id`, `experiment_id`,
`provider`, `plan_tier`, `window_semantic_key`, `fitted`, `fit_residual`,
`held_out_residual`, `validation_observations`, `fitting_evidence_digest`,
`validation_evidence_digest`, `validation_method`, `validation_version`,
`activation_policy_version`, `uncertainty`, and `activated` (always `false`).

`fit` on a controlled run reports the run's contamination verdict, on the
univariate and the joint path alike; a fit of an experiment with no controlled
run reports none, because only a run records the exclusivity premise and the
thresholds a verdict is judged against. The verdict is evaluated from the ledger
when `fit` runs, with the four signals `begin` recorded thresholds for: quota
moving inside the idle plateau before the run (`pre_burn_idle_movement`), quota
still moving past `end` plus the settlement grace (`extended_settlement_drift`),
meter movement while the run's own account spent no priced credits
(`flat_credits_with_meter_movement`), and another session marked against the
run's account inside the run (`overlapping_session`). The local credits are the
run account's usage between `begin` and `end`, priced under the cost model in
force at `begin`. Readings are read up to the instant before the window the
run ended in resets, since a reading after that reset measures the next window
and its drop is not settlement drift. With `--format json` the report carries
`contamination` with `verdict` (`"clean"`, `"contaminated"`, or `"unavailable"`
when no cost model can price the run's local credits, a joint run in a ledger with
none, with `reason` saying why), `findings` (one object per fired signal with
`signal` and `detail`), and `refuses_activation`. The text report prints a
`Contamination:` line with the verdict and one line per finding. A contaminated
verdict never stops the fit from recording its candidate.

`activate` of a result fitted from a controlled run evaluates the same verdict
at activation time and refuses, before anything is written, with the
contaminated-run refusal naming the run and the first fired signal, when any
signal other than `overlapping_session` fired. An overlapping session alone is
reported by `fit` and does not refuse: the marker timeline cannot tell the
run's own arm sessions from another's, and every session a burst opens on the
account is marked inside the run. A run whose local credits cannot be priced is
refused with the insufficient-evidence class. A result whose source experiment
is not a controlled run is not judged for contamination.

`activate` of a per-kind result goes through the same gate: the recorded policy
version and evidence digests, disjoint training and validation sets, the source
run's contamination verdict, `--max-condition-micros` against the recorded
condition number, and the held-out residual against `--max-residual-ppm` (ppm of
quota, 10000 by default, the one-percentage-point resolution providers report
usage at). `--max-residual-micros` bounds a scalar result's residual in credits
and never judges a per-kind one. Its event is written to
`calibration_multivariate_lifecycle`, and the active calibration of a scope is
the latest event across that table and `calibration_lifecycle`, so a per-kind
activation supersedes an active scalar calibration and names it. A scalar
activation over an active per-kind calibration is refused naming
`aub-scalar-supersedes-per-kind-ufpq`, because `calibration_lifecycle` cannot yet
name a per-kind predecessor. `calibrate show` and `calibrate history` list
per-kind results under `per_kind_entries`, beside the scalar `entries`, each with
its `health`, `is_active` and `events`; `calibrate compare` refuses a per-kind id
by name. A consumer that converts credits into percentage points, `spend
--window-equivalent` and the `can-run` headroom, reports the window as needing a
scalar calibration rather than falling back to a rate-card estimate, since the
window does have a calibration and it is not one credits can be read through.

Subcommand `passive` generates candidates from
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
`agy`, `claude-code`, `codex`, `opencode` or `pi`. An `opencode` source names the
directory holding its session database with the pattern `opencode.db`; the
database is opened read-only and parsed whole, never sliced by lines.

**Refuses:** to touch anything but rebuildable, transcript-derived rows. It
never writes or deletes a meter attempt, a response, an observation, a
calibration, or any other irreplaceable evidence.

**Progress:** while it runs, `aub ingest transcripts` writes one line per
progress report to stderr, whatever the log level, naming the phase it is in:

```
ingest transcripts: scanning files=100/5900 events=12345 elapsed=1s rate=98.2 files/s
ingest transcripts: deduplicating events=304544 elapsed=22s
ingest transcripts: writing batches=12/72 events=95000/304544 elapsed=52s rate=3113.9 events/s
```

- `scanning` prints every 100 files or 30 seconds, whichever comes first, and
  once more for the last file. `events` counts the usage events parsed so far,
  and `rate` is files read per second since the previous line.
- `deduplicating` prints exactly once, when the last file is parsed. It covers
  deduplication, session resolution and batch splitting, which print nothing
  else until the first batch is written.
- `writing` prints once when the first batch starts (`batches=0/N`, no rate),
  then after a committed batch whenever a second or more has passed since the
  previous line, and always after the last batch, which reads `batches=N/N`.
  `rate` is events committed per second since the previous line.

A line prints a rate only once time has passed since the previous line, and
every rate carries its unit. The two summary lines on stdout are unchanged.

## `aub rebuild`

**Answers:** can the transcript-derived materializations be rebuilt from
scratch while every irreplaceable record is left untouched?

**Refuses:** the same evidence `ingest` refuses to touch, structurally rather
than by convention: `rebuild` can only address rebuildable materialization
groups, so it has no code path that could delete a meter attempt, an attempt
result, response evidence, an observation, or a calibration even if asked to.

`aub rebuild sessions` is not a sweep: it re-resolves every stored session's
project and repository keys from its stored working directory through the
current `[projects]` and `[repositories]` alias tables and `[layout]` roots,
rewriting derived keys only. Bounds, run ids and every evidence table stay untouched, so a new
alias applies to history and not only to sessions ingested after it.

## `aub doctor`

**Answers:** is the recorded evidence healthy, and does the transcript
corpus still match its parsers?

`sqlite-and-schema-health` treats a durable meter-attempt start with no
terminal result as valid evidence, not as a missing or invented outcome. Once
the command budget stored with that attempt has elapsed, a passing check names
the count as collector interruption evidence. It never inserts a
`meter_attempt_result` row to make the ledger look complete.

`sampling-failure-counts` is reconciled from the complete account report after
each successful sampling tick. Each `(category, reason)` entry carries the
instant it last occurred as `last_seen_unix_nanos`. A tick carrying a failure
increments its count and restamps it to that tick's instant; a tick not
carrying it keeps the entry unchanged while its last occurrence is less than
24 hours old, and removes it once it is 24 hours old or older. The check
fails while any entry is inside the 24-hour window, naming each entry's
category, reason, count and age since its last occurrence, and passes when
none is. A failure from the night is therefore still on `doctor` the next
day, while a recovered fault stops reporting on its own. An entry without
`last_seen_unix_nanos` counts as already expired. The completed batch is
the authority rather than a ledger query because a due-lookup failure can occur
before the ledger contains a row from which to reconstruct it.

`account-in-auth-backoff` names every configured account the authentication
backoff is currently holding, with its trailing `auth_required` streak, the
hold the scheduler would apply past the last rejection, and how long ago the
account last observed anything. The verdict comes from the same delay function
the scheduler and the coverage engine call, so the three cannot disagree about
what backoff means. Like every check that is not a configured floor it reports
and does not gate: `aub doctor` still exits zero with it failing. The alarm
that rides an exit code is `aub coverage`, scheduled separately
(`docs/scheduling.md`).

`window-estimate-in-use` reports `INFO`, naming each provider and window
(`anthropic/five_hour`), while a window figure would come from a
percent-of-window rate-card estimate rather than a calibration. It is not a
failure: nothing is wrong while an estimate stands in. It passes silently
once a calibration exists for every such window, so the day the calibration
lands the line disappears.

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
