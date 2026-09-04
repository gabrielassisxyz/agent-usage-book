# Backup policy

Quota history cannot be reconstructed: a provider answers with the reading
it has right now, not a replay of what it would have answered yesterday. A
lost state directory is a lost series, not a delay, which is what makes a
backup an operational requirement here rather than an afterthought. This
document is the policy as an ordered procedure; `docs/recovery.md` is what to
do with the archive it produces when the state directory is actually
damaged.

1. **Create an archive.** `aub backup DESTINATION` takes a consistent SQLite
   backup through the SQLite backup API, writes a checksum manifest beside
   it, and verifies the archive it just wrote before returning. A backup
   command that reports `verified=false` has not produced a usable archive;
   treat that run as failed and re-run it.
2. **Point `doctor` at it.** Set `backup.destination` in the configuration
   file to the same path (see the [configuration
   sketch](PLAN.md#47-suggested-configuration-sketch)). `aub backup` takes
   its destination as an explicit argument every time and remembers nothing
   durably, so without this setting `aub doctor` has nowhere to look and
   reports the check not applicable rather than failing it: a missing backup
   is silent unless this step is done.
3. **Re-verify on a schedule independent of the archive's own creation.**
   `aub backup verify DESTINATION` re-runs the same checksum, manifest,
   SQLite integrity and foreign-key checks against the archive already on
   disk, so bit rot or a partial copy is caught by a process that did not
   write the file. Run it wherever the archive itself lives, on whatever
   cadence that storage already gets checked.
4. **Watch the review horizon.** `backup.review_after` (default 48h) is how
   long a verified archive is trusted before it counts as due for
   replacement. `aub doctor`'s backup-age check fails once the last verified
   backup is older than this, whether because no backup was ever taken,
   because the last one was never verified, or because it aged past the
   horizon; the failure names which of the three happened. Put `aub doctor`
   on the same schedule as the sampler (`docs/scheduling.md`) so an aging
   backup surfaces before it becomes a missing one.
5. **On a doctor failure, go back to step 1.** Create a fresh archive at the
   configured destination and let step 1's own verification confirm it. The
   review horizon is not a grace period to negotiate; it is the signal that
   a fresh archive is due.

A backup that is created but never pointed at with `backup.destination`, or
never re-verified, satisfies none of this beyond the moment it was written:
the policy is the schedule, not the one command that starts it.
