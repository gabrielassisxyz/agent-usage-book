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

## 6. Know the recovery procedure before it is needed

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
reports `sqlite-and-schema-health` failing with `page 1 integrity probe
failed`, and every other subcommand refuses to open the ledger with the same
message. It happened twice on 2026-09-11, and both times the file was set
aside by hand at midnight, which is what this section exists to replace.

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


## 7. Read a failure by its exit code and problem code first

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
