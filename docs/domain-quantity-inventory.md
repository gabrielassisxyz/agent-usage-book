# Domain quantity inventory

The Phase 0 negative-trait inventory (`aub-rif.12`): no `Default`, no free-standing
`Display`/`LowerHex`/other formatting trait, private representation with a validated
public smart constructor, and (for coefficient types and conversion witnesses) no
unchecked external construction.

## Scope of this inventory

This bead's formal dependencies are `aub-rif.1` (tokens), `aub-rif.2` (credits),
`aub-rif.3` (quota) and `aub-rif.4` (money): those are the only quantity beads the bead
graph requires closed before this one runs. The inventory below, and the guard script
that checks it (`bin/checks/70-quantity-inventory`), cover exactly those four files:
`src/domain/tokens.rs`, `src/domain/credits.rs`, `src/domain/quota.rs`,
`src/domain/money.rs`.

`src/domain/time.rs`, `src/domain/interval.rs`, `src/domain/ids.rs` and
`src/domain/provenance.rs` (from `aub-rif.5`, `aub-rif.6`, `aub-rif.7` and
`aub-rif.11`) already exist in the tree — the domain wave batched more beads than this
one's formal dependency edge names — but are **not yet covered** by this inventory or
its guard script. That is a real, visible gap, not an oversight papered over: extending
the guard's file list and this document to include them is exactly the kind of growth
the guard below exists to force, the first time someone runs it against those files.
Scoping the guard's *scan* to the four dependency files now, rather than failing
immediately on a backlog this bead did not create, is what keeps the guard green on
the day it is introduced instead of red from the first run.

`src/domain/window.rs` and whatever `aub-rif.9` and `aub-rif.13` land as are still
`in_progress` as of this writing and are excluded for the same reason: their shape
could still change before they close.

## Ordinary Phase 0 measurements

Private representation, a validated public smart constructor, no `Default`, no
free-standing formatting trait. Two types already had one of the two cases from their
own bead (`aub-rif.2`, `aub-rif.4`); this bead did not duplicate an existing, working,
already mutation-proved fixture, and the table says exactly which file covers which
case for every type.

| Type | File | `Default` case | `Display` case |
| --- | --- | --- | --- |
| `InputTokens` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `OutputTokens` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `CacheReadTokens` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `CacheWriteTokens` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `TokenCount` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `KnownTokenVector` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `UsageVector` | `tokens.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `Credits` | `credits.rs` | `credits_default.rs` (aub-rif.2) | `domain_quantities_no_display.rs` |
| `QuotaFractionPpm` | `quota.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `QuotaUsed` | `quota.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `QuotaRemaining` | `quota.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `PercentagePoints` | `quota.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `Money<Usd>` | `money.rs` | `domain_quantities_no_default.rs` | `money_display.rs` (aub-rif.4) |
| `MoneyPerMillionTokens<Usd>` | `money.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |

`Money<Usd>` and `MoneyPerMillionTokens<Usd>` are generic (`Money<C: Currency>`); each is
tested at one concrete currency, since the absence of an impl does not vary by which
currency instantiates the type parameter.

`Usd` and `Eur` themselves are uninhabited currency marker types, not quantities, and
are excluded (see the exclusion list below).

## Coefficient types and conversion witnesses

Construction is `pub(crate)`, restricted to this crate (the tightest boundary
expressible without a module-tree restructure; see `src/domain/credits.rs`'s module
documentation). Each has its own "construction outside the boundary" compile-fail case.

| Type | File | Compile-fail case |
| --- | --- | --- |
| `CreditsPerToken` | `credits.rs` | `credits_per_token_construction_outside_boundary.rs` |
| `CreditsPerPercentagePoint` | `credits.rs` | `credits_per_percentage_point_construction_outside_boundary.rs` (aub-rif.2) |

Future `WindowCalibration` and `CostModel` witnesses are explicitly out of scope here;
their construction compile-fail cases belong to `aub-c0b.1` and
`aub-ai3.1`/`aub-ai3.2` respectively.

## Excluded from the four scoped files

Named here, rather than left for the guard script to discover as an unexplained gap,
because each is a real `pub struct`/`pub enum` in a scoped file that is not a measured
quantity:

- `TokenKind` (`tokens.rs`) is a tag enum selecting which known kind a count belongs to,
  not itself a measured quantity.
- `Usd`, `Eur` (`money.rs`) are uninhabited currency marker types, never instantiated.

## Deliberately excluded from `time.rs`, `interval.rs`, `ids.rs`, `provenance.rs`

Recorded here so a future extension of the guard's file list knows what was already
looked at and ruled out, rather than re-litigating it:

- `MeasurementBasis` (`time.rs`) is a tag enum, not a measured quantity.
- `ClockAnomaly` (`time.rs`) is an error/event descriptor.
- `RealClock`, `FakeClock` (`time.rs`) are behavioral time sources, not values.
- `IntervalError` (`interval.rs`) is an error enum.
- `WitnessId`, `QuerySemantics`, `ProvenanceManifest`, `Derived<T>` (`provenance.rs`)
  are compound aggregates with their own bead-level (`aub-rif.11`) validated
  constructors, not scalar measurements.

## Representative, not exhaustive, checks

Two properties are structural consequences of Rust's privacy rules rather than a
per-type risk that varies by which quantity owns the field, so they are proven once per
distinct *shape* rather than once per every type name above:

- **Direct field/tuple construction outside the owning module** —
  `domain_quantity_direct_construction.rs` covers one tuple-struct-over-a-primitive
  (`InputTokens`), one tuple-struct-over-a-newtype (`QuotaUsed`), one
  multi-field brace struct (`KnownTokenVector`), and one generic struct
  (`Money<Usd>`).
- **`unwrap_or_default()` on `Option<Quantity>`** —
  `domain_quantities_unwrap_or_default.rs` covers `InputTokens`, `Credits`,
  `QuotaFractionPpm`, `Money<Usd>` and `PercentagePoints`. The underlying missing
  `Default` impl is already proven exhaustively by
  `domain_quantities_no_default.rs`; this fixture exists to prove the *generic-bound*
  code path (`T: Default`) fails the same way the direct associated-function call does,
  not to re-enumerate every type a second time.
