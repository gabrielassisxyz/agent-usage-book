//! End-to-end tests for the compile-fail regeneration guard (aub-tojp).
//!
//! The guard is driven against a scratch crate that mirrors the repository's shape:
//! a trait, one implementor, and one compile-fail fixture whose capture names the
//! trait. The two scenarios from the bead are replayed: adding an impl elsewhere in
//! the crate changes only the `help:` text and must regenerate without ceremony,
//! while breaking the fixture into a different error code must be refused with both
//! codes named and the old capture restored.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GUARD_BIN: &str = env!("CARGO_BIN_EXE_compile_fail_regenerate");

const SCRATCH_HARNESS: &str = r#"#[test]
fn compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
"#;

const SCRATCH_LIB: &str = r#"//! Minimal trait graph for the compile-fail regeneration guard.

pub trait DomainQuantity {}

pub struct TokenCount;
impl DomainQuantity for TokenCount {}

pub struct Credits;

pub struct Interval<T: DomainQuantity> {
    _marker: std::marker::PhantomData<T>,
}

impl<T: DomainQuantity> Interval<T> {
    pub fn new(_lower: T, _upper: T) -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}
"#;

const SCRATCH_FIXTURE: &str = r#"// Compile-fail: `Interval<T>` requires `T` to be a domain quantity,
// so instantiating it over a bare primitive must not compile.

use {lib_name}::Interval;

fn main() {
    let _ = Interval::<f64>::new(0.0, 1.0);
}
"#;

const BROKEN_FIXTURE: &str = r#"// Compile-fail: this fixture now fails for a different reason: a
// mismatched type instead of an unsatisfied trait bound.

fn main() {
    let _: u32 = "not a number";
}
"#;

/// A scratch crate in the session temp directory, removed on drop. Each test uses a
/// distinct package name so the two crates never collide in the shared target
/// directory when the tests run in parallel.
struct ScratchCrate {
    root: PathBuf,
}

impl ScratchCrate {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-capture-guard-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests/compile_fail")).unwrap();

        let lib_name = format!("capture_guard_scratch_{name}");
        let manifest = format!(
            r#"[package]
name = "capture-guard-scratch-{name}"
version = "0.1.0"
edition = "2024"

[lib]
name = "{lib_name}"

[dev-dependencies]
trybuild = "=1.0.120"
"#
        );
        fs::write(root.join("Cargo.toml"), manifest).unwrap();
        fs::write(root.join("src/lib.rs"), SCRATCH_LIB).unwrap();
        fs::write(root.join("tests/compile_fail.rs"), SCRATCH_HARNESS).unwrap();
        fs::write(
            root.join("tests/compile_fail/interval_over_primitive.rs"),
            SCRATCH_FIXTURE.replace("{lib_name}", &lib_name),
        )
        .unwrap();
        Self { root }
    }

    fn fixture(&self) -> PathBuf {
        self.root
            .join("tests/compile_fail/interval_over_primitive.stderr")
    }

    /// Adds an impl for a second type, the trait-graph move that changes only the
    /// `help:` text of the fixture's error.
    fn add_credits_impl(&self) {
        let lib = self.root.join("src/lib.rs");
        let mut source = fs::read_to_string(&lib).unwrap();
        source.push_str("\nimpl DomainQuantity for Credits {}\n");
        fs::write(lib, source).unwrap();
    }

    /// Rewrites the fixture so it fails for a different reason: E0308 instead of
    /// E0277.
    fn break_fixture(&self) {
        fs::write(
            self.root
                .join("tests/compile_fail/interval_over_primitive.rs"),
            BROKEN_FIXTURE,
        )
        .unwrap();
    }
}

impl Drop for ScratchCrate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_guard(root: &Path, override_flag: bool) -> Output {
    let mut command = Command::new(GUARD_BIN);
    command.arg("--crate-root").arg(root);
    if override_flag {
        command.arg("--override");
    }
    command.output().expect("the guard binary must run")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The reproduction from the bead, first half: adding an impl elsewhere in the crate
/// changes only the `help:` text under an unchanged error code, and regeneration
/// proceeds without ceremony.
#[test]
fn adding_an_impl_elsewhere_regenerates_under_an_unchanged_code() {
    let scratch = ScratchCrate::new("impl");

    // A brand-new fixture has no capture to compare against: the guard refuses by
    // default and requires the explicit override.
    let first = run_guard(&scratch.root, false);
    assert!(
        !first.status.success(),
        "a new fixture must be refused without --override:\n{}",
        stderr_of(&first)
    );

    // The initial capture is created with the explicit override.
    let initial = run_guard(&scratch.root, true);
    assert!(
        initial.status.success(),
        "the initial capture must be created with --override:\n{}",
        stderr_of(&initial)
    );
    let captured = fs::read_to_string(scratch.fixture()).unwrap();
    assert!(
        captured.contains("E0277"),
        "the initial capture must carry E0277, got:\n{captured}"
    );

    // An impl added elsewhere in the crate adds a help block but leaves the code
    // unchanged: regeneration proceeds without ceremony.
    scratch.add_credits_impl();
    let regenerated = run_guard(&scratch.root, false);
    assert!(
        regenerated.status.success(),
        "an unchanged error code must regenerate without --override:\n{}",
        stderr_of(&regenerated)
    );
    let updated = fs::read_to_string(scratch.fixture()).unwrap();
    assert!(
        updated.contains("E0277") && updated.contains("Credits"),
        "the regenerated capture must keep E0277 and gain the new help block, got:\n{updated}"
    );
}

/// The reproduction from the bead, second half: a fixture that now fails for a
/// different reason is refused, both codes are named, and the old capture is
/// restored rather than blessed.
#[test]
fn a_changed_error_code_is_refused_and_the_capture_is_restored() {
    let scratch = ScratchCrate::new("code_change");

    let initial = run_guard(&scratch.root, true);
    assert!(
        initial.status.success(),
        "the initial capture must be created with --override:\n{}",
        stderr_of(&initial)
    );
    let before = fs::read_to_string(scratch.fixture()).unwrap();

    // The fixture now fails for a different reason: E0308 instead of E0277.
    scratch.break_fixture();
    let refused = run_guard(&scratch.root, false);
    assert!(
        !refused.status.success(),
        "a changed error code must be refused"
    );
    let message = stderr_of(&refused);
    assert!(
        message.contains("E0277") && message.contains("E0308"),
        "the refusal must name both codes, got:\n{message}"
    );
    let after = fs::read_to_string(scratch.fixture()).unwrap();
    assert_eq!(
        after, before,
        "the refused capture must be restored to its prior content"
    );

    // The explicit override is what a deliberate change goes through.
    let forced = run_guard(&scratch.root, true);
    assert!(
        forced.status.success(),
        "--override must force the regeneration:\n{}",
        stderr_of(&forced)
    );
    let forced_capture = fs::read_to_string(scratch.fixture()).unwrap();
    assert!(
        forced_capture.contains("E0308"),
        "the forced capture must carry the new code, got:\n{forced_capture}"
    );
}
