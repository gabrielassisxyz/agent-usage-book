// Rebuilding an irreplaceable class must not compile. The rebuild sweep takes a
// RebuildGroup, whose variants name groups, not classes, and whose class sets are
// derived from the shared durable-class taxonomy filtered to rebuildable classes
// (aub-lqe.11). Any attempt to name an irreplaceable class as the rebuild target
// is a type error: MeterAttempt, MeterAttemptResult, MeterResponseEvidence,
// MeterObservation, CalibrationExperiment and RateCard are absent from RebuildGroup
// and there is no conversion from DurableClass into it.

use agent_usage_book::store::retention::{DurableClass, delete_rebuildable};

fn main() {
    // MeterAttempt is irreplaceable evidence (meter attempt history is how
    // coverage distinguishes a dead scheduler from a dead endpoint) and must
    // not be addressable by delete_rebuildable, which accepts only RebuildGroup.
    // This must fail to compile because DurableClass is not RebuildGroup.
    let conn = unimplemented!();
    delete_rebuildable(conn, DurableClass::MeterAttempt);
}