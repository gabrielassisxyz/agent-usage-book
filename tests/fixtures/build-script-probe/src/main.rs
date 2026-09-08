//! Probe binary for the shared-target build-script guard: it exists only so
//! `tests/build_script.rs` can build a tiny crate that uses this repository's
//! real `build.rs`, instead of building the whole crate five times.

fn main() {
    println!("{}", env!("AUB_GIT_REVISION"));
    println!("{}", env!("AUB_TOOLCHAIN_VERSION"));
}
