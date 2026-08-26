//! Tests for the shared test-support crate. These live in the main package's
//! integration-test tree so `bin/ci` runs them through the ordinary `cargo test`
//! gate, while the helpers themselves stay a dev-dependency that never reaches the
//! release binary.

use std::path::Path;
use std::sync::Mutex;

use test_support::{
    LogEvent, Rng, Seed, StateDir, assert_event, aub_binary_in, check_property, load_fixture,
};

/// A deterministic generator: the same seed yields the same string in every process.
fn generate_deterministic_string(seed: u64) -> String {
    let mut rng = Rng::new(Seed(seed));
    let mut out = String::new();
    for _ in 0..8 {
        out.push_str(&format!("{} ", rng.next_u64()));
    }
    out
}

/// Extracts the panic message from a caught panic payload.
fn panic_message(err: Box<dyn std::any::Any + Send>) -> String {
    match err.downcast::<String>() {
        Ok(s) => *s,
        Err(err) => match err.downcast::<&str>() {
            Ok(s) => (*s).to_string(),
            Err(_) => String::new(),
        },
    }
}

// --- state directory ---------------------------------------------------------

#[test]
fn state_dir_has_production_permissions() {
    let dir = StateDir::new();
    assert!(dir.path().is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "state dir must be 0700");
    }
}

#[test]
fn state_dir_removes_itself_after_a_panic() {
    let dir = StateDir::new();
    let path = dir.path().to_path_buf();
    assert!(path.is_dir());

    let result = std::panic::catch_unwind(move || {
        let _held = dir;
        panic!("deliberate panic to prove Drop runs");
    });
    assert!(result.is_err());
    assert!(
        !path.exists(),
        "state dir must be removed after a panic, but {} still exists",
        path.display()
    );
}

// --- binary locator ----------------------------------------------------------

fn profile_dir() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

#[test]
fn binary_locator_resolves_from_an_explicit_target_dir() {
    let target = std::env::temp_dir().join(format!("aub-target-{}", std::process::id()));
    let bin = target.join(profile_dir()).join("aub");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "fake binary").unwrap();

    assert_eq!(aub_binary_in(&target), bin);

    std::fs::remove_dir_all(&target).unwrap();
}

#[test]
fn binary_locator_fails_naming_the_expected_path_when_absent() {
    let target = std::env::temp_dir().join(format!("aub-empty-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let result = std::panic::catch_unwind(|| aub_binary_in(&target));
    let message = panic_message(result.expect_err("locator must fail when the binary is absent"));
    let expected = target.join(profile_dir()).join("aub");
    assert!(
        message.contains(&expected.display().to_string()),
        "failure must name the expected path {expected:?}, got: {message}"
    );

    std::fs::remove_dir_all(&target).unwrap();
}

#[test]
fn binary_locator_never_falls_back_to_path() {
    // Child mode: resolve from the given target dir and report found/not-found.
    if std::env::var("TEST_SUPPORT_CHILD_PATH_CHECK").is_ok() {
        let target = std::env::var("TEST_SUPPORT_CHILD_TARGET").unwrap();
        let out = std::env::var("TEST_SUPPORT_CHILD_OUT").unwrap();
        let found = std::panic::catch_unwind(|| aub_binary_in(Path::new(&target))).is_ok();
        std::fs::write(&out, if found { "found" } else { "not-found" }).unwrap();
        return;
    }

    // A fake `aub` on PATH must not satisfy the locator, which only looks in the
    // target directory. Run in a child so PATH can be set without mutating this
    // process's environment (set_var is unsafe in edition 2024).
    let fake_bin = std::env::temp_dir().join(format!("aub-fake-path-{}", std::process::id()));
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::write(fake_bin.join("aub"), "fake").unwrap();

    let target = std::env::temp_dir().join(format!("aub-empty-path-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let out_path = std::env::temp_dir().join(format!("aub-path-check-{}.txt", std::process::id()));
    let child_path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env("TEST_SUPPORT_CHILD_PATH_CHECK", "1")
        .env("TEST_SUPPORT_CHILD_TARGET", &target)
        .env("TEST_SUPPORT_CHILD_OUT", &out_path)
        .env("PATH", child_path)
        .arg("binary_locator_never_falls_back_to_path")
        .arg("--exact")
        .output()
        .unwrap();
    assert!(output.status.success(), "child process failed");

    let report = std::fs::read_to_string(&out_path).unwrap();
    let _ = std::fs::remove_file(&out_path);
    std::fs::remove_dir_all(&fake_bin).unwrap();
    std::fs::remove_dir_all(&target).unwrap();

    assert_eq!(report, "not-found", "locator must not fall back to PATH");
}

// --- fixture loading ---------------------------------------------------------

static CWD_LOCK: Mutex<()> = Mutex::new(());

struct CwdGuard {
    original: std::path::PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

fn with_cwd<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _lock = CWD_LOCK.lock().unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    let _guard = CwdGuard { original };
    f()
}

#[test]
fn fixture_loading_is_cwd_independent() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let relative = "tests/fixtures/sample.txt";
    let expected = load_fixture(crate_root, relative);

    let from_root = with_cwd(Path::new("/"), || load_fixture(crate_root, relative));
    let from_temp = with_cwd(&std::env::temp_dir(), || load_fixture(crate_root, relative));
    let from_crate = with_cwd(crate_root, || load_fixture(crate_root, relative));

    assert_eq!(from_root, expected);
    assert_eq!(from_temp, expected);
    assert_eq!(from_crate, expected);
}

// --- seeded determinism ------------------------------------------------------

#[test]
fn seeded_generator_is_identical_across_processes() {
    // Child mode: write the generator output for the requested seed to a file.
    if let (Ok(out_path), Ok(seed)) = (
        std::env::var("TEST_SUPPORT_CHILD_OUT"),
        std::env::var("TEST_SUPPORT_CHILD_SEED"),
    ) {
        let seed: u64 = seed.parse().expect("child seed must be numeric");
        std::fs::write(&out_path, generate_deterministic_string(seed)).unwrap();
        return;
    }

    let seed = 42u64;
    let expected = generate_deterministic_string(seed);
    let out_path = std::env::temp_dir().join(format!(
        "test-support-child-{}-{seed}.txt",
        std::process::id()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env("TEST_SUPPORT_CHILD_OUT", &out_path)
        .env("TEST_SUPPORT_CHILD_SEED", seed.to_string())
        .arg("seeded_generator_is_identical_across_processes")
        .arg("--exact")
        .output()
        .unwrap();
    assert!(output.status.success(), "child process failed");

    let actual = std::fs::read_to_string(&out_path).unwrap();
    let _ = std::fs::remove_file(&out_path);
    assert_eq!(
        actual, expected,
        "generator output must be identical across processes"
    );
}

#[test]
fn property_failure_names_the_seed() {
    let result = std::panic::catch_unwind(|| {
        check_property("always_false", 0..10, |seed| seed != 7);
    });
    let message = panic_message(result.expect_err("property must fail"));
    assert!(
        message.contains("seed 7"),
        "failure must name the failing seed, got: {message}"
    );
}

// --- log event assertion -----------------------------------------------------

#[test]
fn assert_event_fails_with_a_readable_diff() {
    let events = vec![
        LogEvent::new("attempt_started").field("account", "work-b"),
        LogEvent::new("attempt_finished").field("account", "work-b"),
    ];
    let result = std::panic::catch_unwind(|| {
        assert_event(&events, "attempt_started", "account", "work-a");
    });
    let message = panic_message(result.expect_err("assert_event must fail when absent"));
    assert!(
        message.contains("attempt_started"),
        "diff must name the expected event, got: {message}"
    );
    assert!(
        message.contains("work-a"),
        "diff must name the expected value, got: {message}"
    );
    assert!(
        message.contains("work-b"),
        "diff must list the actual events, got: {message}"
    );
}

#[test]
fn assert_event_passes_when_the_event_is_present() {
    let events = vec![LogEvent::new("attempt_started").field("account", "work-a")];
    assert_event(&events, "attempt_started", "account", "work-a");
}

// --- test-only guarantee -----------------------------------------------------

/// Reads the contents of one `[section]` out of a TOML document.
fn toml_section<'a>(manifest: &'a str, name: &str) -> &'a str {
    let header = format!("[{name}]");
    let start = manifest
        .find(&header)
        .map(|i| i + header.len())
        .unwrap_or(manifest.len());
    let rest = &manifest[start..];
    let end = rest
        .find("\n[")
        .map(|i| start + i)
        .unwrap_or(manifest.len());
    &manifest[start..end]
}

#[test]
fn test_support_is_a_dev_dependency_not_a_regular_dependency() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("Cargo.toml must be readable");
    let dependencies = toml_section(&manifest, "dependencies");
    let dev_dependencies = toml_section(&manifest, "dev-dependencies");

    assert!(
        dev_dependencies.contains("test-support"),
        "test-support must be a dev-dependency"
    );
    assert!(
        !dependencies.contains("test-support"),
        "test-support must not be a regular dependency, or it would reach the release binary"
    );
}
