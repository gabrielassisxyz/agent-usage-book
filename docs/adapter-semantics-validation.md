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
| Anthropic (`src/meter/anthropic.rs`) | the usage view in the Anthropic Console for the configured account | whole percentage points, that is 10000 parts per million | `five_hour` (account wide), `seven_day` (account wide), and one `seven_day_<model>` per model the response carries (model specific) |

When the surface changes what it displays or the granularity it displays it at, update
this row in the same change that adapts the adapter, and record a fresh comparison.

## Performing one comparison

1. Pick a recent successful observation for the account. Note its `aub` observation
   identifier (the `meter_observation` rowid).
2. List **every** semantic window that observation recorded, not only the one a status
   line displays. `store::adapter_semantics_validation::uncompared_window_ids` returns
   the windows of an observation that still lack a comparison; the comparison of an
   observation is complete only when that list is empty. A window nobody looks at is
   where a misreading survives longest.
3. For each window, read the corresponding value from the authoritative surface as
   close in time to the observation as possible, and record the local timestamp of the
   reading.
4. Compute the verdict with
   `domain::authoritative_comparison::compare_against_authoritative_surface`, passing the
   adapter's stored `quota_used`, the value read from the surface, and the documented
   granularity from the table above.
5. Record the comparison with
   `store::adapter_semantics_validation::insert_comparison`. It stores the observation
   identifier, the window, the surface name, the granularity, both values, the reading
   timestamp, and the verdict.
6. If the verdict is an unresolved mismatch, record a `mismatch` annotation with
   `insert_annotation`. It becomes an open finding, surfaced by
   `open_semantic_mismatch_findings`.

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
and any mismatch has to be explained or carried as an open finding. The environment that
added this procedure has no access to a provider account, so the first real comparison
is outstanding.

| date (local) | adapter | observation id | windows compared | result |
|---|---|---|---|---|
| _pending_ | Anthropic | _pending_ | _pending_ | first real comparison not yet performed |
