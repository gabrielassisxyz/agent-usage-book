# Invariants

This document repeats the 27 invariants from the design (PLAN.md section 42) in
the source tree, each with the module that enforces it and the test, lint or
database constraint that would catch a violation. A rule in a 5,000 line design
document is read once by whoever converts it into beads; a rule here is read by
whoever is about to break it.

## The five correctness dimensions

The system has five independent correctness dimensions, and none substitutes
for another:

1. **Unit**: what physical quantity is this?
2. **Freshness**: how current is this remote reading?
3. **Coverage**: does this value include everything its name implies?
4. **Evidence quality**: were the included values measured or reconstructed?
5. **Provenance**: what evidence and conversion witnesses justify it?

An exact but stale meter reading is stale. A fresh provider reading may still
be incomplete if the provider omits a required window. A current token estimate
reconstructed from characters is still an estimate, not measured usage. A
numerically plausible result without a provenance chain is not publishable.

## The invariants

Numbering matches PLAN.md section 42, so a reference to invariant 23 means the
same thing in both documents.

| # | Invariant | Enforcing module | Test or check |
|---:|---|---|---|
| 1 | Provider quota attempts and observations are irreplaceable evidence. | store, backup | retention policy (no auto-prune); backup restore test |
| 2 | Transcript-derived spend is reconstructible. | transcripts, store | rebuild test |
| 3 | No semantically meaningful raw numeric primitive crosses a domain boundary. | domain | compile-time newtype boundary (no `From` between quantities) |
| 4 | Freshness is an exhaustive three-way state. | domain | exhaustive match over `Freshness` (no `Option` or bool) |
| 5 | Sampling-attempt outcome is not the same dimension as freshness. | meter, domain | distinct types (attempt outcome vs freshness) |
| 6 | A failed source never produces zero. | meter, presentation | unit test (failed source yields an error, never zero) |
| 7 | A historical value is never presented without its actual observation time. | report, presentation | unit test (typed report carries observation time) |
| 8 | Provider credentials are resolved from named-account configuration, never accidental ambient state. | auth, config | unit test (credentials from config, not ambient env) |
| 9 | All replay deduplication passes through one semantic event-identity framework and database uniqueness constraint. | dedup, store | dedup test; database uniqueness constraint |
| 10 | Unknown token components block complete conversions. | cost_model, valuation | unit test (unknown component blocks conversion) |
| 11 | Cost-model coefficients and window calibrations are distinct versioned witnesses. | cost_model, calibration | unit test (distinct witness types) |
| 12 | Calibrated values have immutable IDs and no consumer-side literals. | calibration, config | unit test; lint (no consumer-side literals) |
| 13 | Calibration cannot cross incompatible plan tiers, reset boundaries, or provider semantics. | calibration | unit test (cross-tier fit rejected) |
| 14 | Passive calibration is evidence and candidate generation, not automatic truth. | calibration | unit test (passive fit produces a candidate, not an active calibration) |
| 15 | `status` never opens SQLite, performs network I/O, or writes. | projection, cli | status-latency test (bin/checks) |
| 16 | The projection is disposable and contains no stored freshness boolean. | projection | unit test (projection schema has no `is_fresh` field) |
| 17 | Task ambiguity becomes explicit overhead. | attribution | unit test (ambiguous task goes to the overhead bucket) |
| 18 | Dollar valuation and subscription credits are distinct dimensions. | valuation, cost_model | compile-time distinct types |
| 19 | Meter residual is diagnostic and is not automatically called hidden token spend. | coverage, report | unit test (residual labeled "residual", not hidden spend) |
| 20 | Estimated transcript usage is never silently promoted to measured usage. | transcripts, evidence | unit test (estimated never labeled measured) |
| 21 | The friction ledger remains external and joins only through stable IDs. | sessions, attribution | unit test (friction ledger external, stable-ID join) |
| 22 | Irreplaceable meter history is backed up and never automatically pruned. | backup, store | backup restore test; retention test (no auto-prune) |
| 23 | An outbound meter request is never begun before its attempt identity is durable. | meter, store | unit test (attempt start survives a kill) |
| 24 | An attempt without a terminal result is evidence of collector interruption, not evidence that no attempt occurred. | meter, store | unit test (interrupted attempt reported as interruption) |
| 25 | Provider response evidence and its normalized interpretation are separate records; a corrected adapter reinterprets retained evidence and never overwrites the earlier interpretation. | evidence, meter | unit test (reinterpretation preserves the earlier record) |
| 26 | Workload feasibility is evaluated against every constraining window in calibrated credits, never against the lowest remaining percentage alone. | advice | unit test (feasibility over every constraining window) |
| 27 | Coverage denominators come from the sampling policy that was in force over the interval, never from current configuration. | coverage, config | unit test (denominator from the policy snapshot) |

## Enforcement status

Every invariant above names a mechanical enforcer: the module that owns the
rule and the test, lint or database constraint that would catch a violation.
No invariant is unenforced. The enforcement is specified by the design and
lands with the module implementations in later beads; the modules are
currently skeletons, so the enforcers named here are the design's contract
rather than code that exists today.
