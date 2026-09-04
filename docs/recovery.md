# Recovery procedure

This procedure restores the Phase 1 ledger and its pending observation spool.
It must be followed in order. A restore writes only to a new directory. Never
overwrite or replay directly from the damaged state directory: keep it as the
forensic copy until the recovery has been reviewed.

1. Stop every mutating `aub` invocation that can write the state directory.
2. Preserve the damaged state directory. Do not overwrite it, delete it, or use
   it as the restore destination. If its pending spool must be replayed, use
   `--surviving`; `aub` snapshots those files before replay so the damaged
   directory remains unchanged.
3. Verify the archive checksum and manifest before restoring it:
   `aub backup verify ARCHIVE`.
4. Restore into a directory that does not exist yet:
   `aub backup restore ARCHIVE RESTORED_DIR --surviving DAMAGED_DIR`.
   Omit `--surviving` only when there is no surviving pending spool to recover.
5. Read the restore result. It reports `integrity=ok`, `foreign_keys=ok`, and
   the migrations applied to the restored database. Do not put the restored
   directory into service if any of those checks fail.
6. Check both replay lines and the exact `observations=N` count. Archive and
   surviving pending records are replayed idempotently, so a record found in
   both sources is counted once. Every `unrecovered:` line names evidence that
   was preserved but could not be applied, with its source and reason.
7. The projection is rebuilt, never restored: the archive carries no
   projection file, and the damaged directory's own copy may be exactly what
   the recovery was called in to fix. `aub` rebuilds it deterministically
   from the restored database's own state and reports `projection: rebuilt`.
   A rebuild that could not run yet (the projection publish deferred) reports
   `projection: deferred`; the restored database is unaffected either way,
   and the next publish heals it.
8. Transcript-derived tables have no writer in Phase 1, so there is nothing to
   rebuild. If a later phase adds one, rebuild those tables only from their
   durable inputs after the database and spool recovery succeeds.

## Periodic restore drill

`aub drill` runs this procedure end to end and proves it still works, rather than
leaving that as something an operator finds out mid-incident. It never targets the
configured state directory, in either mode below.

Against a real archive, the way a scheduled drill runs (see
[`docs/scheduling.md`](scheduling.md#scheduling-the-periodic-restore-drill)):

```sh
aub drill --archive ARCHIVE SCRATCH_DEST
```

Against one of four seeded damage cases, for exercising the procedure without a real
archive at hand:

```sh
aub drill --seed truncated-database SCRATCH_DEST
aub drill --seed corrupted-projection SCRATCH_DEST
aub drill --seed malformed-spool-record SCRATCH_DEST
aub drill --seed unsupported-schema-version SCRATCH_DEST
```

Each seeded case damages a scratch copy of a small ledger in a different way, then
restores a clean archive of it with the damaged copy as the surviving directory,
exactly as the numbered steps above describe. `truncated-database` and
`unsupported-schema-version` prove something stronger than "recovery works": that the
procedure never opens the damaged directory's own database at all, only its pending
spool. `corrupted-projection` proves step 7 rebuilds rather than restores. Every run
prints `drill: passed=true` or `false`, and, when `drill.result` is configured, appends
a durable record `doctor` reads for the age of the last successful one.

The repository's own end-to-end suite additionally runs this procedure against the
release binary as part of its regression coverage:

```sh
tests/e2e/run.sh
```

That runner records every command, its exit status, and the state-directory digest
before and after each step in its run log; it is repository test infrastructure, not
something an operator schedules.
