# Compile-fail coverage against design section 34.1

Section 34.1 names eleven compile-fail cases plus one exhaustiveness consequence.
This list maps each to the fixture that covers it, or to the bead that will own it
once its type exists. It is the single place `aub-rif`'s sixth success criterion is
checked, so a case cannot be dropped from this document while remaining in the design.

| # | Design case | Status | Fixture or owning bead |
| --- | --- | --- | --- |
| 1 | TokenCount + Credits | covered | cross_type_arithmetic.rs |
| 2 | QuotaUsed + Money | covered | quota_used_plus_money.rs |
| 3 | Credits passed to formatter expecting tokens | covered | token_kind_mismatch.rs |
| 4 | QuotaRemaining passed as QuotaUsed | covered | quota_used_where_remaining_expected.rs |
| 5 | USD added to another currency without conversion | covered | money_cross_currency.rs |
| 6 | quantity.unwrap_or_default() | covered | domain_quantities_unwrap_or_default.rs |
| 7 | print quantity with bare Display | covered | domain_quantities_no_display.rs, money_display.rs |
| 8 | construct WindowCalibration outside store/calibration module | deferred | aub-c0b.1 (create the calibration tables), with aub-c0b.14 (expose calibrated spend-to-window conversion) as the consumer that would trip it |
| 9 | construct CostModel without an observed TokenKind term | deferred | aub-ai3.1 (create immutable cost model tables), with aub-ai3.2 (fail-closed usage-to-credits conversion) as the consumer |
| 10 | combine Measured and Estimated evidence into Measured | covered by unit tests | quality_combine_measured_and_estimated_is_mixed, quality_combine_never_recovers_measured_from_estimated (src/evidence.rs) |
| 11 | read a Derivation::Unavailable as though it held a value | covered | derivation_unavailable_value.rs |
| 12 | adding a new known TokenKind breaks exhaustive model construction sites | deferred | aub-ai3.2, and aub-wyu.2 (valuation and its exact-money suite) for the valuation side |

Case 10 is not a compile-fail case: `combine` returns `EvidenceQuality<T>` at runtime, so the compiler cannot separate `Mixed` from `Measured`, and a `let` binding against either variant fails with the same E0005 refutable-pattern error, which is a fact about Rust's `let` and not about the lattice. The property is covered by the two unit tests named above, which fail when `combine` returns `Measured`.

The three deferred cases are attributed to the bead that introduces the type, with the
consumer that would trip the case named alongside, so the owning bead knows what it is
guarding when it lands.
