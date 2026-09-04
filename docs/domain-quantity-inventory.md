# Domain quantity inventory

The Phase 0 negative-trait inventory (`aub-rif.12`, extended across all domain files by `aub-qgng`):
no `Default`, no free-standing `Display`/`LowerHex`/other formatting trait, private representation
with a validated public smart constructor, and (for coefficient types and conversion witnesses) no
unchecked external construction.

## Scope of this inventory

The inventory below, and the guard script that checks it (`bin/checks/70-quantity-inventory`),
dynamically cover every file under `src/domain/*.rs`. Every public struct or enum declared in any
domain file is explicitly inventoried: either as a measured quantity in the tables below, or in the
documented exclusion categories with its rationale.

A new public struct or enum added to any domain file must be added to this document, or the quantity
inventory guard will fail `bin/ci`.

## Ordinary Phase 0 measurements

Private representation, a validated public smart constructor, no `Default`, no free-standing
formatting trait.

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
| `RowCount` | `rows.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `Precision` | `render.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `UtcTimestamp` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `UtcDate` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `ProviderObservedAt` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `ReceivedAt` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `MonotonicDuration` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `MonotonicInstant` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `Age` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `ClockSkewEnvelope` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `Timeout` | `time.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |
| `Interval` | `interval.rs` | `domain_quantities_no_default.rs` | `domain_quantities_no_display.rs` |

`Money<Usd>` and `MoneyPerMillionTokens<Usd>` are generic (`Money<C: Currency>`); each is tested at
one concrete currency, since the absence of an impl does not vary by which currency instantiates
the type parameter.

## Coefficient types and conversion witnesses

Construction is `pub(crate)`, restricted to this crate (see `src/domain/credits.rs`). Each has its
own "construction outside the boundary" compile-fail case.

| Type | File | Compile-fail case |
| --- | --- | --- |
| `CreditsPerToken` | `credits.rs` | `credits_per_token_construction_outside_boundary.rs` |
| `CreditsPerPercentagePoint` | `credits.rs` | `credits_per_percentage_point_construction_outside_boundary.rs` (aub-rif.2) |

## Excluded categories

Each of the following is a public struct or enum declared in `src/domain/` that is not a measured
quantity, documented here with its reason for exclusion:

### Tags, currency markers and errors
- `TokenKind` (`tokens.rs`): tag enum selecting which known kind a count belongs to, not a measured quantity.
- `Usd`, `Eur` (`money.rs`): uninhabited currency marker types, never instantiated.
- `TokenClass` (`rate_card.rs`): tag enum selecting which token stream a rate prices, not a measured quantity.
- `BillingBasis` (`rate_card.rs`): tag enum selecting the unit a rate is quoted against.
- `CurrencyCode` (`rate_card.rs`): runtime currency marker enum a rate card carries as imported data; converting into a typed `Money<C>` is a named valuation function (aub-wyu.2), never a cast.
- `ReviewDuePolicy` (`rate_card.rs`): review-due policy tag for temporal reference data, a date or its recorded absence; replaces the freshness enum where authentication is nonsensical (PLAN.md section 25.3).
- `RateCardParseError` (`rate_card.rs`): error enum for rate value parse failures.
- `IntervalError` (`interval.rs`): error enum for interval construction failures.
- `MeasurementBasis` (`time.rs`): tag enum indicating observation basis, not a quantity.
- `ClockAnomaly` (`time.rs`): error and anomaly event descriptor.
- `RealClock`, `FakeClock` (`time.rs`): behavioral time sources, not values (`RealClock` implements `Default` to provide the default wall/monotonic clock).

### Namespaced and semantic identifiers (`ids.rs`)
- `SourceNamespace`: source system namespace wrapper.
- `NativeSessionId`, `NativeTaskId`, `NativeRunId`: un-namespaced raw identifier wrappers.
- `SessionId`, `TaskId`, `RunId`: namespaced identifier composite types.
- `MeterSemanticsId`, `BillingSemanticsId`, `ProviderContractId`, `AdapterVersion`, `CredentialContextId`: semantic contract and version identifiers.

### Attempt lifecycle and failure classifications (`attempt.rs`, `failure.rs`)
- `AttemptId`: monotonic sequence identifier for sampling attempts.
- `AttemptOutcome`, `AttemptStarted`, `AttemptResult`: attempt lifecycle state enums.
- `HttpStatusClass`, `FailureClass`, `AuthReason`: categorized failure classifications.
- `DueReason` (`attempt.rs`): tag enum for why an account was due for an attempt (`aub-me5.3`), not a measured quantity.

### Freshness models (`freshness.rs`)
- `Observed`, `StaleReason`, `Freshness`, `FreshnessKind`, `LatestAttempt`, `FreshnessInput`: freshness domain state models and inputs.

### Provenance and window descriptors (`provenance.rs`, `window.rs`)
- `Digest`, `EvidenceId`, `CostModelId`, `WindowCalibrationId`, `RateCardId`, `WitnessId`, `DerivationId`: provenance identifiers.
- `QuerySemantics`, `ProvenanceManifest`, `Derived`: compound provenance aggregates and wrappers.
- `WindowSemanticKey`, `ModelId`, `WindowScopeKind`, `WindowScope`, `ReportedResolution`, `QuantizationSemantics`, `NominalWindowDuration`, `MeterWindow`, `CreditHeadroomSelection`: window specification enums and composite structs.

### Authoritative surface comparison (`authoritative_comparison.rs`)
- `AuthoritativeComparisonVerdict`: tag enum with exactly two outcomes (agrees within granularity, unresolved mismatch), not a measured quantity. It has no `Default` and no free-standing formatting trait; `as_str` is an inherent method for the stable database spelling.
- `DocumentedGranularity`: a thin newtype over `QuotaFractionPpm` carrying the smallest difference the provider's authoritative surface is able to express. Private representation, a public smart constructor, no `Default`, no `Display`. It delegates its numeric bound to `QuotaFractionPpm`, so it has no dedicated compile-fail fixture of its own.

### Rate cards (`rate_card.rs`)
- `Publication`: provenance descriptor for one rate card, a source reference and a publication instant with absence explicit, not a quantity.
- `RateCardDraft`: the import payload for one rate component, a composite record whose rate is stored as exact integer micros rather than as a measured-quantity newtype; the monetary arithmetic stays in the money module.
- `RateCard`: a persisted rate card, the storage row identity plus import stamp plus draft; composite record, not a quantity.
