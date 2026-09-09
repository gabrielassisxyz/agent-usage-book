# Backup policy

Quota history cannot be reconstructed: a provider answers with the reading
it has right now, not a replay of what it would have answered yesterday. A
lost state directory is a lost series, not a delay, which is what makes a
backup an operational requirement here rather than an afterthought. This
document is the policy as an ordered procedure; `docs/recovery.md` is what to
do with the archive it produces when the state directory is actually
damaged.

1. **Create an archive.** `aub backup [DESTINATION]` takes a consistent SQLite
   backup through the SQLite backup API, writes it as a new dated archive
   under the destination root, writes a checksum manifest beside it, and
   verifies the archive it just wrote before returning. The archive directory
   name carries its creation instant and the source ledger generation, for
   example `aub-backup-2026-09-09T12-00-00.123456789Z-g9648`. An explicit
   argument wins; with no argument the command uses `backup.destination`
   from configuration, so the command and the health check can no longer
   disagree. A backup command that reports `verified=false` has not produced
   a usable archive; treat that run as failed and re-run it. A failed run
   prunes nothing and does not advance the pointer.
2. **Point `doctor` at the root.** Set `backup.destination` in the
   configuration file to the same root (see the [configuration
   sketch](PLAN.md#47-suggested-configuration-sketch)). `aub doctor` reads
   the newest-verified pointer inside that root, and the age it reports comes
   from the pointed-to archive's manifest `created_at_unix_nanos`, never from
   file mtime. Without this setting `aub doctor` has nowhere to look and
   reports the check not applicable rather than failing it: a missing backup
   is silent unless this step is done.
3. **Re-verify on a schedule independent of the archive's own creation.**
   `aub backup verify ARCHIVE` re-runs the same checksum, manifest,
   SQLite integrity and foreign-key checks against one archive already on
   disk, so bit rot or a partial copy is caught by a process that did not
   write the file. Run it wherever the archives live, on whatever
   cadence that storage already gets checked.
4. **Watch the review horizon and the retention buckets.**
   `backup.review_after` (default 48h) is how long a verified archive is
   trusted before it counts as due for replacement. `aub doctor`'s backup-age
   check fails once the last verified backup is older than this, whether
   because no backup was ever taken, because the last one was never verified,
   or because it aged past the horizon; the failure names which of the three
   happened. Put `aub doctor` on the same schedule as the sampler
   (`docs/scheduling.md`) so an aging backup surfaces before it becomes a
   missing one.
   Retention is tiered: `backup.keep_daily` (default 7), `keep_weekly`
   (default 4), `keep_monthly` (default 6) and `keep_yearly` (default 2).
   An archive is retained when any bucket keeps it, and pruning never removes
   the most recent verified archive even when no bucket keeps it. Pruning is
   refused when the root holds no verified archive at all. The defaults bound
   the worst case to 19 archives: at the measured upper bound of 11.7 MB per
   day the ledger passes 4.5 GB within a year, so 30 full copies would cost
   about 135 GB of 210 GB free on /tank, while 19 cost about 86 GB and still
   span a week of dailies, a month of weeklies, half a year of monthlies and
   two yearlies for late-noticed corruption.
5. **On a doctor failure, go back to step 1.** Create a fresh archive under
   the configured root and let step 1's own verification confirm it. There is
   no need to invent a new path: a second run writes a second dated archive
   beside the first. The review horizon is not a grace period to negotiate;
   it is the signal that a fresh archive is due.

## Migration from the single-archive layout

Releases before the series change wrote one archive directly at the
destination path. Those directories are left in place, not adopted
automatically: an automatic move would touch irreplaceable evidence with no
operator in the loop. To migrate, pick a new root (or empty the old path
after copying its archive aside), set `backup.destination` to that root,
and run `aub backup` twice; both archives appear under the root, the pointer
names the newer, and `aub doctor` reports backup age from it. Keep the copied
aside archive until the new series has two verified archives of its own,
then retire it by hand.

A backup that is created but never pointed at with `backup.destination`, or
never re-verified, satisfies none of this beyond the moment it was written:
the policy is the schedule, not the one command that starts it.
