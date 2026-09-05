# Adapter semantics validation against the provider authoritative surface

> Design reference: PLAN.md sections 34.8, 34.30, and 45 (provider semantic drift).
> Bead: `aub-eun.12`.

Every automated test in this system proves that the code does what the code was
written to mean. None of them can prove that what the code was written to mean is
what the provider means. A contract fixture is sanitized from a response that an
adapter already interpreted, so an adapter that misreads a field produces fixtures
that agree with it perfectly and a contract suite that stays green forever.

This procedure closes that gap. It is deliberately **manual**, because the oracle is
a human facing page that nobody here controls, and deliberately **recurring**, because
provider semantic drift (window definitions, billing weights, meter cadence, payload
shapes) is invisible to a suite whose fixtures were captured before the change.

What is automated is the bookkeeping. Each comparison is stored as immutable
validation evidence with the observation it was compared against, so the age of the
last comparison is a number `doctor` can report rather than something someone has to
remember.

## The verdict has exactly two outcomes

For one window, the comparison yields one of:

- **agrees within granularity**: the adapter reading and the authoritative surface
  differ by no more than the surface's own documented granularity.
- **unresolved mismatch**: they differ by more. This creates a finding, not a note.
  The finding names the window, both values and the observation, and stays open until
  a human explains it by recording a correction that links to it.

There is no third outcome and no configurable tolerance. The only quantity the verdict
depends on beyond the two readings is the surface's documented granularity, which is a
fixed property of the surface recorded in the table below, never a knob to widen.

The known closed source 41 percent versus 70 percent discrepancy is not a tolerance
precedent. `aub` agrees with the authoritative surface within that surface's documented
granularity, or it reports the mismatch as unresolved.

## Per adapter: comparison target and documented granularity

| adapter | authoritative surface | documented granularity | windows the adapter reports |
|---|---|---|---|
| Anthropic (`src/meter/anthropic.rs`) | the usage view in the Anthropic Console for the configured account | whole percentage points, that is 10000 parts per million | `limits[].kind=session` (account wide), `limits[].kind=weekly_all` (account wide), and optional `limits[].kind=weekly_scoped` entries keyed by their model scope (model specific) |
| Anthropic idle / not-started (`src/meter/anthropic.rs`) | the usage view in the Anthropic Console for the configured account | whole percentage points (0% used, `resets_at: null` expected when idle, indicating no window in progress) | `limits[].kind=session` and `limits[].kind=weekly_all` in typed `not_started` state (`resets_at: null`), plus any model-scoped `limits[].kind=weekly_scoped` entries |

When the surface changes what it displays or the granularity it displays it at, update
this row in the same change that adapts the adapter, and record a fresh comparison.

A response that omits a required `limits[].kind` is a provider-contract change: the
adapter refuses the incomplete observation with `MissingRequiredField`, and the
semantic-validation workflow keeps the resulting finding open until a human records
a correction. Optional `weekly_scoped` entries may disappear without opening one.

## Performing one comparison

1. Pick a recent successful observation for the account. Note its `aub` observation
   identifier (the `meter_observation` rowid).
2. List **every** semantic window that observation recorded, not only the one a status
   line displays: `aub compare uncompared OBSERVATION_ID` names the windows that still
   lack a comparison, and the comparison of an observation is complete only when it
   answers that none remain. A window nobody looks at is where a misreading survives
   longest.
3. For each window, read the corresponding value from the authoritative surface as
   close in time to the observation as possible, and note the local time of the reading.
4. Record each window through the binary, giving the percentage exactly as the surface
   displays it and the time it was read:

   ```sh
   aub compare record OBSERVATION_ID WINDOW --surface "anthropic console" \
       --surface-percent 21 --read-at 2026-09-04T11:20:00-03:00
   ```

   The verdict is computed by
   `domain::authoritative_comparison::compare_against_authoritative_surface` from the
   adapter's stored reading, the surface value and the documented granularity, which
   defaults to one whole point (the table above) and is overridden with
   `--granularity-percent` only when that table changes. There is no flag that sets the
   verdict, and a window that already carries a comparison is refused by name rather
   than overwritten.
5. An unresolved mismatch is recorded as a `mismatch` annotation by the same command,
   so it becomes an open finding surfaced by `open_semantic_mismatch_findings` without
   a second step to forget.
6. `aub doctor` then reports the age of the newest comparison under
   `adapter-semantics-comparison-age`, and fails it once it is older than
   `adapter_semantics.max_comparison_age` (default 30 days), which is how the recurrence
   this procedure depends on stops being something a person has to remember.

## Correcting a comparison

A comparison is immutable. A wrong comparison is corrected by recording **another**
comparison, and a mismatch is explained by recording a `correction` annotation that
links to the mismatch annotation through `corrects`. The earlier records stay stored;
nothing is overwritten or deleted. An unresolved mismatch stays open until such a
correction exists.

An `exclusion` annotation records a window or observation to hold out of calibration
eligibility because of a known semantic discrepancy. This procedure only persists it;
`aub-c0b.7` consumes it.

## Retention

Comparisons and annotations are irreplaceable validation evidence. The durable class
taxonomy retains them forever and includes them in verified backups; the round trip is
proved by `tests/adapter_semantics_validation.rs`. They are never pruned.

## Comparison log

One comparison per adapter has to be performed and recorded against the real surface,
and any mismatch has to be explained or carried as an open finding. The rows below are
the human-readable index of the immutable `authoritative_surface_comparison` records;
the record itself, with the verdict the code computed, is the one `aub doctor` ages.

| date (local) | adapter | observation id | window | comparison id | adapter | surface | verdict |
|---|---|---|---|---|---|---|---|
| 2026-09-04 11:20 | Anthropic | 2 (account `max`) | `five_hour` | #1 | 1% | 1% | agrees within granularity |
| 2026-09-04 11:20 | Anthropic | 2 (account `max`) | `seven_day` | #2 | 21% | 21% | agrees within granularity |

The first comparison was read by the operator from the Console usage view on 2026-09-04
at about 11:20 local and recorded through `aub compare record` the same day, once that
command existed (`aub-x2bq`); observation 2 carried no model-specific window, so the two
rows are its whole set.
