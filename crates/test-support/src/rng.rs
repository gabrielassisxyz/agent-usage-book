//! A small, auditable deterministic PRNG and a minimal property-test runner.

/// A 64-bit seed for a deterministic generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Seed(pub u64);

/// A deterministic pseudo-random generator (splitmix64). The same seed yields the same
/// stream on every machine and in every process, which is what makes a failing
/// property test reproducible.
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator seeded with the given value.
    pub fn new(seed: Seed) -> Self {
        Rng { state: seed.0 }
    }

    /// The next 64-bit value in the stream.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`, uniform enough for fixture generation.
    pub fn next_below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

/// Runs `predicate` over every seed and panics naming the first seed that fails, so a
/// property failure is reproducible by construction.
pub fn check_property<F>(name: &str, seeds: impl IntoIterator<Item = u64>, mut predicate: F)
where
    F: FnMut(u64) -> bool,
{
    for seed in seeds {
        if !predicate(seed) {
            panic!("property {name:?} failed for seed {seed}");
        }
    }
}
