# `aub`: System Design Document

**One ledger for LLM consumption.**

`aub` joins two physical quantities that currently live in different systems:

- reconstructible local **usage evidence**, primarily token usage recovered from
  agent transcripts; and
- irrecoverable remote **quota observations**, sampled from provider-owned usage
  endpoints.

It does not collapse those quantities into one number. It gives them one chain of
custody, one attribution model, one provenance system, and explicitly versioned
conversion witnesses where one axis must be related to another.

**Status:** pre-implementation design
**Language:** Rust, edition 2024
**Form:** single-shot CLI binary; no daemon, server, container, or async runtime
**Purpose:** one trustworthy ledger for LLM consumption, joining reconstructible token spend with irrecoverable provider quota observations

---

# 0. The governing idea

The defect that motivates `aub` is not principally that seven commands are
inconvenient.

It is that two pieces of software can silently claim to represent the same physical
quantity while:

1. using a copied constant;
2. implementing different definitions of the quantity;
3. observing different accounts;
4. carrying different freshness;
5. dropping different transcript records;
6. and still printing equally confident scalar numbers.

The architecture is therefore designed so that the dangerous mistakes require
crossing explicit type, persistence, or provenance boundaries rather than merely
violating a convention.

The system has five independent correctness dimensions:

1. **Unit**: what physical quantity is this?
2. **Freshness**: how current is this remote reading?
3. **Coverage**: does this value include everything its name implies?
4. **Evidence quality**: were the included values measured or reconstructed?
5. **Provenance**: what evidence and conversion witnesses justify it?

None substitutes for another.

An exact but stale meter reading is stale.

A fresh provider reading may still be incomplete if the provider omits a required
window.

A perfectly current token estimate reconstructed from characters is still an
estimate, not measured token usage.

A numerically plausible result without a provenance chain is not publishable.

The most important asymmetry remains:

> **Spend can be reconstructed later. Quota history cannot.**

That determines implementation order, persistence durability, diagnostics, backup
policy, and operational priorities.

---

# 1. Executive summary

`aub` should be designed around one asymmetry:

> **Token spend can be reconstructed later. Quota history cannot.**

That means the first durable capability is not transcript parsing or reporting. It is reliable, named-account sampling of provider meters into an append-mostly local series.

Everything else builds outward from that series.

The persistence layer is **one bundled SQLite database in WAL mode**, located only
on a local filesystem. Quota attempts and the sanitized provider response evidence
they produce are durable evidence and are never considered rebuildable. The
normalized meter values read out of that evidence are a separately versioned
interpretation of it, so an adapter that misread a field can be corrected later
against retained evidence instead of having destroyed it. Transcript-derived
normalized events, attribution segments, search indexes, and analytical read models
are explicitly rebuildable.

The status bar is the one deliberate exception to "read SQLite directly." A
transactionally-derived, atomically replaced **projection file** contains only the
small amount of state needed by `aub status`. It is disposable and is never an
independent source of truth. `status` performs one bounded local file read, no
SQLite open, no lock acquisition, no transcript scan, and no network operation.
Freshness is recomputed from timestamps at render time, so killing the sampler
causes the projection to visibly age rather than remaining confidently "fresh."

The projection is not a second measurement cache. It contains:

- the identifier and timestamp of the last successful observation;
- the latest sampling-attempt outcome;
- the windows in that observation;
- the projection schema version;
- and the source database ledger generation from which it was built.

It contains neither a copied calibration coefficient nor a persisted `is_fresh`
boolean.

The system preserves the required three-way **freshness** state exactly:

`fresh | stale | auth_required`

but separates freshness from the **outcome of the latest collection attempt**.

That distinction resolves an important ambiguity without inventing a fourth
freshness value:

- a timeout, DNS failure, HTTP 5xx, malformed response, or rate limit is an
  `AttemptOutcome::Unreachable`;
- an expired or rejected credential is `AttemptOutcome::AuthRequired`;
- a successful parse is `AttemptOutcome::Success`;
- the user-facing reading derived from the attempt history is still exactly one of
  `Fresh`, `Stale`, or `AuthRequired`.

For example, a 503 after a good observation yields:

`Stale { last_good: Some(...), reason: SourceUnreachable(Http503) }`.

A 503 before the first successful observation yields:

`Stale { last_good: None, reason: SourceUnreachable(Http503) }`.

Authentication has its own variant because its remedy is materially different.

An attempt becomes durable before the request leaves the process. Its start is
committed first and its terminal result is a separate later fact, so a collector
killed mid-request leaves a started attempt with no result rather than leaving no
evidence that anybody looked.

Every physical quantity should similarly be represented by a distinct domain type. At minimum, token counts, credits, quota fractions, percentage-point deltas, money, durations, timestamps, rates, and intervals are distinct. `QuotaUsed` and `QuotaRemaining` should themselves be distinct despite sharing a display unit. Raw integers and floats belong only inside adapters, parsers, serialization code, and database codecs; they are converted into validated types immediately.

The copied calibration constant becomes structurally difficult to reproduce.
Calibrated quantities have private constructors and production code obtains them
through typed repository accessors. Window calibrations and cost-model
coefficients are immutable versioned data with provenance, experiment and input
hashes, model semantics, residuals, uncertainty, fit version, and applicability
intervals. Every consumer reads the same record by ID.

Crucially, the design keeps two calibrations conceptually separate:

1. **Cost model:** token-kind vector to subscription credits.
2. **Window calibration:** subscription credits to provider percentage points.

They may be estimated in related experiments, but they are not automatically
collapsed into one regression. This preserves the physical distinction that the
legacy tools lost.

The existing dollar-cost functionality should belong in `aub`, but only as a deliberately separate **valuation layer**. It should never call subscription credits “dollars” or imply that published API prices are the user's actual subscription cost. The correct output concept is something like **API list-price equivalent**. Rate cards should be versioned, dated data, not hardcoded source constants. If the price table does not cover every component needed for a total, `aub` must not emit an apparently complete total.

Headless and non-interactive runs are covered by an **external scheduler invoking
`aub sample --due`**, not a daemon. Hooks and the profile-switching launcher add
high-confidence account and session markers. The timer captures quota movement
regardless of whether a shell exists.

Consumption that moves the meter but has no matching local transcript is preserved
as **unexplained meter residual**, with uncertainty. It is not converted into
invented tokens.

The recommended public surface is roughly:

```text
aub sample ...
aub now ...
aub status ...
aub spend ...
aub coverage ...
aub calibrate ...
aub can-run ...
aub task ...
aub export ...
aub backup ...
aub doctor ...
aub config ...
```

`aub now` is the live path and always persists the attempt before presenting it as a
completed collection. `aub status` is the glanceable local-only path. Keeping the
commands separate prevents a shell integration from accidentally enabling network
I/O via a forgotten flag.

There is no ordinary "fetch but do not record" mode.

---

## 1.1 Decisions at a glance

| Question | Decision |
|---|---|
| System of record | Bundled SQLite, WAL, local filesystem only |
| Irreplaceable data | Meter attempt starts and results, sanitized provider response evidence, imported historical meter evidence |
| Evidence versus interpretation | Response evidence is durable; normalized meter values are a versioned interpretation of it |
| Rebuildable data | Transcript normalization, attribution segments, analytical indexes, projection |
| Status path | Atomic projection file; no DB, network, migration, or write |
| Freshness | Exactly `fresh` / `stale` / `auth_required` |
| Network failure | Separate persisted attempt outcome; degrades reading to `stale` with typed reason |
| Meter sampling | External timer calling `aub sample --due`; hooks are opportunistic |
| Coverage | First-class report over expected sampling opportunities |
| Spend dedupe | Semantic event identity plus DB uniqueness, never file hashes |
| Calibration | Cost model and window calibration separated; passive candidates plus controlled experiments |
| Server lag | Settled-boundary/plateau methodology; never assumed away |
| Dollar valuation | Optional counterfactual API-list-price layer |
| Live read | `aub now`; always recorded |
| Glanceable read | `aub status`; projection only |
| Can-run | Historical empirical range plus calibration uncertainty plus fresh quota, evaluated against every constraining window; no duration forecasting |
| Friction ledger | Separate system joined by stable `RunId`/`SessionId` |
| Backups | First-class because quota evidence is irreplaceable |

---

# 2. Problem statement

Today there are multiple tools representing overlapping pieces of the same physical system:

* transcript-derived token consumption;
* inferred subscription credit consumption;
* provider-reported percentage usage;
* account identity;
* quota-window semantics;
* calibration coefficients;
* API dollar-equivalent prices;
* session and project attribution;
* task attribution.

The primary failure is not availability. It is **semantic divergence**.

A calibrated value is copied into another program. The original calibration is known to be incomplete because it omits cache-write billing. A second program uses the copied number without having any path through which a corrected fit could reach it. A third disagreement is even worse: different programs disagree about which physical components contribute to cost at all.

`aub` therefore does not primarily exist to reduce the number of commands.

It exists to establish one chain of custody from:

**raw evidence → normalized units → durable observations → derived quantities → user decision**

and to make it difficult for two modules to silently form different ideas of what the same number means.

An additional requirement follows from the destructive meter axis:

> The system must be able to distinguish "nobody looked" from "we looked and could
> not obtain a value."

That is why failed meter attempts are durable records rather than transient logs.

---

# 3. Design goals

The central correctness goals are stronger than ordinary CLI correctness.

## 3.1 Unit correctness

No semantically meaningful numeric value crosses a module boundary as an untyped integer or floating-point number.

Examples of distinct domain types include:

* `TokenCount`
* `InputTokens`
* `OutputTokens`
* `CacheReadTokens`
* `CacheWriteTokens`
* `Credits`
* `CreditsPerToken`
* `CreditsPerPercentagePoint`
* `QuotaUsed`
* `QuotaRemaining`
* `PercentagePoints`
* `Money`
* `MoneyPerMillionTokens`
* `WindowDuration`
* `UtcTimestamp`
* `ProviderObservedAt`
* `ReceivedAt`
* `MonotonicDuration`
* `MeasurementBasis`
* `Age`
* `Interval<T>`

The type system should make nonsense operations awkward or impossible.

A token count should not be addable to credits. `QuotaUsed` should not accidentally be passed where `QuotaRemaining` is expected. Money from USD and another currency should not silently combine.

## 3.2 Explicit freshness

Every provider meter reading is represented by an exhaustive state:

* `fresh`
* `stale`
* `auth_required`

The state is not an optional field and is not a boolean.

The enum carries sufficient context:

```rust
pub struct Observed<T> {
    value: T,
    provider_observed_at: Option<ProviderObservedAt>,
    received_at: ReceivedAt,
    measurement_basis: MeasurementBasis,
    source: SourceId,
    observation_id: ObservationId,
}

pub enum Freshness<T> {
    Fresh {
        observed: Observed<T>,
        latest_attempt: AttemptRef,
    },
    Stale {
        last_good: Option<Observed<T>>,
        latest_attempt: AttemptRef,
        reason: StaleReason,
    },
    AuthRequired {
        last_good: Option<Observed<T>>,
        latest_attempt: AttemptRef,
        account: AccountId,
        reason: AuthReason,
    },
}
```

`StaleReason` is exhaustive over reasons relevant to presentation, including:

* `AgeExceeded`;
* `NoSuccessfulObservation`;
* `SourceUnreachable(FailureClass)`;
* `MalformedProviderResponse`;
* `RateLimited`;
* `SamplingGap`;
* `ClockAnomaly`;
* `CollectorInterrupted`;
* `CredentialChangedUnverified`.

This does not add a fourth freshness state. It preserves the three outcomes the user
needs while retaining the operational distinction between timeout, malformed data,
and old-but-successful data.

The persisted collection attempt is modeled separately:

```rust
pub enum AttemptOutcome {
    Success { observation_id: ObservationId },
    AuthRequired { reason: AuthReason },
    Unreachable { class: FailureClass },
}
```

The attempt itself is a two-stage append-only lifecycle, because an outcome that is
only written after the network returns is an outcome that a crash can erase:

```rust
/// Durable before any network I/O begins.
///
/// Absence of a terminal result past the command's maximum execution horizon means
/// the collector was interrupted. That is never rewritten as a network timeout, and
/// never as "no attempt occurred".
pub struct AttemptStarted {
    attempt_id: AttemptId,
    account: AccountId,
    started_at: UtcTimestamp,
    credential_context: CredentialContextId,
    policy_snapshot: SamplingPolicySnapshotId,
}

pub struct AttemptResult {
    attempt_id: AttemptId,
    finished_at: UtcTimestamp,
    elapsed: MonotonicDuration,
    outcome: AttemptOutcome,
}
```

A started attempt with no result is itself evidence, and it is a materially
different failure from a provider that answered badly: the collector died, so
`coverage` reports it as collector interruption rather than as provider
unavailability.

The database stores attempts. `Freshness<T>` is reconstructed from attempt history,
the last successful observation, current time, and source policy.

There should be no wildcard matches over this enum in core logic.

No `is_stale` or `is_fresh` boolean is persisted.

Time can change freshness without changing historical evidence.

## 3.3 Single-source calibrated parameters

Calibrated parameters are data, not literals.

The following must never be copied into consumers:

* window capacity;
* credits-per-percentage-point;
* cache-write weighting;
* any other empirically measured billing coefficient.

A calibration is immutable once published. A newer calibration supersedes it.

Consumers resolve the applicable calibration through a central repository.

Production constructors for calibrated domain objects are private to the
calibration and store boundary.

This is more reliable than a generic ban on numeric literals. HTTP status constants,
schema versions, array sizes, retry counts, and test values are legitimate numeric
literals. The invariant is specifically that **domain conversion witnesses cannot be
constructed ad hoc by consumers**.

CI additionally contains a tombstone check for the known legacy constant and tests
that consumers change behavior when the active calibration row changes without any
source-code edit.

## 3.4 No compiled identity

Source code contains no machine-specific paths, account names, usernames, personal identifiers, repository locations, state directories, or credential paths.

Those belong to configuration or runtime-discovered data.

## 3.5 No invented fallback values

Failures remain failures.

`aub` must never translate:

* network failure → `0%`;
* missing transcript → `0 tokens`;
* expired credentials → previous percentage presented as current;
* missing price → zero-dollar cost;
* unknown account → whichever credentials happen to be in the process environment.

A historical value may be displayed, but only with its historical timestamp and stale status.

No quantity newtype implements `Default`.

This intentionally makes common fallback mistakes such as:

```rust
maybe_credits.unwrap_or_default()
```

fail to compile.

## 3.6 Provenance

A user should be able to answer:

> “Why does `aub` believe this number?”

Every important derived quantity should have enough provenance internally to trace it to:

* transcript records;
* provider observations;
* account markers;
* calibration IDs;
* cost-model IDs;
* rate-card IDs;
* task-attribution intervals.

Normal human output need not print all of that, but JSON and diagnostic modes should expose it.

All quantitative commands support:

```text
--explain
--explain=full
```

`--explain` shows, at minimum:

* a stable `DerivationId`;
* source-event and observation counts;
* content-addressed input-manifest IDs;
* account attribution evidence;
* cost-model ID;
* window-calibration ID;
* rate-card version when applicable;
* coverage and evidence-quality status;
* empirical-history selection for `can-run`;
* and the arithmetic and conversion sequence.

`--explain=full` expands those manifests into the individual evidence IDs.

The split exists because provenance that cannot be read is not provenance. A report
covering 100,000 usage occurrences must not dump 100,000 identifiers to prove it
knows where they came from, so the ordinary level names the manifest and the full
level names its members.

Conceptually:

```rust
pub struct DerivationId(...);

pub struct ProvenanceManifest {
    inputs_hash: Digest,
    input_count: usize,
    witnesses: BTreeSet<WitnessId>,
    query_semantics: QuerySemantics,
}
```

The goal is that "where did this number come from?" never requires reading Rust.

## 3.7 Data quality

Data quality is a first-class dimension independent of freshness, and it is not one
dimension. Two orthogonal questions are being asked at once:

* **coverage**: is all the evidence that should contribute actually present?
* **evidence quality**: how were the values that are present obtained?

A single enum forces one of those answers to be erased. Consider nineteen transcripts
that parse, one that fails, and one CLI among the nineteen that exposes only
character-derived token estimates. That report is partial and estimated at the same
time, and a report that can only say one of those is lying about the other.

```rust
pub enum CoverageCompleteness {
    Complete,
    Partial { missing: BTreeSet<ComponentKind> },
}

pub enum EvidenceQuality<T> {
    Measured,
    Estimated {
        methods: BTreeSet<EstimatorId>,
        uncertainty: Option<Interval<T>>,
    },
    Mixed {
        methods: BTreeSet<EstimatorId>,
        uncertainty: Option<Interval<T>>,
    },
}

pub struct Qualified<T> {
    value: T,
    coverage: CoverageCompleteness,
    quality: EvidenceQuality<T>,
    provenance: Provenance,
}
```

A third state is also needed, because "a value exists but is qualified" and "the
requested derivation cannot be performed at all" are different answers. A valuation
missing the cache-write rate for one model is not a partial total; there is no total:

```rust
pub enum Derivation<T> {
    Available(Qualified<T>),
    Unavailable {
        missing: BTreeSet<RequiredFact>,
        provenance: Provenance,
    },
}
```

Examples:

* an exact transcript usage record is `Complete + Measured`;
* a parser that cannot observe cache-write usage is `Partial + Measured`;
* character-count token reconstruction is `Complete + Estimated`;
* an aggregate mixing exact and reconstructed events is `Complete + Mixed`;
* an aggregate with a parse failure and reconstructed records is `Partial + Mixed`;
* a valuation missing one model's cache-write rate is `Unavailable`, not a total.

Coverage and evidence quality propagate independently and monotonically. Combining
complete coverage with partial coverage cannot produce complete coverage. Combining
measured and estimated values cannot produce `Measured`.

Where a partial quantity has a meaningful lower-bound interpretation, output may
label it **floor**. Whether a partial value is mathematically a lower bound is a
domain rule about the missing component, and it is never implied merely by
`Partial`.

## 3.8 No scalar erasure of token physics

The normalized token representation is a vector over kinds.

```rust
pub enum TokenKind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

pub struct KnownTokenVector {
    input: InputTokens,
    output: OutputTokens,
    cache_read: CacheReadTokens,
    cache_write: CacheWriteTokens,
}
```

There is deliberately no generic `total_tokens()` used for billing or calibration.

Persistence additionally permits unknown future provider and CLI components:

```rust
pub struct UsageVector {
    known: KnownTokenVector,
    unknown: BTreeMap<ExternalComponentKey, TokenCount>,
    coverage: CoverageCompleteness,
    quality: EvidenceQuality<TokenCount>,
}
```

Any non-empty unknown-component set prevents complete conversion to credits or
money until a model explicitly defines it.

---

# 4. Non-goals

`aub` is not:

* an account switcher;
* a throttle;
* a budget enforcement mechanism;
* an account scheduler;
* a server;
* a resident daemon;
* a generic vendor-coverage project;
* a replacement for the separate run-friction ledger;
* a general cost forecaster;
* a tool that derives fake token counts from quota deltas.

The last point is particularly important.

If a web session consumes provider quota but produces no local transcript, `aub` may know that quota changed. It does **not** therefore know which token classes caused that change. The gap remains explicit.

`aub` also does not predict run duration. The separate friction ledger owns wall
clock. Consequently `can-run` may display a quota reset timestamp but must not claim
that a task will or will not cross that reset unless such a duration is explicitly
provided by some future external integration.

---

# 5. Assessment of the already-decided constraints

The existing constraints are sound.

### Rust 2024

A good choice. The type system is directly useful for the principal correctness claim.

Pin the toolchain in `rust-toolchain.toml`.

Persist the `aub` version and source revision used for every calibration result and
schema migration.

### No async runtime

Also appropriate for the described system.

A normal execution performs a small number of independent HTTP operations. A bounded fan-out of blocking requests in scoped threads is simpler than carrying an executor through the entire program.

The design must compensate with:

* explicit connect timeouts;
* explicit response timeouts;
* bounded concurrency;
* no thread waiting indefinitely on a broken endpoint.

This should be reconsidered only if `aub` eventually turns into a long-running process or must multiplex dozens or hundreds of simultaneous operations. Neither is currently in scope.

### SQLite

SQLite is the recommended persistence decision, not merely a candidate.

Its transactional semantics, WAL support, indexes, migrations, portability, and ability to mix append-heavy tables with tiny read models fit this workload well.

Use a bundled SQLite build so behavior does not depend on the host's SQLite version.

The state directory must be on a local filesystem. `aub doctor` checks that WAL is
safe there and rejects known network-filesystem cases.

### Advisor, not manager

Correct boundary.

`can-run` reports evidence and an interval. It does not reserve quota, switch profiles, stop work, or enforce its conclusion.

### No daemon

Compatible with the “meter history matters most” requirement as long as an external scheduler invokes the binary.

The scheduling service may be cron, a systemd timer, launchd, or another machine-native scheduler. That scheduler is not part of `aub`.

The recommended pattern is that the scheduler invokes `aub sample --due` more often
than the nominal sampling interval, for example every minute, while `aub` itself
decides which accounts are due. This permits reset-edge sampling without a resident
process or dynamically rewriting timer definitions.

### Separate friction ledger

Keep it separate.

`aub` should understand a shared `RunId` and be able to emit or store that identifier, but it must not absorb wall time, retries, surveys, or subjective run quality.

---

# 6. The fundamental data model: evidence versus reconstruction

The database should make a formal distinction between two classes of information.

| Class | Examples | Can be rebuilt? | Durability expectation |
| --- | --- | ---: | --- |
| Irreplaceable evidence | provider attempt starts and results, sanitized meter-response evidence capsules, historical account/session markers when supplied by hooks | No | highest |
| Versioned interpretation | normalized meter observations and windows derived from retained response evidence | Usually | high |
| Reconstructible materialization | parsed transcript usage, dedup index, session grouping, repository grouping, task attribution | Yes | normal |
| Versioned reference data | calibration results, cost models, rate cards | Only if original source still exists | high |
| Derived read models | precomputed summaries, analytical read models | Yes | disposable |
| Status projection | current compact meter picture for shell/status use | Yes | disposable |
| Sampling failures | attempted reads that failed or required auth | No | high |
| Ingest quarantine | records that could not be normalized | Re-derivable, but diagnostically valuable | normal |

Provider response evidence and provider interpretation are distinct classes, and
collapsing them is the quiet way to lose data that was successfully collected. The
fields received from the remote source are evidence. Turning those fields into
`QuotaUsed`, window semantics and reset facts is an adapter-versioned interpretation
of that evidence, and interpretation is software that can be wrong. A normalized
value stored as though it were the remote truth, with the response discarded, is a
misreading that no later fix can reach.

This distinction should appear in table names, migration documentation, backup tooling, and recovery behavior.

The important consequence is:

> `aub rebuild` may delete transcript-derived tables. It must never delete meter observations.

Nor may `rebuild` delete meter-attempt history, because the absence or presence of
attempts is how coverage distinguishes a dead scheduler from a dead endpoint.

---

# 7. Domain model

## 7.1 Quantities

Internally, quantities should use validated newtypes with private storage.

Raw numeric representations are implementation details.

For example, percentages should not use unconstrained `f64` values. A fixed-point representation is preferable.

One practical representation is a quota fraction stored in parts per million:

* `0` = 0%
* `500_000` = 50%
* `1_000_000` = 100%

That provides far more precision than any expected provider display without floating-point comparison issues.

Percentage **levels** and percentage-point **deltas** should still be different types.

Similarly:

* transcript token counts are unsigned integers;
* monetary values should use fixed decimal or integer micros;
* calibrated coefficients should use fixed decimal or rational representations rather than binary float at persistence boundaries.

Regression calculations may use floating point internally, but fit results are validated and converted into typed persisted quantities before leaving the calibration module.

Authoritative persisted percentages are fixed-point integers, not SQLite `REAL`.

Recommended representation:

```text
QuotaFractionPpm:
0         =   0.000000%
1_000_000 = 100.000000%
```

The name says parts per million and not micros deliberately. `Micros` reads as
micro-percentage-points to at least half its readers, and a unit whose name has two
plausible meanings is exactly the defect this document exists to prevent.

Use similarly explicit scaled integer or rational representations for:

* fractional credits;
* credits-per-token coefficients;
* percentage-point conversion factors;
* money.

Floating point remains acceptable for statistical diagnostics such as condition
numbers and R², because those are not physical ledger quantities.

## 7.2 Usage is a vector, not one token number

Do not normalize every transcript immediately into a single “tokens” value.

The normalized usage object should retain token classes such as:

* input;
* output;
* cache read;
* cache write;
* other provider-specific classes when they genuinely exist.

Conceptually:

```text
UsageVector {
    input
    output
    cache_read
    cache_write
    ...
}
```

This is crucial to avoiding the current defect.

The cost model converts this vector into credits.

The valuation model separately converts the vector into money.

Neither conversion should be allowed to pretend an unrecognized token class is free.

## 7.3 Meter windows

A provider snapshot consists of a set of independently named quota constraints.

Each window contains at least:

* a stable semantic key;
* quota used;
* reset timestamp;
* nominal window duration;
* scope;
* provider/account identity;
* measurement timestamp.

Scope should distinguish at least:

* applies to all models on the account;
* applies to one model;
* potentially another explicitly named provider constraint.

For a chosen model, its lowest remaining fraction is:

> the minimum remaining fraction among every window that constrains that model.

This semantics should be implemented in one domain function and reused everywhere.

It should not be separately re-derived by the CLI, status renderer, and advisor.

That definition is a display concept, and it is not the feasibility calculation.
Window calibration is explicitly window-specific, so two windows may carry very
different credits per percentage point, and the window with the smallest remaining
fraction need not be the window with the smallest remaining workload capacity:

```text
Window A: 20 percentage points left
          100 credits per percentage point
          => 2,000 credits of headroom

Window B: 40 percentage points left
          10 credits per percentage point
          => 400 credits of headroom
```

A is the lower percentage and B is what actually stops a 500-credit task. Two
distinct domain functions are therefore defined:

* `lowest_remaining_fraction_window`, used by status and display;
* `limiting_credit_headroom_window`, used by workload advice.

`can-run` uses the second (§26.4). Nothing uses the first to decide whether work
fits.

## 7.4 Freshness

Use the three-way representation in §3.2.

A provider that has never yielded a successful observation after an attempted
sample is `Stale { last_good: None, reason: NoSuccessfulObservation | ... }`.

A configured provider for which no attempt has ever occurred is likewise stale from
the point of view of a current status request, but its reason is `SamplingGap`.

This keeps "no usable current reading" separate from authentication while preserving
the required exhaustive vocabulary.

## 7.5 Freshness changes with time

A subtle but important rule:

> A row that was fresh when written does not remain fresh forever.

The database stores the collection result and its measurement time.

The reader computes **effective freshness at read time** using the configured freshness policy.

A previously successful sample ages into `stale` even if no subsequent request occurred.

A more recent failed request also matters. Consider:

* 10:00, fresh meter response;
* 10:02, request times out;
* 10:03, status read.

The status must be stale because the newest attempt could not reach the source, even though the 10:00 value is young.

Authentication failures remain actionable, but only within the credential context
that produced them. A later generic timeout does not imply that expired credentials
were repaired. Neither does an old rejection say anything about credentials that have
since been replaced:

```text
10:00  old credential   → 401
10:05  operator replaces the credential
10:06  new credential   → endpoint times out
```

Concluding `auth_required` at 10:06 attributes a verdict to credentials that are no
longer in use. The rule is therefore scoped:

* a later attempt in the same credential context that fails before authentication
  retains the unresolved auth condition;
* a later attempt in a demonstrably different credential context invalidates the old
  auth conclusion, and until the new context succeeds the state is stale rather than
  fresh:

```text
Stale {
    reason: CredentialChangedUnverified,
    ...
}
```

The status path still resolves no credentials. It reads the credential-context IDs
that sampling attempts already persisted (§10.1).

Freshness also needs one explicit answer about which clock it is reading. Every meter
adapter declares the timestamp semantics of its provider contract, and freshness uses
the resulting typed measurement basis:

* provider observation time where the endpoint documents that field as the
  measurement time;
* otherwise local receive time;
* or the older of the two where the provider's semantics specifically require that
  conservative reading.

A provider timestamp outside the configured clock-skew envelope is a `ClockAnomaly`.
It is never a licence to manufacture a negative age or a freshness in the future.

Monotonic time governs in-process timeouts and the command budget. Wall-clock
timestamps never decide whether a blocking HTTP operation has exceeded its timeout,
because a clock adjustment mid-request would otherwise cancel or extend it.

This behavior belongs in one freshness state machine.

The state machine consumes:

* last successful observation;
* latest attempt, including its terminal result or the absence of one;
* the credential context of each of those;
* latest successful authenticated attempt;
* configured freshness horizon and clock-skew envelope;
* current clock.

The same function is used by:

* `now`;
* `status`;
* `can-run`;
* JSON output;
* tests.

The projection stores inputs to this state machine, not its final result.

## 7.6 Evidence qualification

Derived report values use `Derivation<T>` over the `Qualified<T>` defined in §3.7, so
a value that cannot be derived at all is representable without inventing a qualified
number for it.

Remote meter values add the freshness wrapper:

```rust
Freshness<Qualified<AccountMeter>>
```

This avoids forcing nonsensical authentication semantics onto local concepts such as
rate cards or transcript-derived token counts.

## 7.7 Semantic identifiers

Four things that a naive design would call "the version" are separate facts:

```text
aub adapter implementation v14
provider endpoint schema v3
physical quota semantics "account-5h-v2"
billing semantics "model-x-subscription-v4"
```

Calibration applicability depends on the last two and on neither of the first two. An
adapter refactor that changes no physical meaning must not invalidate a calibration,
and a provider that changes how a window works must invalidate it even when the Rust
code still parses the same JSON unchanged. Coupling physical truth to software
release numbers gets both of those wrong.

Introduce stable semantic identifiers, distinct from the `aub` binary version and
from adapter implementation versions:

```rust
MeterSemanticsId
BillingSemanticsId
ProviderContractId
```

They are recorded on observations, cost models and calibrations, and they are what
applicability is decided against.

---

# 8. Architectural structure

A single Rust package with a library plus thin binary entry point is sufficient.

There is no need to begin with a large Cargo workspace.

The conceptual modules are:

| Module | Responsibility |
| --- | --- |
| `domain` | quantities, IDs, freshness, window semantics, intervals, provenance |
| `evidence` | coverage and evidence-quality propagation, provenance graphs, source qualification |
| `config` | all paths, accounts, credentials, sampling policy, aliases |
| `store` | SQLite schema, migrations, repositories, transactions |
| `meter` | sampling orchestration and provider adapters |
| `auth` | configured credential acquisition |
| `projection` | construction and atomic publication of the status projection |
| `transcripts` | recursive discovery and source-specific parsers |
| `dedup` | one canonical replay-deduplication implementation |
| `sessions` | normalized session timelines |
| `attribution` | account, project, repository, and task attribution |
| `cost_model` | usage-vector → credits |
| `calibration` | credits ↔ quota-window relationship |
| `valuation` | usage-vector → API-price equivalent |
| `advice` | historical task distributions + live quota + calibration |
| `coverage` | expected-vs-observed sample opportunities and destructive gaps |
| `backup` | consistent archival snapshots of irreplaceable state |
| `report` | typed report models |
| `cli` | argument parsing and orchestration |
| `presentation` | human and machine-readable output |

The executable's `main` should contain almost no business logic.

## 8.1 Dependency direction

The lowest layers are:

```text
domain
evidence
config interfaces
```

They know nothing about:

* SQLite;
* HTTP;
* terminal formatting;
* transcript locations.

Adapters depend inward on those domain abstractions.

Workflows orchestrate adapters and repositories.

Presentation consumes typed report models and never performs physical-unit
arithmetic.

## 8.2 Forbidden dependencies

The architecture should be mechanically reviewed for these violations:

* `presentation` importing provider adapters;
* `transcripts` importing calibration;
* `meter` writing SQLite directly;
* `cost_model` reading configuration files directly;
* `advice` constructing calibration constants;
* `status` referencing the HTTP transport layer;
* provider adapters resolving arbitrary credential paths themselves.

---

# 9. Boundary rules

The architecture should impose several hard rules.

### Provider adapters do not know SQLite

A provider adapter receives:

* a resolved credential handle;
* request parameters;
* an HTTP client;
* a clock.

It returns a typed provider observation.

It does not write files or databases.

### Credential code does not know provider semantics

Credential resolution turns a configured credential source into provider authentication material.

It does not decide what a quota window means.

### Transcript parsers do not calculate costs

A transcript parser emits normalized usage events.

It must not know:

* subscription window capacity;
* API dollar pricing;
* task history;
* meter percentages.

### The calibration layer never parses transcripts

It receives normalized `UsageVector`/`Credits` and meter observations.

### Presentation receives already-typed report objects

Formatting code should not perform business arithmetic.

That prevents the status renderer from accidentally growing its own idea of remaining percentage.

### Quantities do not implement free-standing `Display`

Physical quantities are rendered through presentation helpers that require explicit
context such as:

* unit label;
* coverage and evidence quality;
* freshness when applicable;
* precision policy.

This makes it harder for a bare scalar to escape into a user-visible interface.

### Conversion witnesses are explicit values

Functions that convert:

```text
UsageVector -> Credits
Credits -> PercentDelta
UsageVector -> ApiListPriceEquivalent
```

must receive typed witnesses:

```text
CostModel
WindowCalibration
RateCard
```

respectively.

They cannot silently reach for global constants.

---

# 10. Configuration

Configuration is the only authority for local identity and paths.

A logical account configuration should contain things such as:

* account key/name;
* provider;
* credential source;
* expected plan tier if known;
* sampling interval;
* freshness threshold;
* enabled models if relevant.

It should also contain:

* provider adapter key;
* logical credential source;
* expected sampling cadence;
* optional account-identity assertion;
* optional known-machine exclusivity policy used only to determine whether passive
  calibration evidence is eligible.

Transcript configuration should contain:

* CLI/source kind;
* one or more roots;
* parser-specific options;
* logical project/repository mappings where needed.

State configuration should contain:

* SQLite location;
* spool/recovery directory if used;
* optional diagnostic log policy.

The account name is a local logical identity such as `work-main` or `personal-a`. It should not be inferred from whichever token happens to be in the current environment.

## 10.1 Credentials

The current problem of three tools independently deriving credential paths should disappear.

All credential resolution goes through `auth`.

A configured account might obtain credentials from:

* an explicit file path;
* a profile directory;
* a credential-helper command;
* a keychain-backed mechanism;
* an explicitly named environment variable where that is actually unambiguous.

Raw secret material must not be stored in SQLite.

Error messages must never print secret values.

Where feasible, a provider adapter may validate that the credential's provider identity has not unexpectedly changed. Any stored remote identity used for this purpose should be an opaque or hashed fingerprint, not required for display.

Prefer identifying the configured **credential source** rather than hashing secret
material.

For example, the profile-switching launcher already knows it selected logical
profile `work-a`; that fact is stronger and safer than hashing an access token.

If a credential fingerprint is unavoidable:

* use a keyed local HMAC rather than an unsalted raw hash;
* never store the credential itself;
* rotate the local HMAC key if the credential set is replaced;
* treat this attribution method as weaker than an explicit launcher or session marker.

Credential resolution therefore returns an identity alongside the material:

```rust
ResolvedCredential {
    material: Secret<AuthMaterial>,
    context_id: CredentialContextId,
}
```

`CredentialContextId` is safe to persist and identifies the credential revision
without exposing credential bytes. Prefer deriving it from source revision metadata,
such as the logical profile the launcher selected; use the keyed HMAC above only
where nothing else can distinguish one credential revision from the next.

It is persisted on every attempt, and it is what scopes a sticky authentication
failure to the credentials that actually produced it (§7.5).

## 10.2 Configuration resolution

`aub config` prints every resolved key with provenance:

```text
state.dir                  file:/.../aub.toml
account.work-a.credential  file:/.../aub.toml
sampling.interval          default
transcript.cli-a.root      flag
```

Resolution order is documented and deterministic:

```text
command-line override
→ explicitly supported environment override
→ config file
→ non-identifying platform default
```

This answers "which config won?" without source inspection.

---

# 11. Persistence design

## 11.1 One SQLite database

Use one database.

Do not create separate “meter.sqlite” and “spend.sqlite” stores unless future operational evidence demands it.

One database enables the high-value session/account join transactionally and avoids introducing synchronization between two local sources of truth.

## 11.2 WAL

Initialize the database in WAL mode.

Writer behavior:

* network work occurs outside transactions;
* write transactions remain short;
* a meter attempt, its successful observation and that observation's child windows commit atomically.

Reader behavior:

* short-lived read transaction;
* no waiting for writer completion under normal WAL operation.

`status` does not open SQLite at all.

All ordinary analytical readers use read-only connections and short snapshots.

Writers keep transactions short. Transcript ingest commits in bounded batches so it
cannot monopolize the single SQLite writer slot.

## 11.3 Durability

For irreplaceable meter writes, favor durability over a small amount of write latency.

A meter sample occurs every few minutes, not millions of times per second.

Start with:

```text
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
```

Every connection applies those connection-scoped settings and verifies that the
required journal mode is in effect rather than assuming an earlier process set it.

Core schema tables additionally use SQLite `STRICT` mode plus explicit `CHECK`,
`UNIQUE` and foreign-key constraints. The database is a module boundary like any
other, and SQLite's ordinary dynamic typing would otherwise accept values the Rust
types reject. Representative constraints:

```text
token_count >= 0
quota_fraction_ppm BETWEEN 0 AND 1_000_000
finished_at >= started_at where both clocks are trustworthy
one terminal result per attempt
one preferred interpretation per response evidence and semantics version
```

The Rust type system is not the only enforcement layer.

The write volume is tiny enough that weakening durability should require measurement
and a written justification.

`busy_timeout` for ordinary writers may be bounded, but meter data does not vanish
if that timeout is exceeded because successful network results first enter the
pending-observation spool (§13).

## 11.4 Schema migrations

Migrations are versioned and forward-only.

`aub status` must **never run a migration**.

A status-line execution that discovers an incompatible schema should immediately return a compact “schema upgrade required” condition.

Normal interactive/mutating commands may perform migrations under an explicit migration lock, or migration may be exposed as an administrative operation. The important requirement is that status rendering cannot unexpectedly take a schema lock.

Before any migration that rewrites irreplaceable meter tables:

1. create a consistent SQLite backup;
2. verify it;
3. perform the migration;
4. run integrity checks;
5. only then publish a new projection schema.

Append-only and add-column migrations should be preferred over destructive rewrites.

## 11.5 Retention policy

Default retention:

* meter attempts: forever;
* normalized meter observations and windows: forever;
* calibration evidence and results: forever;
* account and session markers: forever unless explicitly purged;
* transcript-derived normalized usage: rebuildable, configurable;
* ingest quarantine: configurable but retained by default;
* projection: one current file;
* optional debug payload captures: short retention only.

The tool never automatically discards quota history merely to save space.

## 11.6 Ledger generation

The database maintains a monotonically increasing `ledger_generation`.

Every transaction that changes projection-relevant durable meter state advances the
generation inside that same transaction.

The projection records the exact `ledger_generation` it was built from. No rowid,
timestamp, file mtime or WAL position is used as a substitute, because none of them
is a statement about what evidence the projection actually contains.

---

# 12. Recommended schema

The exact SQL can evolve, but the logical tables should be decided before implementation.

The exact SQL remains an implementation detail, but schema-level invariants are part
of the design. Authoritative physical quantities use integer and fixed-point columns
and `CHECK` constraints rather than unconstrained `REAL`.

## 12.1 Accounts

### `account`

Stable logical account identity:

* account ID;
* configured logical name;
* provider key;
* first/last observed timestamps.

Do **not** make a single mutable `plan_tier` column on this row authoritative.
Plan and tier are time-varying evidence and belong on observations or explicit
account state intervals.

No credentials are stored here.

## 12.2 Sampling runs

### `sample_run`

Represents one invocation or sampling batch.

Important fields:

* run UUID;
* trigger: timer, hook, manual, live;
* process start/end timestamps;
* `aub` version;
* configuration fingerprint;
* sampling-policy snapshot ID.

This is useful for diagnosing a batch in which all providers failed simultaneously.

### `sampling_policy_snapshot`

The immutable, non-secret resolved sampling policy:

* policy snapshot ID;
* effective observation time;
* account;
* ordinary cadence;
* freshness horizon;
* reset-edge policy;
* retry/backoff policy;
* command budget;
* policy algorithm version.

This table exists because `coverage` is otherwise a stronger claim than its inputs
can support. If cadence changes from five minutes to fifteen next month, last month's
coverage denominator is not today's configuration. If a `Retry-After` postponed the
next attempt, that interval was not a missed opportunity. If reset-edge logic changes
in a later release, old expected opportunities must still be reconstructed by the
algorithm that was actually in force. A configuration fingerprint identifies that the
policy differed; it does not say what it was.

## 12.3 Meter attempts

### `meter_attempt`

One immutable row per account/provider attempt, committed before outbound network
I/O begins.

Fields conceptually include:

* attempt UUID;
* sample-run UUID;
* account key;
* provider;
* request-start timestamp;
* credential-context ID;
* sampling-policy snapshot ID;
* due-at timestamp;
* due reason: ordinary cadence, reset edge, post-reset confirmation, forced/manual;
* prior attempt or result on which the due decision was based;
* provider contract ID and meter-semantics ID.

### `meter_attempt_result`

At most one terminal result per attempt:

* attempt UUID;
* completion timestamp;
* monotonic elapsed duration;
* terminal outcome;
* failure class where applicable;
* sanitized provider error classification;
* retry metadata where applicable.

The outcome is never written by mutating a null column on `meter_attempt`. The start
and the result are separate durable facts, so an attempt with no result survives as
evidence in its own right. Once its maximum plausible execution horizon has elapsed,
such an attempt is read as an interrupted collector: not an endpoint timeout, and not
a missing attempt.

A failed attempt is persisted.

That distinction matters because:

> “Nobody sampled for 60 minutes” is materially different from “we attempted every five minutes and the provider was unavailable.”

## 12.4 Response evidence and meter observations

### `meter_response_evidence`

Durable sanitized evidence of what the remote source actually returned:

* attempt UUID;
* HTTP/provider response classification;
* local receive timestamp;
* provider timestamp as originally represented, where present;
* sanitized provider-specific quota evidence capsule;
* capsule schema and sanitizer version;
* cryptographic content hash;
* capture completeness/truncation status.

The capsule is produced before semantic normalization and contains no credential
material. For JSON providers it can be canonical sanitized JSON holding the quota
response subtree and the raw source lexemes. Where the schema itself failed, retain
as much safely sanitizable evidence as possible plus the body hash.

### `meter_observation`

An immutable interpretation of one `meter_response_evidence` row, produced by a
particular adapter and semantics version:

* observation UUID;
* attempt UUID;
* response-evidence UUID;
* account;
* provider;
* provider-observed timestamp if supplied;
* local received timestamp;
* measurement basis declared by the adapter;
* observed plan/tier;
* adapter version;
* provider contract ID;
* meter-semantics ID;
* normalized-response fingerprint.

The attempt and observation are deliberately distinct: failure is evidence even when
there is no observation. Evidence and interpretation are distinct for the same
reason. If adapter v1 misunderstands the units of a valid field, an architecture that
persists only the normalized value and discards the response has turned a successful
collection into permanently wrong data. A corrected parser creates a new
interpretation against the same retained response evidence; it never overwrites the
earlier one.

## 12.5 Meter windows

### `meter_window`

Child rows of a successful interpretable observation:

* observation UUID;
* semantic window key;
* scope kind;
* optional scoped model;
* quota used as fixed-point integer;
* reported measurement resolution;
* reported rounding/quantization semantics where known;
* reset timestamp;
* nominal duration.

The reported value is stored exactly as the provider expressed it. The resolution
fields exist so that later mathematics knows what that value claims: a provider
displaying `41%` under round-to-nearest is asserting an interval, not a scalar, and
calibration that treats it as infinitely precise manufactures drift out of rounding
(§23.5).

Do not persist a separately computed “effective remaining” value as authoritative data. It is derived from the applicable windows.

## 12.6 Session/account markers

### `session_account_marker`

Fields:

* session ID;
* timestamp;
* source ordering key where available;
* logical account;
* marker source;
* optional run ID;
* optional confidence/evidence designation.

Timestamps alone cannot order two markers that share the source's timestamp
resolution, and account attribution turns on exactly that ordering, so a source
sequence number is preserved wherever the source provides one.

The existing status-line series can populate this table during migration.

Markers support account switching within one session rather than assuming a one-session/one-account invariant.

## 12.7 Transcript files

### `transcript_file`

A rebuildable index:

* source configuration key;
* relative file identity;
* size;
* modification metadata;
* content/parser fingerprint;
* parser version;
* last successful ingestion point.

Prefer storing paths relative to configured transcript roots.

## 12.8 Sessions

### `session`

Contains normalized session facts:

* session ID;
* source/CLI;
* start;
* end if known;
* project key;
* repository key;
* optional run ID.

A session should not contain a single mandatory account field because account assignment can change over time.

## 12.9 Usage events

### `usage_event`

A normalized, deduplicated usage event:

* canonical event ID/fingerprint;
* session ID;
* event timestamp;
* model identity;
* evidence kind;
* source record provenance;
* parser version.

### `usage_component`

Child rows:

* event ID;
* token class;
* `TokenCount`.

Using component rows instead of four permanently fixed token columns allows an
unknown future component to survive normalization rather than being silently
discarded.

Unknown token classes may be reportable even when no cost model knows how to value them.

## 12.10 Usage occurrences and deduplication evidence

The canonical logical event is `usage_event` (§12.9). Where that event was seen is
a separate table.

### `usage_occurrence`

Where a normalized source record appeared:

* transcript/file identity;
* source location/line/offset;
* parser version;
* canonical fingerprint;
* canonical event ID.

This permits `aub` to say:

> event X appeared 407 times in replayed transcript history but contributes once.

The database uniqueness constraint is the final deduplication authority.

Strong and heuristic identities use separate uniqueness domains. A source-provided
native ID is a claim; a heuristic fingerprint is an inference, and giving both the
same authority at the database boundary lets an inference silently overrule a fact.

For strong source identity:

```text
UNIQUE(source_namespace, native_event_id)
```

For sources with no stable identity, a parser-specific heuristic key may be unique
only within that parser's documented replay-equivalence domain.

Persist alongside each occurrence:

* identity strength;
* identity namespace;
* native ID where available;
* heuristic-key algorithm version where applicable;
* canonical payload digest.

The system should optionally retain duplicate-occurrence metadata or at least duplicate counters so a report can say how much replay material was discarded.

The fact that roughly 98,000 duplicate records can occur in a day makes deduplication part of the data model, not a parser afterthought.

Dedup strength is recorded:

* `Strong`: a stable source event or request ID exists;
* `Heuristic`: a canonical semantic signature is used because the source lacks a
  stable identifier.

Heuristic collisions are diagnosable and included in `doctor`.

## 12.11 Ingest quarantine

### `ingest_quarantine`

Records source material that could not be normalized:

* source file ID;
* offset/line;
* parser;
* failure class;
* excerpt hash by default;
* optional bounded redacted excerpt, only under an explicit diagnostic policy;
* first/last observed time.

Transcript excerpts carry far more privacy risk than usage counters do, so the
default keeps the hash and not the text: the hash is enough to recognise the same
failure recurring, which is what the quarantine is for.

Parse failures are not silently skipped.

## 12.12 Task events and attribution

### `task_event`

Normalized issue-tracker events:

* task ID;
* timestamp;
* event kind;
* session/agent association where available.

### `attribution_segment`

Rebuildable intervals:

* session;
* start;
* end;
* attribution target.

The target is either:

* a real task ID;
* an explicit overhead bucket.

## 12.13 Cost models

### `cost_model`

Immutable model identity:

* provider;
* model/model-class scope;
* billing-semantics ID;
* optional plan scope where billing semantics are plan-dependent;
* version;
* validity interval;
* published-at timestamp;
* provenance.

Activation and supersession are append-only lifecycle events, not a mutable column on
this row. A witness row that gets edited in place cannot answer what `aub` would have
said last month, and that question has to stay answerable (§12.14).

### `cost_model_term`

* cost-model ID;
* token kind;
* credits-per-token coefficient;
* uncertainty;
* derivation method;
* evidence experiment.

A complete active model must have a term for every observed known token component
that it claims to support.

## 12.14 Calibration data

Tables distinguish:

* cost model;
* calibration experiment;
* window-calibration candidate/result;
* append-only calibration activation/supersession events.

Every witness in this system carries two independent times, and conflating them makes
historical reports irreproducible:

```text
valid time      when this witness describes the physical world
knowledge time  when aub learned, published or activated it
```

An API price that actually took effect on 1 June but was imported on 12 August means
that a report produced on 1 July was right about what `aub` then knew and wrong about
the world. Both readings are legitimate questions, and both must remain answerable.
This does not require a general temporal database: immutable records plus append-only
activation events are enough.

A calibration result contains at least:

* provider;
* plan tier;
* window semantic key;
* meter-semantics ID;
* billing-semantics ID;
* cost-model ID;
* fitted credits per percentage point;
* equivalent full-window capacity;
* fit residual;
* uncertainty interval;
* lag estimate/handling metadata;
* sample count;
* fit timestamp;
* source experiment IDs;
* status.

Also persist:

* fitting evidence hash;
* validation evidence hash;
* validation method and version;
* out-of-sample residual diagnostics;
* activation-policy version;
* `aub` version and source revision;
* statistical method and parameters;
* condition number where a multivariate fit was used;
* observation coverage requirements;
* settling/plateau policy;
* explicit reason for excluded samples.

## 12.15 Rate cards

Rate-card data should be immutable and versioned.

A rate component includes:

* vendor;
* model;
* token class;
* monetary rate;
* currency;
* billing basis, e.g. per million tokens;
* effective start;
* effective end;
* imported/published-at timestamp;
* publication/source metadata.

A corrected price creates a new record rather than mutating history.

## 12.16 Sampling leases

### `sample_lease`

Short-lived operational coordination per account:

* account;
* holder UUID;
* acquired time;
* expiration.

This prevents timer, hook and manual races without a global sampler lock.

It is disposable operational state, not evidence.

---

# 13. Protecting meter observations against loss

Because meter values cannot be reconstructed, sampling should refuse to begin unless local persistence is viable.

Before making HTTP requests, `aub sample` should confirm:

* state directory exists;
* database can be opened;
* schema is usable;
* the process can create durable local state.

Network requests occur outside the actual database write transaction, but the result should be persisted immediately after parsing.

For additional crash protection, a small pending-observation spool is justified.

The flow is:

1. resolve the account and its sampling policy;
2. commit an immutable `meter_attempt` start record;
3. perform the request;
4. convert the result immediately to a sanitized terminal result and response
   evidence;
5. atomically write that terminal result to the pending spool in the state directory;
6. commit the terminal result, the response evidence and its interpretation into
   SQLite;
7. delete the pending record.

Step 2 comes before step 3 deliberately. If the process is killed, the machine
crashes, or the HTTP library wedges after the request begins, a design that creates
the attempt row from the response leaves nothing at all, and `coverage` then reports
that nobody looked at an interval where `aub` did look. That contradicts the claim
this system exists to make, so the ordering is an invariant and not an optimisation.

On startup, mutating commands replay any pending record whose UUID is not yet in SQLite.

If the process crashes after the SQLite commit but before deleting the spool, the UUID makes replay idempotent.

This does not eliminate the microscopic crash window between receiving a network packet and the first durable local write, but it reduces avoidable loss substantially.

Raw provider responses should not be spooled by default because they may contain account or request information that is unnecessary for the ledger.

The pending record contains the **normalized typed attempt and observation**, not the
raw HTTP body.

Use:

* write new file;
* `fsync` file;
* atomic rename into the pending directory;
* `fsync` directory where supported.

On startup, every mutating workflow first drains pending meter evidence into SQLite
before beginning lower-priority rebuildable work.

If SQLite remains busy, the pending evidence stays durable and `sample` reports an
infrastructure failure rather than discarding it.

## 13.1 Provider raw-body retention

Do not store authentication headers, cookies, credential-bearing request material, or
arbitrary unreviewed provider bodies.

Do retain a provider-specific sanitized meter evidence capsule by default where the
provider contract permits it. The capsule preserves the original source values needed
to reinterpret quota semantics after an adapter bug is found, which is the difference
between a repairable misreading and a permanent one. It is not an HTTP archive: it
holds the quota-relevant subtree, not the response.

Persist:

* sanitized quota-response evidence;
* normalized interpretations;
* adapter version;
* response/body hash;
* HTTP metadata needed for diagnosis;
* unknown-field and schema diagnostics.

An opt-in short-retention debugging facility may retain encrypted or aggressively
sanitized bodies if a specific provider proves impossible to debug otherwise.

Production parser contract tests use sanitized fixtures derived from real responses,
not a permanent archive of account-bearing response bodies.

---

# 14. Sampling strategy

## 14.1 Two kinds of trigger

Sampling comes from two sources.

### Periodic scheduler

A machine-native scheduler invokes:

```text
aub sample --due
```

frequently.

The command itself decides which configured accounts are due according to their last attempts and configured cadence.

This is preferable to baking one scheduler entry per account.

A reasonable initial policy is for the external scheduler to invoke `--due` every minute while each account has a configured target sample interval such as five minutes.

Those values are configuration, not source constants.

### Opportunistic hook

The profile launcher or agent hook invokes `aub` when it knows:

* account;
* session ID;
* optionally run ID.

The marker should be recorded immediately even when no provider sample is due.

If the account has been sampled recently, the network request can be skipped while the session/account marker is still preserved.

The hook form is:

```text
aub sample --account ACCOUNT --session-id SESSION --if-due
```

The session/account marker is written regardless of whether the network poll is due.

An optional `--run-id` records the friction-ledger join key.

## 14.2 Avoiding timer/hook stampedes

A small SQLite sampling lease per account can prevent:

* the periodic timer;
* a status hook;
* and a manual command

from all hitting the same endpoint simultaneously.

Lease acquisition occurs in a very short transaction.

The lease expires automatically in case the owning process crashes.

This is operational metadata, not measurement evidence.

The lease is per account, not global. Two unrelated provider accounts may be sampled
concurrently.

## 14.3 Scoped-thread concurrency

When several accounts/providers are due:

1. determine due work;
2. acquire per-account sampling eligibility;
3. launch bounded blocking HTTP requests using scoped threads;
4. join them;
5. persist each result.

One broken provider must not prevent another provider's observation from being committed.

Network concurrency is bounded even if many accounts are eventually configured.

Every request has:

* connect timeout;
* read timeout;
* total request timeout;
* overall command wall-clock budget.

The total-budget expiry is itself an `Unreachable::Timeout` attempt outcome for any
unfinished source.

## 14.4 Reset-edge sampling without a daemon

The previous observation supplies known reset timestamps.

`sample --due`, when invoked frequently by the external scheduler, treats an account
as due when:

* its ordinary sample interval expired; or
* a known reset is approaching within the configured edge lead and no sufficiently
  recent pre-reset sample exists; or
* a post-reset confirmation sample is due.

This preserves better reset-boundary evidence without creating a resident scheduler.

## 14.5 Retry policy

Sampling is evidence collection, not a request-success benchmark.

Defaults should therefore be conservative:

* no retry on authentication failure;
* no immediate retry on rate limit;
* at most a small bounded retry for connection establishment and transient transport;
* honor `Retry-After` in future due calculation;
* never let retries violate the command-wide wall-clock budget.

Every attempted request is represented in the final attempt outcome.

The resulting next-due decision is either persisted or deterministically
reconstructible from the attempt result plus its policy snapshot, so `coverage` can
tell a deliberately postponed interval from a missed one.

---

# 15. Coverage: the destructive-axis guard

`coverage` is a first-class product feature, not merely a `doctor` subsection.

The failure the system can never repair is an interval during which nobody sampled.

`aub coverage --since ...` computes two distinct quantities over periods for which a
sampling policy is known:

### Attempt coverage

Did `aub` begin the sampling opportunities it was supposed to begin?

A no-attempt interval covers a scheduler that died, a sleeping laptop, an unavailable
state directory and an `aub` that was never invoked. `aub` reports the gap; it does
not claim to know which of those caused it, because nothing in the attempt table
distinguishes them (§20.3).

Coverage separately reports started attempts that never acquired a terminal result.
Those are collector or process interruption, which is a different fact from provider
unavailability and must not be folded into it.

### Measurement coverage

Of attempted sampling opportunities, how many produced valid provider observations?

This distinguishes:

* authentication outage;
* rate limiting;
* provider outage;
* parser or API-schema breakage.

Per account and window, report:

* expected sampling opportunities;
* attempted opportunities;
* successful observations;
* attempt coverage;
* measurement coverage;
* started attempts with no terminal result;
* longest no-attempt gap;
* longest no-observation gap;
* gaps spanning a known quota reset;
* most recent timer-triggered run;
* most recent successful observation.

Denominators are reconstructed from the sampling-policy snapshot that was in force
during the interval (§12.2). A later configuration change never rewrites historical
expectations, and an interval with no historical policy snapshot is reported as
`policy_unknown` rather than silently evaluated against today's configuration.

Intervals spanning resets are marked severe for calibration purposes because the
window's peak consumption may have been lost forever.

`coverage` has a configured threshold and a non-zero threshold-breach exit status,
making it suitable for an external notification mechanism without turning `aub`
into a daemon.

---

# 16. The status-bar path

The status path deserves its own architecture because its requirements differ from
interactive commands.

## 16.1 Projection publication

After every committed transaction that can change status-visible meter state, which
includes sampling batches containing authentication or network failures, pending-spool
recovery, meter import and explicit repair, publish a projection for the resulting
`ledger_generation`:

1. commit or spool all attempt evidence;
2. construct the projection from durable database state;
3. write `projection.tmp`;
4. `fsync` it;
5. atomically replace `projection`;
6. sync the containing directory where supported.

The database commit always precedes publication. A crash may therefore leave the
projection older than SQLite, and must never leave it claiming evidence newer than
the database.

The projection contains, per account:

* last successful observation timestamp and windows;
* latest attempt timestamp with its terminal outcome, or the fact that it has none;
* logical account and provider identity;
* the credential context of the latest attempt;
* projection/schema version;
* source `ledger_generation`.

It does **not** contain:

* `fresh=true`;
* calibration constants;
* computed historical spend;
* token valuation;
* raw credentials;
* provider raw bodies.

## 16.2 Status contract

`aub status` performs:

* minimal configuration sufficient to locate the projection;
* one bounded local file open and read;
* freshness computation;
* formatting.

It must not perform:

* HTTP;
* SQLite open;
* transcript discovery;
* transcript parsing;
* calibration fitting;
* rate-card updates;
* schema migrations;
* database writes;
* lock waiting.

This makes "status never blocks on another `aub` operation" structurally testable.

A provider value that aged past its freshness threshold is rendered as stale.

If the projection is missing, malformed, unsupported, or too old:

```text
aub ?
```

with a compact reason when the output mode permits.

The projection cannot become an alternate source of truth because:

1. it is rebuildable from SQLite;
2. all values point back to observation and attempt IDs;
3. it contains timestamps rather than stored freshness;
4. `doctor` compares it against SQLite;
5. any successful repair simply recreates it.

## 16.3 Status exit behavior

`status` always exits zero unless argument parsing itself fails.

Status bars often interpret a non-zero exit as process failure and suppress useful
degraded-state output. Fresh, stale and auth state is therefore conveyed in output,
not through process failure.

---

# 17. Transcript ingestion

Transcript data is reconstructible, so its pipeline should optimize correctness and auditability first, then speed.

## 17.1 Recursive discovery is mandatory

Transcript scanning must recurse beneath configured roots.

Subagent transcripts are first-class input.

A parser's test corpus must contain nested subagent examples so the non-recursive regression cannot return unnoticed.

Discovery does not follow symlinks by default and applies a configured maximum walk
depth so a bad root cannot accidentally recurse through an entire filesystem.

## 17.2 Incremental ingestion

`aub spend` may refresh the local transcript index before producing a report.

Unchanged files do not need reparsing.

The index records sufficient parser/file fingerprints to determine whether:

* a file is unchanged;
* a file was appended;
* a parser version changed;
* a full rebuild is required.

A full derived rebuild is always available because transcripts remain authoritative.

For append-mostly JSONL sources, retain an ingestion watermark based on a
platform-abstract file identity plus:

* size;
* mtime;
* consumed byte offset;
* parser version.

If the file shrinks or identity changes, re-read from the beginning. Canonical
event-level deduplication makes this safe.

A trailing partial line is not consumed until it becomes complete.

## 17.3 Parser adapters

Each CLI format has one parser adapter.

Adapters translate source records into normalized events and must explicitly classify evidence:

* provider/CLI reported;
* reconstructed;
* derived.

For a transcript with no usage fields, character-count reconstruction is not treated as a measured token count.

It is a reconstructed estimate with a named algorithm version.

If that estimator has a defensible uncertainty range, retain that range. Do not erase “estimated” while normalizing.

Every parser has:

* explicit input-format version assumptions;
* sanitized golden fixtures;
* mutation and unknown-field tests;
* a compatibility failure mode that sends input to quarantine rather than silently
  dropping it.

---

# 18. Central deduplication

Deduplication should occur once, after source parsing and before usage events enter the canonical store.

The dedup module owns the definition of event identity.

Preferred identity order:

1. a stable source-provided record/event ID;
2. otherwise a source-specific canonical fingerprint of stable semantic fields.

Never use the entire **file hash** as the usage deduplication key. Replayed usage
occurs inside otherwise distinct transcript files and even inside the same file.

The fingerprint must not include the transcript filename when that would stop duplicate replay records in different files from matching.

Likely fingerprint inputs include:

* adapter namespace;
* session;
* message/event identity where available;
* timestamp or sequence;
* model;
* canonical usage counters.

The exact rules are parser-specific but implemented through one dedup framework.

Canonical fingerprints exclude:

* source pathname;
* ingest time;
* line number when a stable record identity exists.

They include enough semantic context to avoid collapsing independent equal-sized
requests.

Where a source provides no stable request or message ID, its adapter must document
its heuristic identity and report `DedupStrength::Heuristic`.

Heuristic deduplication fails visibly rather than quietly. The realistic collision
here is semantic, not cryptographic: two genuinely independent requests can share a
model, a timestamp and a token count. If two occurrences map to one heuristic key but
normalize to materially different semantic payloads, do not pick one. Record a
`dedup_collision` diagnostic, quarantine the affected pair, and mark the affected
aggregate incomplete where the difference matters. Silent selection here is an
undercount, and an undercount is indistinguishable from correct output.

The heuristic key should reach for the strongest available replay discriminator, such
as source sequence or message ancestry, before falling back to timestamp plus usage
counters.

Cumulative counters require special treatment:

1. deduplicate cumulative records first;
2. order the surviving records;
3. derive deltas.

Doing this in the opposite order can double-count replay.

Dedup statistics should be visible diagnostically. If 98,004 duplicates are eliminated, that fact is valuable evidence that the system is doing material work.

`aub spend --explain` and `doctor` can report:

```text
canonical usage records: 4,113
replayed occurrences:    98,004
heuristic identities:       317
```

---

# 19. Session, project, and repository attribution

## 19.1 Session identity

Session ID is a first-class typed identifier and should be normalized immediately.

It is namespaced by its source. A native session ID from one CLI must never collide
with a textually identical ID from another:

```rust
pub struct SessionId {
    source: SourceNamespace,
    native: NativeSessionId,
}
```

The same applies to every task and run identifier that originates outside `aub`.

It is one of the most valuable existing joins because it already appears on both the spend and account/meter sides.

## 19.2 Account attribution

Do not add `account` directly to every session and assume it is fixed.

Instead, account markers form a timeline.

For a marker sequence:

```text
10:00 account-a
10:40 account-b
```

usage before 10:40 belongs to `account-a` and usage afterward belongs to `account-b`, subject to timestamp resolution.

If only one marker exists, it applies forward within the session according to clearly documented marker semantics.

When no marker can justify an account assignment, usage goes to an explicit `unknown-account` bucket.

It is never assigned to “the currently active profile.”

Account evidence is ranked:

1. explicit session/account marker from launcher or hook;
2. explicit provider/account identity returned during that session;
3. configured credential-source identity with validated mapping;
4. conservative temporal inference;
5. unattributed.

The evidence class is persisted and appears under `--explain`.

By default, inferred attribution is excluded from calibration.

## 19.3 Project and repository

Project and repository should also be typed logical identities.

Configured aliases are preferred to embedding full machine paths in report identity.

A source may provide a working directory, from which a resolver can determine the configured project or repository.

Unmapped work stays `unknown-project` or `unknown-repository`; it does not disappear from totals.

---

# 20. Headless, non-interactive, and web work

This requires distinguishing **quota coverage** from **token attribution coverage**.

## 20.1 Headless local runs

Periodic account sampling captures their quota effect even if no interactive shell hook runs.

If the headless agent still writes a transcript, spend can later be reconstructed.

To attribute that transcript to the correct account, the launcher should provide an automatic account/session marker when starting the run.

That does not make `aub` an account manager. The launcher merely tells the observer what already happened.

## 20.2 Web runs

A web run may have:

* account-level quota movement;
* no local transcript;
* no local session ID.

The sampler still preserves the provider-meter series.

`aub` must not manufacture corresponding token records.

Reports can therefore distinguish:

* locally explained consumption;
* provider meter movement without locally attributable transcript evidence.

That discrepancy is itself useful.

It must be called a **residual**, not "web spend," because the difference can also be
caused by:

* provider accounting lag;
* quantization;
* cross-machine use;
* missing transcript roots;
* calibration error;
* plan or accounting changes.

## 20.3 Sampling gaps

Because a missing sample is permanently missing evidence, `aub doctor` should inspect expected cadence.

It should distinguish:

* no sampling process ran;
* sampling started and the collector never finished;
* sampling ran and provider transport failed;
* authentication was required;
* valid observations existed but exceeded freshness policy.

The distinction depends on persisting sampling attempts, not just successful meter values.

`aub` does not claim to know why a no-attempt gap occurred. Without an independent
host-availability signal, laptop sleep, scheduler failure, a process that failed to
launch and a deliberately disabled timer are one indistinguishable condition. Adding
such a signal is not currently justified; claiming the distinction without it is a
fabrication, so the reports say no-attempt interval and stop there.

---

# 21. Per-task attribution

Task attribution should be based on explicit temporal segmentation rather than proportional smearing.

Suppose claim events are:

```text
09:10 claim TASK-1
09:45 claim TASK-2
10:30 claim TASK-3
```

Then the basic intervals are:

```text
09:10 <= t < 09:45 → TASK-1
09:45 <= t < 10:30 → TASK-2
10:30 onward       → TASK-3
```

If the tracker has explicit release/completion events, those create additional boundaries.

The important rule is:

> Ambiguity becomes overhead, not invented attribution.

Recommended overhead buckets include concepts such as:

* before first claim;
* after explicit release with no next claim;
* ambiguous boundary;
* missing timestamp;
* unmapped session;
* tracker unavailable;
* `contended`, for overlapping claims where attribution would require invention;
* `unclaimed_session`, for a session that has usage but no task claim at all.

If one usage record represents a cumulative delta spanning a task boundary and there is no principled way to split it, do not divide it in proportion to wall-clock time. Put it into a boundary-ambiguity bucket.

This gives task totals that are smaller but defensible.

The overhead buckets should be visible alongside task consumption.

An invariant must hold:

```text
sum(task-attributed usage)
+ sum(overhead usage)
= total canonical usage in the selected session set
```

No remainder is tolerated.

---

# 22. The cost model

The existing term “credits” needs one authoritative definition.

A **cost model** maps a usage vector to subscription-credit units.

Conceptually:

```text
credits =
    input_tokens      × input_weight
  + output_tokens     × output_weight
  + cache_read_tokens × cache_read_weight
  + cache_write_tokens× cache_write_weight
  + ...
```

Those weights are versioned reference data.

If cache-write tokens exist in the usage vector and the selected cost model has no cache-write term, the calculation is incomplete.

It must fail closed rather than implicitly using zero.

This is the direct architectural fix for the original floor-producing calibration.

A cost model has an immutable identity such as:

```text
provider / cost-model-v3
```

Every calibration names the exact cost-model ID used.

## 22.1 Do not automatically fit everything jointly

The cost model and window size are physically distinct unknowns.

A multivariate fit across token kinds is useful when a token-kind coefficient itself
is unknown, but it introduces identifiability problems. Input and cache usage may be
highly correlated in ordinary agent traffic.

Therefore:

* known and measured token-kind coefficients remain separate cost-model evidence;
* window calibration normally consumes an already-complete cost model;
* a joint experiment is permitted only when deliberately varied workloads make the
  individual coefficients identifiable;
* ill-conditioned multivariate fits are rejected rather than published.

When multivariate fitting is used, record:

* method;
* regularization if any;
* coefficient standard errors and intervals;
* condition number;
* non-negativity constraints;
* experimental phase design.

An unidentifiable coefficient is an unavailable fact, not zero.

---

# 23. Calibration

Calibration should become a versioned experiment rather than a number emitted by a one-shot script.

## 23.1 What is being estimated

Keep two concepts separate:

1. the cost model converts token classes into credits;
2. the window calibration relates credits to percentage movement for a particular plan/window.

If a full window is `W` credits:

```text
credits per percentage point = W / 100
```

Only one form needs to be persisted as canonical; the other is derived.

The important point is that there is one record.

## 23.2 Calibration scope

A calibration is valid only for the dimensions demonstrated by its evidence.

At minimum:

* provider;
* plan tier;
* window type/semantic key;
* cost-model version.

It must not mix plan tiers.

If a plan changes, the observations on opposite sides of that boundary belong to different calibration strata.

## 23.3 Calibration state machine

A good interactive surface is:

```text
aub calibrate begin ...
aub calibrate status ...
aub calibrate end ...
aub calibrate fit ...
aub calibrate show ...
aub calibrate history ...
```

`begin` records:

* account;
* plan tier;
* target window;
* cost-model version;
* baseline meter observation;
* start timestamp;
* an explicit assertion that the account is reserved for the controlled calibration.

`aub` does not enforce exclusivity, but records the experimental premise.

During the burn, the ordinary scheduler keeps sampling.

`end` records the end of controlled local work.

Sampling continues afterward so server-side accounting can catch up.

`fit` operates on the recorded meter series and the controlled transcript sessions.

The forty-minute process therefore does not require a forty-minute resident `aub` process.

## 23.4 Two calibration modes

### Mode A: controlled experiment

This remains the authoritative cold-start path and the fallback when observational
data cannot establish exclusivity.

The workload should deliberately vary token mixes when cost-model terms are being
estimated:

* input-heavy/fresh-context phase;
* cache-write-heavy phase;
* cache-read-heavy phase;
* output-heavy phase.

The goal is not merely to burn quota. It is to generate identifiable evidence.

Sampling cadence is temporarily tightened by invoking `sample --due` more
frequently through the external scheduler or explicit calibration loop invocation.
`aub` itself still runs, answers, and exits.

### Mode B: passive candidate and validation fit

Once the meter series exists, ordinary work can produce valuable calibration
evidence.

Passive calibration is **not assumed valid merely because session IDs can be
joined**.

An interval is eligible only when:

* account attribution is high-confidence;
* the relevant meter window has observations on both sides;
* no reset falls inside;
* plan tier is unchanged;
* all contributing usage is exact;
* no unknown token components exist;
* meter coverage around the interval is sufficient;
* no known second local session or account consumer overlaps;
* the account's exclusivity policy permits passive fitting;
* and server-side settlement criteria are satisfied.

Passive fitting produces a **candidate** by default.

It does not automatically replace the active calibration.

The candidate is useful for:

* detecting drift;
* cross-checking controlled fits;
* reducing the frequency of controlled burns;
* eventually becoming activatable after enough real-world validation.

This is intentionally more conservative than assuming ordinary traffic is a clean
experiment.

## 23.5 Accounting lag

Provider accounting lag must be modeled rather than ignored.

The calibration record should retain:

* local cumulative credits over time;
* provider meter observations over time;
* provider measurement resolution and quantization intervals;
* reset boundaries;
* provider measurement timestamps when available.

Calibration mathematics treats a quantized provider reading as an admissible interval
wherever its resolution is known, rather than as an infinitely precise scalar. A
whole-percentage display under round-to-nearest is a source observation centred on 41
points carrying a corresponding quantization interval, and the reported value is
still retained exactly. Fitting to the scalar invents drift and residual alarms out
of the provider's rounding.

The preferred controlled-burn strategy avoids needing to assign individual meter
increments to individual requests.

Use **settled boundaries**:

1. establish a stable baseline plateau;
2. perform the controlled workload;
3. stop local workload;
4. continue sampling;
5. wait until the meter satisfies a configured plateau/settlement criterion;
6. fit cumulative local credits against the total settled meter delta.

Within-burn server lag then mostly cancels.

If the terminal plateau does not occur within the experiment's maximum settlement
window, the experiment remains incomplete and no fit is published.

For passive evidence, prefer intervals bounded by sufficiently settled meter regions
over naïvely pairing every adjacent sample.

An optional future lag model may estimate alignment statistically, but the first
release should not depend on a sophisticated deconvolution of delayed provider
accounting.

If a regression is used after qualifying evidence, conceptually:

```text
meter percentage-point change = intercept + slope × credits
```

From the slope derive credits per percentage point.

A robust regression is preferable to ordinary least squares if provider meters are quantized or updated in batches.

The fit should report at least:

* coefficient;
* equivalent full-window capacity;
* residual in percentage points;
* estimated/selected lag;
* uncertainty interval;
* number of usable observations;
* observations excluded and why.

A large fitted intercept is diagnostic evidence of contamination, lag mismatch, or an incomplete cost model.

## 23.6 Post-burn settlement

After the controlled work stops, the meter may continue moving because the provider is catching up.

The calibration should not declare the terminal meter value settled merely because one request returned.

A settlement policy can require several appropriately spaced observations with no material change.

The exact threshold belongs to calibration configuration and is recorded with the experiment.

## 23.7 Detecting contamination

Exclusive account time is a necessary experimental assumption.

`aub` can additionally flag evidence against that assumption.

For example:

* quota moves during a pre-burn idle period;
* quota keeps moving far beyond the expected settlement interval;
* local controlled credits are flat while provider meter movement is substantial;
* another locally known session is marked against the same account.

Those conditions should cause the fit to be rejected or explicitly marked contaminated.

## 23.8 Cache-write completeness

No new window calibration becomes active unless its referenced cost model covers every token class present in the calibration workload.

This is the critical difference from the current floor.

If the cache-write coefficient itself needs refitting, that is a cost-model experiment rather than something hidden inside window-capacity calibration.

A multi-term cost model must also reject statistically underidentified experiments. If cache-write and another token class always move in the same proportion, there is insufficient evidence to estimate independent coefficients.

## 23.9 Calibration validity and health

An active calibration is immutable.

It may become unusable because:

* plan tier changed;
* provider or window semantics changed;
* the referenced cost model was superseded for good reason;
* passive validation shows statistically significant drift;
* the configured review horizon expired.

An adapter implementation upgrade alone never invalidates a calibration. Only a
change to the corresponding semantic identifier does (§7.7).

Do not simply "widen the range because the calibration is old" unless there is an
actual statistical model supporting that widening.

Instead distinguish:

* `Provisional`;
* `Current`;
* `ReviewDue`;
* `Suspect`;
* `Superseded`;
* `Inapplicable`.

`can-run` requires a `Current` applicable calibration by default.

A review-due or suspect historical calibration may still be displayed by
`calibrate show`, but it does not silently power a current routing recommendation.

---

# 24. Calibration publication

A fit and an active production calibration are not necessarily the same thing.

The lifecycle should be:

```text
experiment
→ candidate fit
→ independent validation
→ active calibration
→ superseded calibration
```

Validation means independent evidence. A fit does not become validated by having its
residuals computed against the observations it was fitted to, which is a measure of
how well the fitter interpolates and says nothing about whether the coefficient is
right. Training and validation evidence IDs must therefore be disjoint for ordinary
activation:

```text
controlled experiment A            → candidate
controlled experiment B, or an
  eligible later passive interval  → validation
explicit activate                  → Current
```

A single controlled fit may be published deliberately as `Provisional`. It never
becomes `Current` on its own. This matters more once `can-run` exists, because a
calibration then makes decisions rather than filling a report.

Activation is an explicit action.

The calibration record is always immutable. Activation and supersession are separate
append-only facts, never an edited flag on the record, so which coefficient was in
force on a past date stays reconstructible.

A refit creates a new record and a new activation event.

The old record remains available so historical advice and reports can explain which coefficient was in effect.

There is never a workflow that says:

> “Copy this number into another source file.”

That operation simply does not exist.

Every result stores an `inputs_hash` over the exact evidence IDs it consumed.

Running the same fitter version on the same evidence should therefore be
reproducible and recognizable as the same fit.

---

# 25. The dollar-cost axis

The dollar axis belongs in `aub`, but with a narrower and more precise meaning than “cost.”

## 25.1 Why it belongs

It is derived from exactly the same normalized usage vector.

Keeping another standalone parser and pricing tool would recreate:

* duplicate transcript handling;
* duplicate token-class semantics;
* another place for cache handling to diverge.

So the computation belongs in the shared ledger.

## 25.2 Why it must remain separate from subscription quota

Published API pricing and subscription-meter consumption are different economic systems.

A token can simultaneously have:

* a subscription-credit impact;
* an API list-price equivalent.

Those are not interchangeable currencies.

Therefore no generic `Cost` type should represent both.

Use explicit concepts such as:

* `Credits`
* `ApiListPriceEquivalent`

Human output should say what the monetary figure represents.

Prefer:

```text
API list-price equivalent
```

or:

```text
counterfactual API cost
```

Never print a bare `$12.40 cost` for subscription traffic.

## 25.3 Rate cards are data

Do not compile vendor price literals into Rust source.

Rate cards are imported as immutable records with:

* effective dates;
* source/publication information;
* model;
* token class;
* currency;
* price.

When a vendor changes pricing, add a new version.

Historical traffic is valued using the rate effective at the event time.

Rate cards are temporal reference data, but they do not use the meter's
`auth_required` freshness enum because authentication is nonsensical for a local
price book.

Instead they carry:

* effective interval;
* source/publication reference;
* imported/verified timestamp;
* review-due policy.

This preserves semantic precision rather than forcing every time-sensitive object
through one enum.

A rate file on disk is an import source, never a runtime witness. Valuation reads the
immutable versioned rate-card record in the database, like every other consumer
resolving a witness centrally; it does not read a TOML file at calculation time. A
configured path names the book to import, and importing it is an explicit operation:

```text
aub rate-card import rates.toml
aub rate-card show
```

## 25.4 Incomplete valuation

If a period includes a model/token class without a matching rate, the normal headline total becomes unavailable.

It is acceptable to show something explicitly named:

> known-price subtotal

but it must not look like the complete answer.

The default `aub spend` report can omit dollar valuation entirely unless requested.

The feature is opt-in, e.g.:

```text
aub spend --value api-list
```

---

# 26. “Can I run this now?”

This is the first feature that genuinely joins both axes.

It should be implemented only after:

* transcript spend is trustworthy;
* account attribution works;
* calibration is versioned;
* meter freshness is reliable.

## 26.1 Inputs

The advisory requires:

* task kind;
* completed historical tasks of that kind;
* normalized usage of those tasks;
* applicable cost model;
* current account;
* current plan tier;
* applicable calibration;
* a fresh current meter snapshot.

By default, task-history samples containing estimated tokens, unknown account
attribution, unknown token components, or incomplete task segmentation do not enter
the reference distribution.

## 26.2 Historical task distribution

Do not collapse history immediately into one average.

For the selected task kind calculate:

* sample count;
* empirical median credits;
* a documented empirical central range;
* an upper reference quantile.

For example:

```text
median
p25–p75 observed range
p90 upper historical reference
```

These are empirical history statistics, not a probabilistic forecast of the next
task.

The exact default quantiles should be documented. A central range such as 20th–80th or 25th–75th percentile is easier to interpret than pretending an arbitrary interval is a formal prediction interval.

The report always labels the sample size and historical selection period.

## 26.3 Calibration uncertainty

For every provider window that constrains the selected model, resolve that window's
applicable current calibration, and convert the window's remaining percentage points
into a credit-headroom interval:

```text
window credit headroom
    = remaining percentage points
    × credits per percentage point for that window
```

Calibration uncertainty propagates into that interval rather than the conversion
using a point coefficient alone.

This is one reason the calibration result needs an interval and residual.

## 26.4 Current remaining quota

The target model may have several applicable provider windows.

For each:

```text
remaining = 100% - used
```

For status and display, the smallest remaining percentage may be identified.

For advice it may not. Windows are calibrated independently, so remaining percentage
is not a common unit: the window with the smallest remaining fraction can hold the
largest amount of work (§7.3). The limiting advisory constraint is the window with
the smallest defensible credit headroom, and finding it requires converting every
applicable window into credits first.

The report should therefore distinguish:

* the lowest remaining percentage window;
* the limiting calibrated workload window.

They may be different, and when they are, the second one is the answer.

If any constraining window has no applicable current calibration, the quantitative
verdict is `UNKNOWN`. An uncalibrated window is not assumed unable to bind.

## 26.5 Result

For each applicable constraint `w`:

```text
headroom[w] = remaining_points[w] × calibration[w]
margin[w]   = headroom[w] - historical_task_credits
```

The verdict is evaluated against every applicable window, and the limiting one is the
smallest margin rather than the smallest percentage.

The user-facing result should resemble conceptually:

```text
Historical task evidence: 14 completed tasks
Median: ...
Observed task range: ...–... credits

Constraining windows:
  5-hour account window:  ...% remaining, calibration #..., headroom ...–... credits
  model-specific window:  ...% remaining, calibration #..., headroom ...–... credits
  lowest remaining percentage: ...
  limiting calibrated window:  ...

Remaining margin after comparable work, per window:
  ...–... credits
```

The final conclusion is an interval and comparison such as:

> Comparable tasks consumed 550–940 credits; the limiting window is the weekly
> model window with 1,040–1,180 credits of calibrated headroom, leaving an
> evidence-based margin of 100–630 credits.

Where the lowest-percentage window and the limiting window differ, both are shown,
because a reader who only sees the percentage will draw the wrong conclusion from
it.

It does **not** say:

> “This will cost 7.4%.”

Nor should it say "safe" as if `aub` were an enforcement system.

Recommended verdict vocabulary:

* `AMPLE`: the upper historical reference fits with substantial current margin;
* `MARGINAL`: the median fits but the upper historical reference approaches or
  exceeds the current remaining amount;
* `INSUFFICIENT`: even the median historical consumption exceeds current remaining;
* `INSUFFICIENT_EVIDENCE`: history is too weak;
* `UNKNOWN`: current quota or calibration cannot be justified.

The exact classification thresholds are policy data and must be documented and
configured if these labels are enabled. The underlying interval is always printed.

## 26.6 Insufficient evidence

`can-run` refuses a quantitative answer when required evidence is absent.

Examples:

* current meter is stale;
* authentication required;
* any constraining window has no applicable calibration;
* cost model lacks one token class;
* plan tier does not match calibration;
* too few historical tasks for the configured range policy;
* task records are mostly unattributable.

It should explain every missing prerequisite in the same invocation.

Do not substitute:

* global average task cost;
* a different plan tier's calibration;
* estimated-token sessions;
* last week's stale meter;
* API-list-price conversion.

## 26.7 Live versus cached

Because the command asks “now,” the default should perform a fresh persisted meter sample for the selected account before advising.

An explicit cached mode may use a persisted reading, but only if it still satisfies freshness policy.

The status-line command remains network-free.

## 26.8 Reset timestamps

`can-run` may display:

```text
limiting window resets at 16:20
```

but does not reason that the proposed task will finish before or after that reset.

Run duration belongs to the separate friction ledger and is explicitly outside this
measurement system.

---

# 27. Recommended CLI surface

The original sketch is directionally right, but calibration should be treated as a versioned subsystem rather than one singleton “coefficient” command.

## `aub sample`

Operational collection command.

Representative modes:

```text
aub sample --due
aub sample --account work-a
aub sample --all
aub sample --account work-a --session-id SESSION
```

Responsibilities:

* record session/account marker if supplied;
* decide whether sampling is due unless forced;
* persist the attempt start;
* contact provider;
* persist the terminal result and any response evidence;
* persist successful windows;
* return explicit state.

This is the command invoked by timers and hooks.

Scheduled mode regards remote failures as **successfully recorded evidence**. The
command exits non-zero for inability to persist or operate, not merely because the
provider answered with an authentication or transport failure. `coverage` is the
mechanism for alarming on prolonged source failure.

That contract is right for a timer and surprising for a human script, so a caller
that requires every requested remote observation to have succeeded asks for it:

```text
aub sample ... --require-success
```

Under that explicit mode, authentication and remote-unavailable outcomes are still
durably recorded first, and then reported through their ordinary non-zero live-source
exit classes.

## `aub now`

Human-facing live meter report.

```text
aub now
aub now --account work-a
```

It forces a persisted sampling attempt, then renders the resulting current state.

There is no mode in which `now` fetches evidence and intentionally discards it.

## `aub status`

Strict low-latency status-line API.

```text
aub status
aub status --account work-a
aub status --model MODEL
aub status --format json
```

No network. No writes. No SQLite.

## `aub spend`

Transcript-side report.

```text
aub spend --today
aub spend --from ... --to ...
aub spend --group-by day
aub spend --group-by session
aub spend --group-by project
aub spend --group-by repo
aub spend --group-by account
aub spend --group-by task
aub spend --value api-list
```

A request may permit multiple grouping dimensions.

Transcript refresh policy is explicit rather than implied:

```text
--refresh auto   # default
--refresh never
--refresh force
```

Every report identifies the transcript-ingestion generation it consumed.

## `aub ingest`

Explicit ingestion of rebuildable sources:

```text
aub ingest transcripts
aub ingest transcripts --source cli-a
aub ingest transcripts --changed-only
```

This is optional for interactive use, because `aub spend` keeps its automatic
incremental refresh. It exists because parsing and reporting being one inseparable
step costs operational ability: CI can validate parsers without running a report, a
scheduler can precompute ingestion, `doctor` can name a concrete repair action,
benchmarks can separate parsing from aggregation, and a report can be reproduced
against a fixed ingestion generation.

## `aub rebuild`

Explicit destructive rebuild of rebuildable materializations:

```text
aub rebuild transcripts
aub rebuild attribution
```

`rebuild` is structurally forbidden from deleting meter attempts, attempt results,
response evidence, observations, calibrations, or any other irreplaceable or
reference evidence. That is a property of what the command can address, not a rule it
is trusted to follow.

## `aub coverage`

```text
aub coverage --since 30d
aub coverage --account work-a
aub coverage --severe
```

Reports attempt and measurement coverage on the destructive quota series.

## `aub calibrate`

```text
aub calibrate begin
aub calibrate status
aub calibrate end
aub calibrate fit
aub calibrate show
aub calibrate history
aub calibrate activate ID
```

`show` is the replacement for a bare "coefficient" command.

It shows the active coefficient together with residual, uncertainty, cost-model version, plan tier, fit date, and experiment.

Additional modes:

```text
aub calibrate passive
aub calibrate compare CANDIDATE ACTIVE
aub calibrate activate ID
```

Passive results are candidates until explicitly activated.

## `aub rate-card`

```text
aub rate-card import rates.toml
aub rate-card show
aub rate-card history
```

Rate books are imported into immutable versioned records. Valuation resolves those
records, never a file at calculation time.

## `aub can-run`

```text
aub can-run --task-kind TYPE --account work-a --model MODEL
```

Joins task history, cost model, calibration, and fresh quota.

## `aub task`

```text
aub task ingest
aub task report TASK-ID
aub task overhead --since ...
```

Owns task-claim ingestion and segmentation but not issue management.

## `aub export`

Versioned JSONL export for external joins:

```text
aub export --key session-id
aub export --key run-id
```

The friction ledger is the primary intended consumer.

## `aub backup`

Creates a consistent SQLite backup using the SQLite backup API and optionally a
checksum manifest:

```text
aub backup /path/to/archive/
aub backup verify /path/to/archive/
```

Because quota history cannot be reconstructed, backup is an operational requirement,
not an afterthought.

## `aub doctor`

Checks structural health rather than usage.

Useful checks include:

* configuration validity;
* SQLite health;
* schema constraint and `STRICT`-table integrity;
* schema version;
* pending evidence recovery;
* recent sampling cadence;
* unresolved authentication;
* transcript roots;
* parser failures;
* unmapped accounts;
* missing active calibrations;
* stale rate cards where valuation is requested;
* projection/DB generation mismatch;
* backup age;
* meter anomalies;
* excessive unexplained residual;
* heuristic dedupe counts;
* clock skew;
* local-filesystem and WAL suitability.

## `aub config`

Prints resolved configuration and source provenance without secret values.

---

# 28. Machine-readable output

JSON output is part of the public contract and should retain units.

Avoid structures such as:

```json
{"remaining": 42.7}
```

Prefer semantically explicit objects equivalent to:

```json
{
  "remaining": {
    "value": "42.7",
    "unit": "percent"
  }
}
```

Similarly:

```json
{
  "token_count": {
    "value": "12480",
    "unit": "tokens"
  }
}
```

Derived values should expose provenance IDs where useful.

A meter account object should always contain explicit freshness when it contains a reading.

No consumer should need to infer stale state from timestamps itself.

JSON is a versioned interface:

```json
{
  "schema": 1,
  "command": "now",
  "generated_at": "...",
  "knowledge_at": "...",
  "ledger_generation": "...",
  "accounts": []
}
```

`generated_at` says when the report was rendered and `knowledge_at` says which
witness set it was rendered against, which are different facts once a corrected rate
card or calibration lands (§12.14).

For meter state, exactly one freshness variant is always present.

Failure classes remain machine-readable; scripts never need to parse prose such as
"network problem." JSON additionally carries stable symbolic problem codes such as
`AUTH_REQUIRED`, `REMOTE_TIMEOUT`, `COLLECTOR_INTERRUPTED` and `INGEST_PARTIAL`, so
automation reads a name rather than squeezing every semantic distinction through the
process exit code.

---

# 29. Partial data and data quality

The system needs two concepts separate from freshness:

**coverage** and **evidence quality**.

Freshness describes a live remote reading.

Coverage describes whether a requested aggregate includes all evidence that should
contribute. Evidence quality describes how the values it does include were obtained.

For example:

* 20 transcript files parsed;
* 1 transcript failed;
* known usage totals 1.2M tokens.

Printing:

> Total: 1.2M tokens

would be misleading.

Printing:

> Known subtotal: 1.2M tokens; report incomplete because one transcript failed

is justified.

The same rule applies to money valuation and task attribution.

Both dimensions belong to report metadata, and neither is encoded by pretending
missing records are zero.

Human reports use explicit terms:

* `complete`;
* `partial`;
* `estimated`;
* `known subtotal`;
* `floor`.

They never use a bare `total` where known missing evidence affects that aggregate.

A report may be partial and estimated at once. Human and JSON output preserve both
facts rather than choosing whichever label is more convenient (§3.7).

---

# 30. Failure semantics

Failures should be normalized so modules do not invent their own presentation.

| Failure | Stored? | User-visible behavior |
| --- | ---: | --- |
| No credentials configured | attempt/config diagnostic | no quota number; explain account and credential source |
| Provider says credentials invalid/expired | yes | `auth_required`; no current value |
| Endpoint unreachable (DNS/connect timeout) | attempt yes | `stale`; identify transport failure; show last good only as historical |
| HTTP rate limit | yes | `stale`; retain reason and retry information if supplied |
| HTTP 5xx | yes | `stale` |
| 200 with malformed payload | yes | `stale`; provider schema error |
| Provider response timestamp already too old | yes | `stale` even though request succeeded |
| DB unavailable before sample | no network request | fail sample; do not fetch and discard evidence |
| DB commit fails after request | pending spool retained if possible | nonzero operational failure; recover next invocation |
| Projection unavailable/corrupt | no DB fallback in status path | `?`/no-data status; `doctor` can rebuild |
| Transcript missing | report incomplete | never substitute zero |
| Transcript parse error | diagnostic + incomplete report | known subtotal may remain labeled partial |
| Duplicate transcript record | canonical event kept once | duplicate counter increments |
| Unknown token class | event retained | affected credit/valuation total unavailable |
| Missing cost-model term | n/a | no credit total for affected usage |
| Plan mismatch | n/a | calibration not applicable |
| Window reset inside calibration segment | n/a | segment excluded |
| Mixed plan tiers in calibration | n/a | fit rejected |
| Calibration contaminated | n/a | candidate rejected/not activatable |
| Missing rate card | n/a | API valuation unavailable for affected aggregate |
| Task boundary ambiguous | derived | usage goes to explicit overhead bucket |
| Account unknown | derived | usage goes to unknown-account bucket |
| Web consumption has no transcript | meter remains valid | quota movement exists; no invented token attribution |
| Timer never ran | no attempt rows | first-class coverage gap |
| Timer ran, provider failed | attempt rows exist | measurement gap, not attempt gap |
| Meter percent decreases without reset | observation retained | anomaly; exclude interval from calibration |
| Reset timestamp changes unexpectedly | observation retained | semantics anomaly; calibration guard |
| Clock moves backward | evidence retained with clock flag | exclude unsafe interval from calibration |
| Collector died after a durable attempt start | started attempt yes; no terminal result | stale once the command horizon passes; reported as collector interruption, never as an endpoint timeout |
| Projection lags DB | status may show older timestamp, never newer | `doctor` discrepancy; next sample/repair rebuilds |
| Passive fit contaminated | candidate retained | cannot activate |
| Calibration review overdue | historical result remains visible | `can-run` refuses current quantitative verdict |

---

# 31. Zero is data

A useful invariant throughout the system is:

> Zero is only produced by evidence or valid arithmetic.

A provider reporting exactly 0% used is a valid zero.

A transcript containing a valid usage event with zero cache writes is a valid zero.

A missing transcript is not zero.

An HTTP failure is not zero.

A missing price is not zero dollars.

A stale meter is not zero remaining.

Tests should explicitly encode this distinction.

---

# 32. Importing useful existing state

The replacement does not need to throw away every existing artifact.

## Existing persisted meter series

Import it.

Its fields already contain:

* timestamp;
* session;
* account;
* plan tier;
* two percentages;
* reset times.

These should produce both:

* legacy meter observations;
* session/account markers.

However, their known staleness problem must survive migration.

If the persisted timestamp represents hook time rather than actual provider measurement time, do not relabel the data as precise fresh samples.

Mark provenance as legacy and retain the known uncertainty.

Import the account and session evidence as timeline markers, not as a permanently
fixed `session.account_id`.

## Existing regression fit

The 564,577 result may be imported as historical calibration evidence, but it should **not become the active trustworthy calibration** if its cost model omitted cache-write billing.

It can be represented as:

```text
legacy / incomplete-cost-model / known-floor
```

That lets `aub` explain continuity without perpetuating the defect.

It is not activatable for `can-run` if its cost-model completeness is insufficient.

## Existing hardcoded copy

Do not import the copy as an independent fact.

It is not evidence. It is a duplicate.

## Existing transcript tools

Reuse their parser behavior and real transcript fixtures as test material.

Do not retain separate dedup implementations.

Use the legacy reporters as differential-test oracles during migration, not as
authoritative sources after retirement.

## Existing character-count estimator

It can survive as a named reconstruction algorithm with explicit evidence status.

It should not masquerade as provider-reported token usage.

## Existing API price table

Import it as an initial dated rate card.

The comments identifying dates become structured metadata.

Where original publication provenance is missing, flag that fact instead of pretending the data is fully sourced.

---

# 33. Build sequence

## Phase -1: Preserve quota before writing Rust

The project will take longer to build than one sampling interval.

Before implementation begins, put the **existing trustworthy-enough live meter** on
an external timer and archive its outputs with timestamps.

This seed format may be ugly and temporary.

Its sole purpose is to prevent the design period itself from creating permanent
meter-history holes.

It is later imported with explicit legacy provenance.

This is the only phase whose value cannot be recovered later.

The implementation order should follow data irreversibility rather than feature attractiveness.

That principle puts four things ahead of the real provider rollout that a
feature-ordered plan would leave for later hardening: durable attempt starts,
recoverable provider response evidence, reconstructible coverage denominators, and a
backup that has been restored and verified at least once. Each of them is a property
of the series itself. Adding any of them after real observations have accumulated
leaves an early stretch of history that permanently lacks it, which is the one class
of defect this project cannot repair.

## Phase 0: Freeze the domain vocabulary

Implement and test:

* IDs, namespaced where they originate outside `aub`;
* quantity newtypes;
* the three time types and the measurement-basis rule;
* the coverage and evidence-quality lattice;
* freshness;
* semantic identifiers;
* window semantics;
* provenance;
* intervals;
* clock abstraction;
* error taxonomy.

No providers or transcripts yet.

**Exit criterion:** core modules cannot accept raw percentages/tokens/credits interchangeably.

Do not spend weeks perfecting every eventual type before Phase 1. Implement the
minimum domain surface necessary to begin recording irreplaceable evidence.

## Phase 1: Evidence substrate

Implement:

* configuration;
* state-directory resolution;
* SQLite migrations over `STRICT` tables with their constraints;
* the two-stage attempt lifecycle;
* response-evidence capsules and the interpretation tables derived from them;
* pending-result spool and recovery;
* ledger generation;
* backup, restore and backup verification.

No provider is contacted in this phase. It exists to make the shape of the evidence
correct before any of it is irreplaceable.

**Exit criterion:** an attempt start survives a kill before the response; a restored
backup passes integrity and foreign-key checking; and the projection's generation can
be compared against the database it claims to describe.

## Phase 2: Synthetic sampler, projection and coverage

Implement:

* a synthetic provider adapter;
* freshness computation;
* projection writer and reader;
* `aub status`;
* sampling-policy snapshots;
* `aub coverage`;
* sample lease and debounce.

Test with crash injection at every stage of the write path.

**Exit criterion:**

* a fake meter can be sampled repeatedly, stored, recovered after interruption, and
  read concurrently;
* an interrupted attempt is reported as interruption, and never as an endpoint
  timeout or as a missing attempt;
* coverage denominators reconstruct from the policy that was in force rather than
  from current configuration;
* `status` remains responsive while writers, migrations and tests are exercised;
* projection corruption degrades to `?`, never zero.

## Phase 3: Real named-account sampling

Implement only providers actually in use.

Add:

* credential abstraction and credential context;
* blocking HTTP client;
* sanitized real-response fixtures per adapter;
* scoped concurrent sampling;
* timeout handling;
* `aub sample`;
* `aub now`.

**Exit criterion:**

* the timer has run unattended for a meaningful burn-in period;
* every configured named account is sampled without relying on ambient credentials;
* coverage distinguishes no-attempt from failed-attempt intervals on real traffic;
* a replaced credential clears a sticky auth conclusion instead of inheriting it.

At this point the irreplaceable series starts accumulating.

Do not wait for spend support before deploying this phase.

## Phase 4: Legacy series import

Import the one existing persisted series.

Preserve uncertainty and provenance.

Immediately gain the session/account join.

## Phase 5: Transcript normalization

Implement:

* recursive discovery;
* parsers;
* the occurrence model and separate strong/heuristic identity domains;
* central dedup;
* usage-vector storage;
* incremental indexing;
* `aub ingest` and `aub rebuild`;
* session/project/repository reports.

**Exit criterion:** the new spend totals reproduce or explain differences with the trustworthy portions of the old tools, including subagent transcripts.

Run the differential harness over a substantial historical corpus. Every difference
must be classified, for example:

* newly discovered subagents;
* replay removal;
* parser correction;
* cache-write visibility;
* legacy bug.

## Phase 6: Account attribution

Join usage-event timelines with session/account markers.

Expose unknown account explicitly.

**Exit criterion:** the same session ID visible on both sides finally produces account-level spend.

Also establish an attribution-quality metric and explicit unknown-account bucket.

## Phase 7: Cost-model completeness

Import or implement known token-kind credit semantics.

Do not proceed to authoritative window calibration until cache-write and every other
observed billing-relevant component has an explicit treatment.

**Exit criterion:** a fixture containing an unmodeled token kind makes complete
credit conversion impossible.

## Phase 8: Calibration

Implement:

* controlled experiments;
* settled-boundary detection;
* passive candidate generation;
* contamination and identifiability rejection;
* held-out validation on evidence disjoint from the fit;
* immutable records with append-only activation and supersession;
* calibration comparison and drift reporting.

Do not activate the legacy cache-write-incomplete fit.

**Exit criterion:**

* controlled synthetic experiments recover known values;
* real controlled evidence produces a complete active calibration;
* a passive candidate agrees within documented uncertainty or produces an explicit
  drift finding;
* a candidate that fits its own training evidence but fails held-out evidence cannot
  be activated;
* cache-write is actually modeled.

## Phase 9: Residual and self-audit

Build interval reconciliation between observed meter movement and locally explained
credit movement.

**Exit criterion:** known synthetic hidden traffic produces a positive residual;
known calibration overprediction produces the corresponding signed discrepancy,
with lag and quantization uncertainty represented.

## Phase 10: Task attribution

Implement the issue-tracker adapter and temporal segmentation.

**Exit criterion:** all spend is either assigned to a task or to a named overhead/unknown bucket. Nothing is silently prorated.

## Phase 11: `can-run`

Join:

* task history;
* usage → credits;
* calibration, resolved per constraining window;
* live meter;
* window semantics.

**Exit criterion:** every numeric advisory is an interval with provenance, refuses
stale quota, and names the limiting window by calibrated credit headroom rather than
by lowest remaining percentage.

## Phase 12: API-equivalent valuation

Add immutable dated rate cards, `aub rate-card import`, and monetary reporting.

This comes late deliberately: dollars are useful but are not required to repair the meter/spend correctness problem.

## Phase 13: Hardening and retirement

Add:

* migration testing;
* corrupted-state recovery;
* performance tests;
* import/export;
* diagnostics;
* documentation;
* scheduler examples;
* long-lived fixture corpus;
* periodic restore drills against the verification built in Phase 1;
* projection rebuild tests;
* legacy differential harness;
* old-tool retirement checklist.

A legacy tool is retired only after:

1. `aub` answers its question;
2. parallel runs agree or every discrepancy is explained;
3. operational users have switched;
4. the obsolete binary or hook is removed from ordinary `PATH`.

Leaving a retired tool conveniently runnable recreates the possibility of trusting a
confident obsolete number.

---

# 34. Testing strategy

The test suite should not focus primarily on “given fixture X, CLI prints 42.”

The distinctive obligations are **type safety, provenance, state transitions, incompleteness, and temporal semantics**.

## 34.1 Compile-time unit tests

Use compile-fail tests to demonstrate invalid programs do not compile.

Examples:

```text
TokenCount + Credits
QuotaUsed + Money
Credits passed to formatter expecting tokens
QuotaRemaining passed as QuotaUsed
USD added to another currency without conversion
```

The fact that these fail to compile is itself part of `aub`'s correctness evidence.

Also verify:

```text
quantity.unwrap_or_default()
print quantity with bare Display
construct WindowCalibration outside store/calibration module
construct CostModel without an observed TokenKind term
combine Measured and Estimated evidence into Measured
read a Derivation::Unavailable as though it held a value
```

Adding a new known `TokenKind` should cause relevant exhaustive model construction
sites to fail compilation until consciously updated.

## 34.2 Exhaustive freshness tests

Core matches over the freshness enum should not use wildcard arms.

CI should deny wildcard enum matching where practical.

Adding a fourth state in the future should therefore break every place that has not consciously chosen behavior for it.

Separately exhaustively test every `AttemptOutcome` and `StaleReason`.

The important invariant is:

> expanding transport failure taxonomy does not silently expand freshness taxonomy.

## 34.3 Fake-clock state-machine tests

Never test freshness with the real wall clock.

Inject a clock and test sequences such as:

```text
fresh observation at T
read at T + 1 minute        → fresh
read after freshness expiry → stale
new successful observation  → fresh
```

Also:

```text
fresh
transport failure
→ stale, with historical success still labeled historical
```

and:

```text
auth_required
transport failure
→ auth condition remains unresolved
successful authenticated response
→ fresh
```

Also:

```text
no prior success
503 attempt
→ stale, no numeric meter value, reason HTTP 503
```

and:

```text
fresh observation
auth rejection
→ auth_required, previous numeric value may appear only as historical
```

and:

```text
auth_required under credential context A
credential replaced, context B
transport failure under context B
→ stale, reason CredentialChangedUnverified, not auth_required
→ successful response under context B
→ fresh
```

## 34.4 Freshness property tests

Useful properties include:

* time alone can make fresh data stale;
* time alone can never make stale data fresh;
* historical timestamps are never rewritten to “now”;
* authentication cannot become resolved without evidence of an authenticated success or explicit credential reset semantics;
* stale historical values are never labeled current.

These are stronger than snapshot output tests.

Property-test the projection reader against the same freshness state-machine
function used by `now` so the two surfaces cannot develop independent semantics.

## 34.5 No-silent-fallback tests

For every data source, begin with a successful value, then inject a failure.

Assert that:

* the old numeric value does not appear under a fresh label;
* zero does not replace it;
* the failure reason is present;
* the timestamp of the historical measurement remains visible if it is shown.

## 34.6 SQLite reader/writer concurrency

Integration tests run:

* meter writer;
* transcript writer in bounded batches;
* long analytical SQLite reader;
* rapid projection/status reader.

Assert:

* meter writes eventually land or remain durably spooled;
* transcript writes do not lose meter evidence;
* long readers see consistent snapshots;
* `status` never opens SQLite;
* projection readers see old-complete or new-complete files, never torn JSON;
* auth and network-failure attempts update the projection as well as successful samples.

## 34.7 Crash recovery

Inject failures at stages such as:

* after the attempt-start commit but before the request returns;
* after network parse but before SQLite commit;
* after pending file write;
* after SQLite commit but before pending-file deletion.

Restart.

Assert exactly one observation exists after recovery, and that the first case leaves
a started attempt with no terminal result rather than either a fabricated timeout or
no attempt at all.

Inject a crash after DB commit but before projection replacement. On restart:

* the DB contains the evidence exactly once;
* the old projection remains valid but older;
* `doctor --fix` or the next sample rebuilds it;
* freshness still ages honestly.

## 34.8 Provider adapter contract tests

Every adapter shares a common suite covering:

* valid success;
* zero percentage;
* multiple windows;
* model-specific windows;
* 401;
* provider-defined authentication expiration;
* 403 with ambiguous semantics;
* 429;
* timeout;
* malformed JSON;
* missing expected field;
* unknown additional field;
* stale server timestamp;
* reset change.

Provider-specific logic decides when a response truly means `auth_required`; not every 403 should automatically be classified that way.

Contrary to the idea that provider shapes need not be tested, adapters require
sanitized real-response fixtures. A parser or schema change is exactly the kind of
failure that could otherwise transform remote truth into absence.

Maintain:

* golden response fixtures;
* unknown-field fixtures;
* missing-field fixtures;
* known provider error bodies;
* optional manually captured live acceptance comparisons.

Adapters also carry contract tests over the semantic identifiers, because
applicability decisions hang on them:

* an adapter refactor with unchanged semantics leaves an existing calibration
  applicable;
* a changed meter-semantics ID makes the old calibration inapplicable;
* a changed billing-semantics ID rejects an incompatible cost model or calibration.

And over the two-stage attempt lifecycle:

* a kill between attempt start and response leaves a started attempt with no result;
* that attempt reads as collector interruption past the command horizon, and never as
  a timeout;
* an interpretation bug can be corrected against retained response evidence, and the
  correction creates a new interpretation rather than overwriting the old one.

## 34.9 Named-account isolation

Create two configured logical accounts for one fake provider with distinct credentials.

Run concurrent samples.

Assert:

* requests use the intended credentials;
* observations land under the intended logical account;
* ambient process credentials do not influence either;
* swapping credential files is detectable where provider identity validation exists.

## 34.10 Window-semantic tests

Given:

```text
account remaining = 40%
model remaining   = 15%
```

effective model remaining must be 15%.

Add unrelated model windows and prove they do not constrain the chosen model.

Test exact reset boundaries.

Also test:

* provider adds a new account-wide window;
* provider omits a previously present model window;
* reset timestamp unexpectedly changes;
* percentage falls without a corresponding reset.

The latter two become anomalies and are excluded from calibration evidence.

## 34.11 Transcript parser corpus

Maintain real sanitized fixtures for every supported CLI format.

Fixtures should include:

* simple sessions;
* nested subagent paths;
* truncated files;
* partially written final records;
* file rotation;
* malformed records;
* model changes mid-session;
* cache reads/writes;
* sessions with no native usage field.

## 34.12 Recursive-discovery regression

Have a root fixture where all meaningful usage exists only in nested subdirectories.

The expected total must be nonzero.

This permanently guards the known non-recursive-glob defect.

## 34.13 Deduplication stress

Generate replay datasets containing tens or hundreds of thousands of duplicate records.

Assert:

* canonical count is stable;
* duplicate count is correct;
* ingestion is idempotent;
* adding the same transcript from another path does not double-count if the dedup identity says it is the same source event.

Also test near-duplicates that must remain distinct.

Run a fixture approximating the observed 98,004 replay magnitude.

Test strong-ID and heuristic-ID parsers separately.

Add:

* the same heuristic key with a materially different semantic payload produces a
  collision diagnostic rather than an arbitrary winner;
* two legitimately equal-sized adjacent requests remain distinct;
* a stable native ID always outranks a heuristic identity;
* changing the heuristic algorithm version forces a rebuild instead of silently
  changing the canonical identity of old events.

## 34.14 Cumulative-record tests

For cumulative usage sources:

* dedup replay first;
* derive monotonically correct deltas;
* reject or explicitly handle counter resets.

A replayed cumulative record must not create new consumption.

## 34.15 Reconstructed-token tests

For character-count estimation:

* output is marked reconstructed;
* estimator version is retained;
* exact and reconstructed token events remain distinguishable after aggregation.

If exact and estimated data are mixed, reports should expose that composition.

Estimated data is excluded from calibration and from the default `can-run`
historical reference set.

## 34.16 Rebuild determinism

Delete every rebuildable transcript-derived table.

Re-ingest the same transcript corpus.

Assert canonical normalized events and report quantities are identical.

Run both the explicit `aub ingest` path and the automatic refresh path and prove they
converge on the same canonical event set.

This demonstrates that SQLite is a cache for the spend axis, not its only evidence.

## 34.17 Account-segment tests

Test:

* one account for whole session;
* account switch in middle;
* usage exactly on a marker timestamp;
* no marker;
* duplicate markers;
* out-of-order input markers.

Boundary inclusion rules must be deterministic.

Test the attribution-evidence ranking and prove an inferred marker cannot overwrite
an explicit marker.

## 34.18 Task-attribution tests

Test:

* before first claim;
* claim-to-claim boundaries;
* explicit release;
* after release;
* exact boundary timestamps;
* usage with insufficient time resolution;
* cumulative record crossing a boundary.

Ambiguous amounts must land in overhead, never be silently divided.

Assert the conservation invariant:

```text
tasks + overhead == canonical session usage
```

## 34.19 Cost-model completeness

A usage vector containing cache-write tokens plus a model without cache-write weighting must fail valuation into credits.

Adding the required term makes the same calculation valid.

This is a direct regression test for the defect motivating the project.

Also add a compile-time exhaustiveness test where introducing a new known token kind
breaks all complete cost-model builders.

## 34.20 Single-source calibration test

Create one calibration record with a conspicuous synthetic coefficient.

Assert that:

* `calibrate show`;
* spend-to-meter conversion;
* and `can-run`

all report/use the same calibration ID.

Then supersede it.

Assert all consumers move to the new active record without changing source code or configuration copies.

This is more valuable than testing for the absence of a literal alone.

Add a source-tree tombstone check for the historical copied constant so accidental
resurrection is immediately obvious.

Do **not** adopt a blanket prohibition on all other numeric literals; that produces
noise rather than enforcing the actual invariant.

## 34.21 Synthetic calibration recovery

Generate synthetic data with a known:

* cost model;
* window capacity;
* accounting lag;
* noise;
* quantization.

Run the fitter.

Assert the known coefficient lies inside the reported uncertainty interval and the residual behaves sensibly.

Run separate synthetic suites for:

1. known cost model plus unknown window capacity;
2. deliberately varied joint token-kind experiment;
3. highly collinear token kinds that must be rejected;
4. provider quantization;
5. delayed accounting;
6. contaminated hidden traffic.

Include a case where a model fits its training evidence well and fails a held-out
experiment. Activation must be rejected.

## 34.22 Calibration rejection tests

Explicitly verify rejection of:

* mixed plan tiers;
* reset-crossing segments;
* missing cache-write term;
* insufficient variation to identify coefficients;
* too few usable points;
* contaminated idle periods;
* non-positive slope;
* impossible percentages;
* validation evidence overlapping fitting evidence where policy requires
  independence;
* a held-out residual exceeding the activation policy.

## 34.23 Advisory metamorphic tests

Rather than testing one literal result, test properties.

Holding everything else fixed:

* more current remaining quota cannot worsen the reported margin;
* increasing historical task consumption cannot improve the margin;
* widening calibration uncertainty cannot narrow the advice interval;
* adding a tighter applicable provider window cannot increase reported headroom;
* a window with more remaining percentage but less calibrated headroom must be
  capable of becoming the limiting constraint;
* removing a window's calibration must move the verdict to `UNKNOWN` rather than
  letting the remaining windows answer alone;
* removing the fresh meter must make the current advice unavailable.

These tests exercise the meaning of the system.

Also assert:

* making calibration health `Suspect` cannot leave an `AMPLE` quantitative verdict;
* adding estimated historical tasks does not improve the exact-evidence verdict;
* adding an unknown token kind cannot shrink the consumption interval.

## 34.24 Money tests

Use exact decimal fixtures.

Test:

* effective-date boundaries;
* different rates for token classes;
* model price changes;
* missing prices;
* unsupported currencies;
* incomplete valuation.

A missing cache-write price must not imply zero cache-write cost.

## 34.25 JSON contract tests

Machine-readable output should be schema tested.

Assert:

* every quantity is accompanied by unit semantics;
* every meter reading has freshness;
* stale and auth-required are distinguishable;
* coverage and evidence quality are both present and independently readable;
* intervals retain both endpoints;
* provenance identifiers survive serialization;
* `knowledge_at` and `ledger_generation` are present on every report;
* symbolic problem codes are stable across releases.

## 34.26 Coverage tests

Generate a synthetic expected schedule and test:

* timer never ran;
* timer ran but every endpoint failed;
* intermittent failures;
* a no-attempt interval corresponding to simulated machine sleep;
* an attempt started and never completed;
* gap spanning reset;
* normal reset-edge sample;
* a cadence change mid-interval, where the denominator must follow the policy that
  was in force;
* authentication backoff if enabled.

Attempt coverage and measurement coverage must differ appropriately.

The sleep case verifies the no-attempt gap itself. It must not assert a root cause,
because the system does not have evidence of one.

## 34.27 Projection tests

Repeatedly replace the projection while concurrent readers open and read it.

Assert:

* every read is valid old or new data;
* no torn file is observed;
* stored timestamps drive freshness;
* latest failure outcomes appear;
* projection schema mismatch produces `?`;
* rebuilding from the DB is deterministic.

## 34.28 Provenance tests

For seeded end-to-end commands, every physical quantity in normal output must have a
corresponding provenance node in `--explain`.

Rather than merely matching digit strings, test typed report-model field IDs against
provenance graph IDs so timestamps and incidental numbers do not create false
positives.

For manifest-backed provenance, verify that `--explain=full` expands to exactly the
typed evidence IDs whose canonical hash produced the manifest.

## 34.29 Identity and privacy tests

* build in a foreign `$HOME` and username;
* scan release binaries for configured forbidden strings;
* assert no credential bytes enter database fixtures;
* assert the projection contains logical identity only;
* verify fixture anonymization;
* verify raw provider body capture is disabled by default.

## 34.30 Differential tests against legacy tools

Before retirement, execute old and new spend tools over at least a representative
multi-week corpus.

Every discrepancy is categorized.

Likewise, manually compare provider readings against the provider's own trusted UI
or canonical usage surface often enough to validate adapter semantics.

The known 41%-versus-70% closed-source discrepancy is not a tolerance precedent;
`aub` should agree with the provider's authoritative surface within its documented
granularity or explicitly report the unresolved mismatch.

## 34.31 Status no-network structural test

Make `status` depend only on:

* config-location resolution;
* projection parser;
* clock;
* renderer.

The dependency graph should make HTTP transport unreachable from this workflow.

Where practical, CI additionally checks process syscalls on a supported platform to
ensure `aub status` does not open sockets.

---

# 35. Meter reconciliation and self-audit

Once both axes and a complete calibration exist, compute an interval-level
reconciliation:

```text
observed meter delta
− locally explained calibrated delta
= unexplained residual
```

Call it **unexplained residual**, not "unattributed consumption," because it can
contain measurement and model error.

Only compute it on eligible intervals:

* same account and window;
* no reset;
* acceptable meter coverage;
* applicable current calibration;
* sufficient settlement and lag handling;
* exact local usage where required.

The residual itself has uncertainty propagated from:

* meter quantization;
* calibration uncertainty;
* timing alignment.

Quantization uncertainty comes from the provider measurement semantics persisted with
the observation (§12.5), not from one globally guessed tolerance.

Interpretation is diagnostic, not definitive:

* persistently positive: possible web, headless-unlogged, cross-machine or missed
  transcript consumption;
* persistently negative: possible calibration overprediction or provider semantics
  change;
* step change: possible plan or provider accounting transition;
* alternating short-interval residuals that net to zero: likely accounting lag.

`doctor` reports rolling residual health only when the computation is justified.

This becomes a continuous audit of whether `aub` still explains the physical system
it claims to model.

---

# 36. Operational diagnostics

`aub doctor` should be useful enough that a missing series is discovered before somebody needs yesterday's data.

A healthy system should be able to answer:

```text
last timer invocation
last attempt per account
last successful measurement per account
current effective freshness
unresolved authentication state
largest recent sampling gap
started attempts with no terminal result
pending evidence spool count
transcript parser failures
unknown-account spend
unattributed-task spend
active calibration and age
```

This is not monitoring infrastructure. It is a synchronous health report over local evidence.

Add:

```text
attempt coverage
measurement coverage
started attempts with no terminal result
projection generation versus DB
last verified backup
schema constraint and STRICT-table integrity
calibration candidate drift
meter anomalies
unexplained residual interval/fraction
heuristic dedupe usage and collisions
quarantined parser records
clock anomalies
```

"Last backup" means the last backup that passed verification (§38). An unverified
archive is not a backup of anything in particular.

`doctor` should distinguish checks from repairs.

`doctor --fix` may safely:

* rebuild the projection;
* drain recoverable pending meter evidence;
* clear expired operational leases;
* recreate disposable indexes and materializations explicitly designated safe.

It must not silently:

* activate calibrations;
* change price books;
* delete quota history;
* reattribute ambiguous sessions;
* repair evidence by guessing.

Integrity checks are diagnostic. Optimization, checkpointing and vacuuming are
maintenance rather than repair, and if they are ever exposed they belong to an
explicit maintenance command. `--fix` holds only deterministic operations that
restore derived or operational state, which is what makes it safe to run without
reading its source first.

---

# 37. Logging, security, and privacy

Normal machine-readable output belongs on stdout.

Diagnostics belong on stderr.

No telemetry is necessary.

Secrets must never appear in:

* SQLite;
* pending evidence spool;
* normal logs;
* JSON reports.

Paths and provider payloads should be minimized.

Human errors should prefer logical configuration names:

```text
account "work-a": credential file unreadable
```

rather than dumping a complete home-directory path unless verbose diagnostics are explicitly requested.

The SQLite database and local state should be created with user-only filesystem permissions where the platform permits it.

Recommended:

```text
state directory: 0700
database/projection/spool: 0600
```

Provider errors are sanitized before persistence.

No log line prints:

* authorization headers;
* access or refresh tokens;
* cookie values;
* raw credential-file contents.

Transcript discovery does not follow symlinks by default.

Exports make the inclusion of logical account and project identifiers explicit,
because an archival or export file may be shared more broadly than the local state
database.

---

# 38. Backup and recovery

The quota series is irreplaceable; therefore backup belongs in the core operations
design.

`aub backup DEST` uses SQLite's consistent backup facility rather than copying a live
database file blindly.

The pending meter spool is part of irreplaceable state and belongs to the backup cut.
It holds the newest evidence precisely when SQLite is having trouble, which is
precisely when a backup is most likely to be taken.

Backup procedure:

1. establish a short state-snapshot barrier against spool deletion or rotation;
2. drain pending evidence into SQLite where possible;
3. create the SQLite snapshot;
4. include any evidence still pending that belongs to the same backup cut;
5. write checksums and the manifest;
6. open the destination database read-only;
7. run SQLite integrity checking;
8. run foreign-key checking, which integrity checking does not cover;
9. validate every included spool record and its checksum;
10. mark the backup verified only once all checks pass.

A checksum proves only that the archive has not changed since it was written. It says
nothing about whether the contents were logically healthy when written, so an
unverified archive is not yet a backup.

The resulting archive includes:

* database;
* pending evidence belonging to the backup cut, if any;
* schema version;
* checksum;
* `aub` version;
* creation timestamp;
* source `ledger_generation`;
* verification result.

An external daily or weekly scheduler may run backup independently of sampling.

`doctor` reports the age of the last verified backup if backup policy is configured.

Recovery procedure:

1. stop mutating invocations;
2. preserve the damaged state directory;
3. verify backup checksum;
4. restore to a new directory;
5. run integrity and migration checks;
6. replay both restored and surviving local pending evidence idempotently;
7. rebuild the projection;
8. rebuild transcript-derived tables if necessary.

Do not overwrite damaged evidence before retaining a forensic copy.

---

# 39. Performance expectations

Performance is secondary to correctness, but the architecture should avoid obvious traps.

## Status

Target behavior:

* one projection-file read;
* local freshness calculation;
* formatting;
* no network.

History size and SQLite lock state are irrelevant to this path.

## Sampling

Network latency dominates.

Requests run concurrently in bounded scoped threads.

Persistence is one short transaction per batch or a few short independent transactions.

## Spend

Incremental transcript indexing means repeat reports generally examine only changed files.

The expensive full reconstruction path remains available.

## Analysis

Calibration and historical task analysis are rare and can afford more database work.

No effort should be spent designing a server-side cache for them.

---

# 40. Exit codes and scripting contract

Use stable exit classes so automation does not parse prose.

Suggested:

| Code | Meaning |
|---:|---|
| 0 | Command completed for its contract |
| 1 | Unexpected internal failure |
| 2 | Configuration/argument/environment invalid |
| 3 | Requested live source requires authentication |
| 4 | Requested live or remote source unavailable |
| 5 | Store or durable-state failure |
| 6 | Insufficient evidence for requested quantitative result |
| 7 | Explicit threshold/advisory result not met |
| 8 | Local ingest or report incomplete because required source material could not be normalized |

Code 4 is strictly about a live or remote source; code 8 is strictly about local
material. The earlier wording let one report qualify for both, which defeats the
purpose of a stable class.

Special cases:

* `status` returns 0 after successful invocation even when displaying stale, auth or
  no-data conditions;
* scheduled `sample --due` returns 0 when all attempt outcomes were durably
  recorded, including remote auth and network failures;
* it returns non-zero when evidence could not be durably preserved;
* `sample --require-success` records the same evidence and then reports remote
  failures through their ordinary live-source exit classes, for callers that are not
  timers.

This treats a remote failure as data but a persistence failure as an operational
failure.

---

# 41. What not to optimize prematurely

Do not introduce:

* an async runtime solely to parallelize three HTTP calls;
* a daemon just to keep SQLite open;
* a separate status JSON cache;
* a message queue;
* an ORM requiring complex schema indirection;
* a generic plugin framework for dozens of unused providers;
* a distributed database;
* background threads persisting after command exit.

Every one of those increases the number of places where semantic state can diverge.

Also do not prematurely introduce:

* a cryptographic hash chain over the ledger;
* automatic calibration activation;
* complex server-lag deconvolution when settled intervals suffice;
* universal vendor abstraction before a second real provider demands it;
* machine-learning task-cost prediction;
* database pruning of meter evidence;
* policy logic that automatically selects accounts.

The hash chain deserves a word because the phrase "chain of custody" invites it. The
requirement here is semantic integrity, not tamper evidence against an adversary with
write access to the state directory. Immutable evidence rows, content hashes,
database constraints, verified backups and provenance manifests already satisfy it.
Add a chain when an adversarial threat model actually exists.

---

# 42. Important invariants to document in the source tree

The eventual repository should have a short architecture/invariants document repeating these rules so they are not merely historical design intent.

The critical invariants are:

1. Provider quota attempts and observations are irreplaceable evidence.
2. Transcript-derived spend is reconstructible.
3. No semantically meaningful raw numeric primitive crosses a domain boundary.
4. Freshness is an exhaustive three-way state.
5. Sampling-attempt outcome is not the same dimension as freshness.
6. A failed source never produces zero.
7. A historical value is never presented without its actual observation time.
8. Provider credentials are resolved from named-account configuration, never
   accidental ambient state.
9. All replay deduplication passes through one semantic event-identity framework and
   database uniqueness constraint.
10. Unknown token components block complete conversions.
11. Cost-model coefficients and window calibrations are distinct versioned witnesses.
12. Calibrated values have immutable IDs and no consumer-side literals.
13. Calibration cannot cross incompatible plan tiers, reset boundaries, or provider
    semantics.
14. Passive calibration is evidence and candidate generation, not automatic truth.
15. `status` never opens SQLite, performs network I/O, or writes.
16. The projection is disposable and contains no stored freshness boolean.
17. Task ambiguity becomes explicit overhead.
18. Dollar valuation and subscription credits are distinct dimensions.
19. Meter residual is diagnostic and is not automatically called hidden token spend.
20. Estimated transcript usage is never silently promoted to measured usage.
21. The friction ledger remains external and joins only through stable IDs.
22. Irreplaceable meter history is backed up and never automatically pruned.
23. An outbound meter request is never begun before its attempt identity is durable.
24. An attempt without a terminal result is evidence of collector interruption, not
    evidence that no attempt occurred.
25. Provider response evidence and its normalized interpretation are separate
    records; a corrected adapter reinterprets retained evidence and never overwrites
    the earlier interpretation.
26. Workload feasibility is evaluated against every constraining window in calibrated
    credits, never against the lowest remaining percentage alone.
27. Coverage denominators come from the sampling policy that was in force over the
    interval, never from current configuration.

These are more important than most implementation choices.

---

# 43. Expected behavior for the primary workflows

## Workflow 0: “Are we still recording the thing we can never reconstruct?”

1. Build expected sampling opportunities from the policy snapshots in force over the
   interval.
2. Join actual `meter_attempt` rows.
3. Separately count terminal results and successful observations.
4. Identify no-attempt, interrupted-collector and no-observation gaps.
5. Identify reset-spanning gaps.
6. Return coverage health and threshold exit status.

This workflow is as important as any usage report because it detects permanent data
loss while something can still be fixed for the future.

## Workflow 1: “How much did today cost, and where did it go?”

End to end:

1. Load configuration.
2. Discover changed transcript files recursively.
3. Parse through source adapters.
4. Normalize usage events.
5. Apply the one dedup pipeline.
6. Persist/update rebuildable event indexes.
7. Build session timelines.
8. Resolve project and repository.
9. Join session/account markers.
10. Join task segments if requested.
11. Apply a cost model only if credits are requested.
12. Apply a rate card only if API-equivalent valuation is requested.
13. Calculate coverage and evidence quality.
14. Attach provenance IDs.
15. Render groups.

The report can answer by:

* day;
* session;
* project;
* repository;
* account;
* task.

Any unattributed usage remains in explicit buckets and stays in the grand known-usage accounting.

Example semantics:

```text
tokens                    measured, exact
subscription credits      complete via cost-model #8
% of 5h window            via window-calibration #17
API list-price equivalent opt-in, rate-card 2026-08-01
```

If one dimension cannot be justified, omit that dimension rather than dropping the
whole report.

## Workflow 2: “Which account is spending right now, and how much of its window is left?”

1. Read recent account/session markers.
2. Determine which sessions have explicit evidence of an account.
3. Read persisted meter evidence.
4. Compute effective freshness now.
5. Evaluate all applicable provider windows.
6. Identify the window with the lowest remaining fraction, which is a display
   concept and not a feasibility verdict.
7. Show account activity evidence and remaining quota.

The trigger is `aub now`, which records every attempted provider read.

`aub` should only say an account is actively paying for a known session when explicit marker evidence justifies it.

It should not infer “current payer” merely from a delayed meter change.

A 503 does not erase the last known meter value, but it changes the current reading
to stale with reason.

## Workflow 3: “Can I run this unit of work now?”

1. Identify task kind.
2. Retrieve comparable completed task histories.
3. Validate attribution quality.
4. Convert each historical usage vector through one cost-model version.
5. Build empirical credit distribution.
6. Select applicable plan/window calibration.
7. Perform a live persisted meter sample.
8. Require fresh state.
9. Enumerate every provider window constraining the selected model.
10. Resolve an applicable current calibration for each of them.
11. Convert each window's remaining percentage into a credit-headroom interval,
    propagating calibration uncertainty.
12. Compare the historical task-credit interval against every headroom interval.
13. Select the limiting calibrated constraint, which need not be the window with the
    lowest remaining percentage.
14. Return the relevant margins, evidence count, limiting window, and caveats.

No stale meter means no current answer.

No suspect or review-expired calibration means no current quantitative verdict.

No attempt is made to predict whether the task crosses a reset based on duration.

## Workflow 4: Status bar / shell status line

1. Open projection.
2. Read latest attempt plus last-good observation facts.
3. Evaluate freshness against current time.
4. Calculate applicable window minimum.
5. Render.
6. Exit.

Nothing else.

This path remains useful even if transcript parsing, rate cards, or issue-tracker integration are broken.

## Workflow 5: Per-task attribution

1. Parse transcript events with timestamps.
2. Ingest claim/release events from the issue tracker.
3. Construct task intervals.
4. Assign each precisely attributable usage event to its interval.
5. Put temporally ambiguous consumption into explicit overhead.
6. Group usage/credits by task.
7. Retain `RunId` where available for later external friction-ledger joins.

No proportional smearing is necessary.

## Workflow 6: Calibration

1. Verify applicable account, plan and window.
2. Verify cost-model completeness.
3. Select controlled or passive-candidate mode.
4. Qualify evidence.
5. Handle settlement and lag.
6. Fit.
7. Test identifiability and residuals.
8. Persist an immutable candidate.
9. Compare with the active result.
10. Activate only explicitly.

Nothing copies a numeric coefficient into source or config consumer code.

## Workflow 7: Recovery

1. Drain the pending meter spool first.
2. Verify DB integrity.
3. Rebuild the projection.
4. Rebuild transcript materializations if requested.
5. Report any irreplaceable evidence that could not be recovered.

---

# 44. Definition of a trustworthy first release

The first release does not need every feature in this document.

A trustworthy initial release should be considered successful when all of the following are true:

* every actively used named account can be sampled without depending on ambient current credentials;
* an external timer can invoke `aub sample --due`;
* successful and failed attempts are persisted;
* sampling gaps can be detected;
* stale and auth-required states survive storage and reporting;
* `aub status` reads only the projection and never waits on remote I/O;
* one provider's failure does not suppress another account's sample;
* the existing meter series has been imported without pretending its old samples are fresher than they are;
* the status projection and its SQLite source stay semantically consistent under crashes;
* attempt coverage can identify a dead timer;
* a successful network result survives a SQLite writer conflict via the pending spool;
* a daily backup can be verified and restored;
* provider adapter fixtures cover real response and error shapes;
* an attempt is durable before its request leaves the process, so a collector killed
  mid-request is visible as an interruption rather than as an interval nobody sampled;
* the sanitized provider response evidence behind each observation is retained, so an
  adapter misreading discovered later can be corrected instead of mourned;
* coverage denominators are reconstructed from the sampling policy that was in force;
* no calibrated spend conversion is yet allowed to depend on the old cache-write-incomplete constant.

That release would already fix the most irrecoverable problem: future quota history would stop disappearing.

The next trustworthy milestone adds transcripts, recursive subagent coverage, central deduplication, and the previously unused session-ID join.

Only after those are stable should `aub` publish a new calibration and make `can-run` authoritative enough to use.

---

# 45. Risks and explicit cut lines

### Weak account attribution

If headless sessions cannot emit markers, do not paper over the gap with aggressive
temporal inference.

Track attribution coverage and accept an `unknown-account` bucket.

### Unidentifiable token-kind coefficients

If ordinary traffic does not vary token kinds independently, passive multivariate
fitting may be mathematically incapable of separating them.

The correct result is:

```text
cannot identify cache-read coefficient independently from input coefficient
```

not an unstable coefficient with six decimal places.

### Provider semantic drift

Providers may change:

* window definitions;
* billing weights;
* meter update cadence;
* endpoint payloads.

Tripwires are:

* window anomaly detection;
* calibration candidate drift;
* unexplained residual;
* provider contract fixtures;
* plan and window semantic version.

### Projection duplication

The projection adds another file and therefore another possible failure mode.

It is accepted because:

* it contains no calibration or reference constants;
* it is one-way derived;
* status latency is operationally important;
* it recomputes freshness;
* it can be deleted and reconstructed.

If practical benchmarks show direct SQLite reads are already comfortably below the
required status latency on all supported environments, the projection may be
reconsidered. Correctness does not depend on its existence.

### Scope creep into account management

`can-run` will naturally invite automatic account switching.

Do not put that in this binary.

Machine-readable output and exit codes are sufficient for a separate policy tool.

### What to cut first if implementation must shrink

In order:

1. API dollar valuation;
2. rich task-report presentation;
3. passive calibration activation support, retaining passive comparison;
4. provider breadth;
5. pace projection.

Do **not** cut:

* sampling;
* attempt persistence;
* coverage;
* named accounts;
* dedup;
* coverage and evidence quality;
* calibration provenance;
* projection honesty;
* backup.

---

# 46. Final architectural decisions

The open questions resolve as follows.

| Question | Decision |
| --- | --- |
| Persistence | **SQLite, one bundled database, WAL, local filesystem.** Meter attempts and observations are durable evidence; transcript tables are rebuildable materializations. |
| Status-bar concurrency | **Atomic disposable projection file.** No SQLite, network, migration or write on the status path. |
| Freshness | **Exactly `fresh` / `stale` / `auth_required`.** Transport and provider failures are separate persisted attempt outcomes and become typed stale reasons. An auth conclusion is scoped to the credential context that produced it. |
| Attempt durability | **Two-stage append-only lifecycle.** The start is committed before network I/O; the terminal result is a separate fact; a start with no result is collector interruption. |
| Evidence versus interpretation | **Separate records.** Sanitized provider response evidence is irreplaceable; the normalized meter values read out of it are a versioned interpretation that a corrected adapter may supersede. |
| Data quality | **Coverage and evidence quality are orthogonal.** A report can be partial and estimated at once, and a derivation that cannot be performed is `Unavailable` rather than partial. |
| Coverage | **First-class `coverage` command** with separate attempt and successful-measurement coverage. |
| Dollar axis | **Include it as a separate API-list-price-equivalent valuation layer.** Never conflate it with subscription credits; rate cards are immutable dated data. |
| Headless runs | **External periodic `aub sample --due` plus launcher/hook account markers.** No daemon required. |
| Web runs | Meter movement is preserved; token/task attribution remains unavailable; the difference appears only as uncertain unexplained residual. |
| Calibration | **Separate cost model and window calibration. Controlled experiment for authority and cold start; guarded passive candidate and validation fits; settled-boundary lag handling.** |
| Cache-write omission | A cost model missing a present token class is incomplete and cannot produce an active calibration. |
| Live meter | `aub now` performs and persists a sample, then renders it. No normal unrecorded fetch path. |
| Coefficient command | Replace a singleton coefficient concept with `aub calibrate show/history/fit/activate`. |
| Spend reports | `aub spend`, grouping over the one normalized transcript ledger. |
| Joining question | `aub can-run`, using a fresh meter and returning an interval rather than a scalar, evaluated against every constraining window in calibrated credits. |
| Sampling command | `aub sample`, designed for hooks and timers as well as manual use. |
| Replay dedupe | Semantic usage-event identity plus SQLite uniqueness; file hashes are not sufficient. |
| Provider parser changes | Sanitized real-response contract fixtures plus explicit malformed and schema failure handling. |
| Projection | Derived only; latest attempt plus last-good observation plus timestamps; never persisted freshness. |
| Durability after network success | Sanitized pending spool before SQLite commit; idempotent replay. |
| Backup | First-class consistent backup of irreplaceable evidence. |
| Friction ledger | Remains separate; only `RunId` is shared. |

---

# 47. Suggested configuration sketch

```toml
schema = 1

[state]
dir = "~/.local/state/aub"

[sampling]
scheduler_tick = "1m"
default_interval = "5m"
reset_edge_lead = "60s"
request_timeout = "5s"
command_budget = "8s"

[freshness]
meter = "12m"

[coverage]
attempt_floor = 0.98
measurement_floor = 0.95

[[accounts]]
name = "work-primary"
provider = "provider-a"
credential = { kind = "profile", ref = "work-primary" }

[[accounts]]
name = "research"
provider = "provider-a"
credential = { kind = "file", path = "~/.config/provider-a/research.json" }

[[transcripts]]
name = "cli-a"
root = "~/.local/share/cli-a"
pattern = "**/*.jsonl"

[[transcripts]]
name = "cli-b"
root = "~/.cache/cli-b"
pattern = "**/*.jsonl"
usage_evidence = "estimated:char-count/v1"

[tracker]
kind = "local"
path = "~/work/.tracker"

[valuation]
default_rate_book = "vendor-api-prices"

[backup]
review_after = "48h"
```

Values shown are configuration examples, not compiled identity or domain constants.

---

# 48. Example status semantics

```text
# Fresh last attempt
aub work-primary 38% left · 5h

# No successful recent reading because provider timed out
aub work-primary ~38% · stale 14m · timeout

# Provider requires authentication
aub work-primary auth!

# Collector started an attempt and never finished it
aub work-primary ~38% · stale 9m · collector interrupted

# Never successfully observed
aub work-primary ? · stale · no successful sample

# Projection itself missing
aub ?
```

Freshness is never communicated by color alone.

The stale line may show a historical numeric value, but its age and reason are
inseparable from it.

---

# 49. Example `coverage` semantics

```text
coverage - last 24h

account          attempts     measurements    longest blind gap    reset gaps
work-primary     99.3%        98.6%           9m                   0
research         99.3%        71.5%           2h 11m               1

research:
  - scheduler ran normally
  - 41 attempts required authentication
  - one 5h reset occurred without a successful observation in the surrounding interval
```

This immediately distinguishes "timer died" from "provider credentials died."

---

# 50. Example calibration semantics

```text
active window calibration #17

provider/window: provider-a / account:5h
plan:            tier-2
cost model:      #8
method:          controlled-settled-boundaries/v1
evidence:        experiment #31
window capacity: [...]
uncertainty:     [...]
residual:        [...]
input hash:      [...]
fitter:          aub 0.x / revision ...
health:          current

cost model #8:
  - input          modeled
  - output         modeled
  - cache-read     modeled
  - cache-write    modeled
  - unknown kinds  none
```

A passive candidate might render:

```text
candidate #19 differs from active #17 by 3.8%
candidate evidence is sufficient for comparison but is not active
```

No copy/paste operation exists.

---

# 51. Example `can-run` semantics

```text
can-run: refactor-module
account: work-primary
model: model-x

constraining windows, fresh, observed 41s ago:
  - account:5h      38.0% remaining  calibration #17  headroom 3,040–3,420 credits
  - model-x:weekly  52.0% remaining  calibration #22  headroom 1,040–1,180 credits
  - lowest remaining percentage: account:5h
  - limiting calibrated window:  model-x:weekly
  - account:5h resets 16:20

historical exact task evidence:
  - n = 23
  - median = 640 credits
  - p25–p75 = 550–940 credits
  - p90 = 1,520 credits

calibration:
  - #17 and #22, both current
  - complete token-kind coverage

comparison:
  - account:5h      central range leaves 2,100–2,870 credits
  - model-x:weekly  central range leaves 100–630 credits
  - p90 reference exceeds model-x:weekly headroom by 340–480 credits

assessment: MARGINAL
limiting window: model-x:weekly
```

This is an empirical comparison to 23 historical tasks, not a prediction of the next
task's exact consumption.

Note which window won. `account:5h` shows the lower percentage and holds three times
the work. Deciding feasibility from the smaller percentage would have answered
`AMPLE` here, and been wrong.

If current meter collection failed:

```text
assessment: UNKNOWN
reason: provider usage source timed out; last successful observation is stale
```

There is no fallback to the historical meter.

---

# 52. Closing design principle

The unifying idea for `aub` should not be “put the seven old tools into one executable.”

That could reproduce all seven disagreements inside one repository.

The more useful objective is:

> **Every physical quantity has one semantic definition; every concrete claim about
> it carries an explicit provenance chain and an explicit interpretation or witness.**

The weaker first half is deliberate. This system retains several observations of the
same window, competing calibration candidates, superseded interpretations and
estimates alongside measurements. Claiming one representation per physical claim
would describe a system that throws that material away, which is the opposite of what
is being built.

A transcript contributes typed usage evidence.

A cost model converts that usage vector into typed credits.

A provider sampling attempt records whether the remote source could be observed.

A successful provider observation contributes typed quota evidence.

The attempt history plus observation age produces exactly one of
`fresh | stale | auth_required`.

A calibration relates credits to a particular quota window and plan tier.

A rate card independently values the same usage in API-equivalent money.

Account and task timelines attribute evidence only where a join can actually justify it.

The CLI combines those layers, but none is allowed to silently substitute for another.

Coverage verifies that the irreplaceable evidence is actually being collected.

The unexplained meter residual tests whether the two axes still reconcile.

Backups protect the evidence that cannot be reconstructed.

Differential migration tests make removal of the legacy tools a controlled process
rather than a flag day.

If `aub` cannot justify a number from that chain, the correct output is not a
plausible approximation with confident formatting.

The correct output is a named missing fact, an explicit estimate, an explicit floor,
or no number at all.
