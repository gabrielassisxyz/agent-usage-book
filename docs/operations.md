# Operating `aub`

This is the operator's entry point: everything needed to take a fresh
machine to a working sampling cadence and a verified backup, without reading
the source. Each step below is a summary; the linked document is where the
detail and the working examples actually live.

## 1. Install

Download the `aub` binary for your platform from the Releases page, or build
it from source with `cargo install --path .` (see
[README.md#install](../README.md#install)). Note its absolute path with
`which aub`: a systemd unit, a cron entry, and a compositor keybinding do not
read an interactive shell's `PATH`, so every example from here on names that
path explicitly.

A reinstall has to write the same path the unit names. `cargo install --path .`
writes to `~/.cargo/bin/aub`; if the unit was pointed at another install root,
for example `~/.local/bin/aub` from `cargo install --path . --root ~/.local`, a
later plain `cargo install` leaves the scheduler on the old build while every
shell and script that resolves `aub` through `PATH` runs the new one. Nothing
reports the split, and because any `aub` command applies pending migrations
when it opens the ledger, the newer binary can move the schema past the one the
scheduler runs. Pick one install root, pass the same `--root` on every
reinstall, and have scripts name the absolute path as well. A build that adds a
migration is worth running first against a copy of the state directory
(`AUB_STATE_DIR=<copy> aub account list` opens it, and so migrates it, then
`aub doctor` against the same copy reads the result).

## 2. Configure

Write `$HOME/.config/aub/config.toml` (or point `AUB_CONFIG_FILE` at another
path) with at least one `[[accounts]]` entry naming a provider and a
credential. [README.md#configuration](../README.md#configuration) has the
minimal shape and the resolution order; `PLAN.md`'s [configuration
sketch](PLAN.md#47-suggested-configuration-sketch) is a fuller illustrative
example. Confirm it resolved with `aub config`, which prints every key with
the source that won: a key still showing `default` when the file should have
set it means the file was not found at the path `aub` actually resolved.

## 3. Bring up the sampling cadence

Something external has to invoke `aub sample --due` on a cadence, and the
agent session that starts should mark which account it belongs to. The
example systemd and cron units, the example session-start hook, and the full
reasoning (including why a scheduled `sample --due` treats a remote failure
as durably recorded evidence rather than a cadence failure) are in
[docs/scheduling.md](scheduling.md). Follow its own "Bringing up a fresh
machine" walkthrough end to end; `aub status` moving off "never observed"
within one sampling interval is the signal the cadence is live.

### The fixed timeouts a sampling request runs under

The five provider adapters build their request timeouts inline as deliberate
constants, not from configuration -- settled on aub-fhh9 as option D
(2026-09-14), implemented in aub-rqh2. Every request an adapter sends
through the transport, local-file reads included, carries a connect timeout
of 5s, a read timeout of 10s, a total timeout of 15s, and a command budget of
30s. The seven production call sites:

- `src/meter/agy.rs`, the quota request build
- `src/meter/opencode.rs`, the quota page request build
- `src/meter/anthropic.rs`, twice: the status-line record file read and the
  usage endpoint
- `src/meter/codex.rs`, twice: the newest rollout file read and the usage
  endpoint
- `src/meter/ollama.rs`, the quota request build

Two `sampling` keys sit beside these constants and bound other things.
`sampling.request_timeout` is the store timeout: the ledger connection's
wait for the writer slot and `aub backup verify`'s read of an archive. It
does not bound a provider request. `sampling.command_budget` is the
projection's command horizon past which a resultless attempt is classified
as a collector interruption, defaulted to the same 30s so the horizon does
not fire while the adapter is still waiting; it bounds no request either.
Should a provider-request timeout ever need to be operator-tunable, that is
its own decision with a default sized from burn-in, not a new duty for
these keys.

## 4. Know what each command answers, and what it refuses

[docs/commands.md](commands.md) is the per-command reference: the question
each shipping command answers, and the behavioural boundary it never
crosses regardless of how it is called, such as `status` never touching the
network or `sample --due` never failing merely because a remote call did.
`aub --help` covers the mechanical half of the same contract: which shared
flags a command accepts and the formats it renders.

## 5. Establish the backup policy

[docs/backup.md](backup.md) is the ordered procedure: create a verified
archive, point `aub doctor` at it through `backup.destination`, and put
re-verification on a schedule against the review horizon. Do this before
anything depends on the state directory surviving; quota history cannot be
reconstructed once it is gone.

## 6. Importing pre-aub history

Skip this section on a machine that never measured quota before `aub`. Nothing
below it depends on this step.

A machine that did holds up to two series `aub` never wrote: the quota ledger
the pre-`aub` status-line hook appended to, and the seed capture an external
timer archived through the design period, which exists precisely so the design
period would not leave a permanent hole in the meter history (`PLAN.md`'s
[Phase -1](PLAN.md#phase--1-preserve-quota-before-writing-rust) and [Phase
4](PLAN.md#phase-4-legacy-series-import)). Both are irreplaceable: no provider
will answer for last month again. Import them once, and import them here,
after step 5, because each importer refuses to write until it has verified a
backup archive.

**The backup comes first, and it is the archive both commands name.** Create or
pick a verified one (step 5, [docs/backup.md](backup.md)); the imports read the
`--backup` path themselves and refuse with the store exit class when it does
not verify, so an import that went wrong is always recoverable to the state the
archive holds.

```sh
ARCHIVE="$(cat "$BACKUP_DESTINATION/newest-verified")"
/abs/path/to/aub backup verify "$ARCHIVE"
```

**Import the legacy quota ledger.** One named source file, never a directory
scan:

```sh
/abs/path/to/aub import legacy-meter \
    --source /path/to/legacy-quota-ledger.jsonl \
    --backup "$ARCHIVE"
```

**Import the seed capture.** One line of that capture carries a reading per
vendor, and nothing in it says which configured account each vendor's readings
belong to, so the mapping is spelled out: `--vendor-account VENDOR=ACCOUNT`,
repeatable, with `claude` under an `anthropic` account and `codex` under a
`codex` one. A vendor left unmapped is discarded rather than turned into an
account nobody declared.

```sh
/abs/path/to/aub import seed-archive \
    --source /path/to/seed-capture.jsonl \
    --backup "$ARCHIVE" \
    --vendor-account claude=primary \
    --vendor-account codex=work
```

### The counts each import prints, and which of them to expect

Both commands print one line of counts and emit the same fields as a
diagnostic. `records_read` is what the source held; `imported` and `unchanged`
are what became ledger rows on this run and on an earlier one; the rest are the
three ways a record is deliberately not imported.

- **`superseded_by_native=N` is the number to expect to be large**, and it is
  the healthy outcome rather than a loss. Both legacy writers kept running after
  native sampling started, so the tail of each source overlaps a series `aub`
  measured itself. The cutoff is per account and is that account's earliest
  native meter attempt: a reading at or after it is already measured and is
  skipped. On a machine whose sampler has been running for weeks, expect almost
  every reading from the day native sampling began onward to land here, and
  expect `imported` to cover the stretch before it. An account the sampler has
  never reached has no cutoff and imports in full, so `superseded_by_native=0`
  there is also correct. Session and account markers from the legacy meter are
  exempt from the cutoff and import on both sides of it, because native
  sampling records no such marker and nothing later can reconstruct one.
- **`quarantined=N` should be small, and every one of them is a question.** A
  line that will not parse, and a legacy-meter reading whose account no
  `[[accounts]]` entry names, are quarantined with the parser, the failure class
  (`unconfigured_account` for the latter) and the source line number, and no
  account row is invented for them. A nonzero count on a machine whose config is
  complete usually means an account was renamed at some point: add or rename the
  `[[accounts]]` entry (`aub account rename`, and section 4's reference) and
  rerun the same source. Quarantine is not a write that has to be undone.
- **`discarded_unmapped_vendor=N`, seed archive only**, counts readings of a
  vendor no `--vendor-account` named. Expect it to be a multiple of the lines
  read whenever the old capture answered for vendors this ledger has no adapter
  for.

Rerunning the same source is safe and is the normal way to fix a mapping: a
fully imported line is recognised by its source digest and line number, so the
second run reports `imported=0` with the same `unchanged`, `superseded_by_native`
and `quarantined` counts, and leaves the attempt, observation and marker
cardinalities exactly where they were. Neither command prints the source path,
in its output or in its diagnostics; the source is named by content digest.
[docs/commands.md](commands.md#aub-import) carries the full rule set, including
the operator-asserted account classification a codex reading from 2026-08-31
onward imports under.

**Then retire the two legacy writers.** The seed-capture timer and the legacy
status-line hook are what keep writing into the series that was just imported,
and leaving either runnable recreates the defect this project exists to remove:
a confident number from a tool nobody is maintaining. Retirement is its own
procedure, on bead `aub-n27.8`, and its last step is removing the obsolete
binary or hook from the ordinary `PATH` rather than announcing that it is
deprecated. Until that is done, expect each later import of the same source to
report a growing `superseded_by_native`.

## 7. Know the recovery procedure before it is needed

[docs/recovery.md](recovery.md) is the ordered restore procedure for a
damaged state directory, built against the archive step 5 produces. It
matters that this is read once while nothing is on fire: step 2 of that
procedure is "preserve the damaged state directory", which is the opposite
instinct from cleaning up a mess, and the moment to learn that is not during
an actual incident.

### Setting aside a corrupt ledger and restoring from verified archive

`docs/recovery.md` holds the restore procedure and stays its only copy: an
incident is the worst moment to find out that two runbooks disagree about
step order. What belongs here is the part that procedure does not cover,
which is how this particular damage announces itself and what to do with
the file before the restore starts.

The symptom is a ledger whose first page is not a SQLite header. `aub doctor`
reports `sqlite-and-schema-health` failing with
`page 1 integrity probe failed`, and every other subcommand refuses to open
the ledger with the same message. It happened twice on 2026-09-11, and both times the file was set
aside by hand at midnight, which is what this section exists to replace.

Both times the cause was inside `aub` itself, not the disk: the connection
opener created the file and probed its header through short-lived handles on
every open, and POSIX releases every lock a process holds on a file the moment
any of its descriptors is closed. That dropped the shared lock a live
connection holds on the ledger in WAL mode, a second `aub` process (the sampler
tick beside a calibration burst's `aub sample`) then checkpointed and deleted
the WAL and its index under the survivor, and the survivor's next connection
wrote into a fresh WAL against a stale index. `src/store/connection.rs` now
keeps one never-closed handle per database file and reads through it, so the
lock survives every later open in the process; the unit test
`opening_a_second_connection_keeps_the_first_connections_shared_lock` fails if
that ever regresses. Two `aub` processes on one state directory are expected
and safe again, but a hook or driver killed by `timeout` still costs a retry,
and two writers still serialize on the busy timeout.

Stop the cadence first, so nothing writes while the directory is moved:

```sh
systemctl --user stop aub-sample.timer aub-meter-capture.timer
```

Move the damaged state directory aside under a name carrying the minute it
was set aside. Do not delete it: it is the only evidence of what happened,
and `aub backup restore` reads its surviving spool.

```sh
mv ~/.local/state/aub ~/.local/state/aub.corrupt-$(date +%Y%m%dT%H%M)
```

The archive to restore from is named by the `newest-verified` pointer file at
the root of the configured backup destination (`backup.destination`, which
`aub config` prints):

```sh
cat "$BACKUP_DESTINATION/newest-verified"
```

From there follow `docs/recovery.md` in order, starting at its step 3. Restart
the cadence only after `aub doctor` passes against the restored directory:

```sh
systemctl --user start aub-sample.timer aub-meter-capture.timer
```


## 8. Read a failure by its exit code and problem code first

A script or a timer should never need to parse prose to learn what went
wrong. [docs/exit-classes.md](exit-classes.md) is the nine stable process
exit codes; [docs/problem-codes.md](problem-codes.md) is the finer symbolic
code carried in the `--format json` error envelope, one exit class per code.
Both tables are checked against their enums by the test suite, so a code
documented here is a code the binary can actually return.

## Done

A fresh machine has reached a working state once: `aub config` shows every
key resolving from the file just written, `aub status` reflects at least one
recorded sampling attempt for each configured account, and `aub backup
verify DESTINATION` reports `verified=true` against the newest verified
archive under the root named in `backup.destination`.
`tests/e2e/cases/019-fresh-machine-walkthrough.sh`
exercises exactly this sequence end to end against the release binary, using
only the invocations this document and the ones it links to describe.
