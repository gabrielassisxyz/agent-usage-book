// Pruning an irreplaceable class must not compile. The PruneTarget enum
// structurally excludes all irreplaceable classes: MeterAttempt, MeterAttemptResult,
// MeterResponseEvidence, CalibrationExperiment, SampleRun, LedgerGeneration and
// SessionAccountMarker are absent from it. Any attempt to pass a DurableClass variant
// for an irreplaceable class where a PruneTarget is expected is a type error.

use agent_usage_book::store::retention::{DurableClass, prune_target};

fn main() {
    // MeterAttempt is irreplaceable evidence and must not be addressable by
    // prune_target, which accepts only PruneTarget (not DurableClass).
    // This call must fail to compile because DurableClass is not PruneTarget.
    let conn = unimplemented!();
    prune_target(&conn, DurableClass::MeterAttempt);
}
