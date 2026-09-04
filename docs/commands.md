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

## `aub now`

**Answers:** how much quota does each configured account have right now?

**Refuses:** to answer from a cache. Unlike `status`, `now` always forces a
fresh sampling attempt first and renders the result that attempt produced;
there is no flag that fetches and discards, and there is no mode that reads
the last published projection instead of sampling.

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

## `aub config`

**Answers:** which configuration key resolved from where?

**Refuses:** to invent a value for a key nobody set. An unset key prints with
source `default`, never a value that looks like it came from a file. It also
never prints a credential value, only the fact and source of the key that
names one.

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

**Refuses:** to report verified without checking. Creating and verifying an
archive both run the same checksum, manifest and SQLite integrity checks, so
neither a fresh backup nor a re-check can claim `verified=true` on faith.
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

## `aub ingest`

**Answers:** have the transcript-derived tables been refreshed from the
transcripts on disk, under one generation?

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

**Refuses:** the network. `coverage` reads only local ledger history, so it
tells a dead scheduler apart from a live one that is failing on credentials
by what the ledger recorded, never by asking a provider directly.

## `aub import`

**Answers:** which legacy evidence is safe to import into the ledger?

**Refuses:** to import without a verified backup path named first with
`--backup VERIFIED_ARCHIVE`, and refuses a blanket scan: only the explicitly
named source is imported, never everything a directory happens to contain.

## `aub sample`

**Answers:** are configured accounts due for meter sampling, and what did
the endpoints observe?

**Refuses:** to fail merely because a remote call came back with an
authentication or transport failure. In its scheduled shape, durably
recording that outcome is success; the command exits non-zero only when it
could not persist or operate at all. A caller that wants the ordinary
live-source exit classes instead asks for them explicitly with
`--require-success`, which still records the same evidence first.
