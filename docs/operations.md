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
verify DESTINATION` reports `verified=true` against an archive named in
`backup.destination`. `tests/e2e/cases/019-fresh-machine-walkthrough.sh`
exercises exactly this sequence end to end against the release binary, using
only the invocations this document and the ones it links to describe.
