//! Tests for the build script's toolchain-file resolution.
//!
//! The build script is its own crate: cargo compiles it as a binary and executes it
//! before building the package, and `cargo test` never runs `#[cfg(test)]` code inside
//! it. Including the source here compiles it into a test target so the resolution
//! helper is testable under the ordinary gate.
//!
//! The integration test below is the reproduction of the defect this file exists to
//! guard: a build script compiled for one worktree, cached under a shared
//! `CARGO_TARGET_DIR`, and handed to another worktree whose manifest directory differs
//! from the one baked into the binary. The crate being built is the
//! `build-script-probe` fixture, a few lines that use this repository's real
//! `build.rs`, so the reproduction runs in seconds instead of minutes.

#![allow(dead_code)] // the test crate exercises only the resolution helper; the rest of the build script is compiled for fidelity

#[path = "../build.rs"]
mod build_script;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The resolution helper takes the manifest directory as a parameter, so the
/// compile-time constant is not the only input: the build script feeds it the
/// `CARGO_MANIFEST_DIR` environment variable at run time.
#[test]
fn toolchain_file_resolves_under_the_given_manifest_dir() {
    let manifest_dir = Path::new("/some/manifest/dir");
    assert_eq!(
        build_script::toolchain_file_path(manifest_dir),
        PathBuf::from("/some/manifest/dir/rust-toolchain.toml")
    );
}

/// Removes every worktree the test created, even when an assertion failed, so a
/// failing run cannot leave stale registrations in the repository's worktree list.
struct WorktreeGuard {
    repo: PathBuf,
    paths: Vec<PathBuf>,
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(path)
                .current_dir(&self.repo)
                .output();
        }
    }
}

fn worktree_add(repo: &Path, path: &Path) {
    let out = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(path)
        .arg("HEAD")
        .current_dir(repo)
        .output()
        .expect("git must run");
    assert!(
        out.status.success(),
        "git worktree add {} failed:\n{}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn cargo_build(dir: &Path, shared_target: &Path) -> std::process::Output {
    Command::new("cargo")
        .args(["build"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", shared_target)
        .output()
        .expect("cargo must run")
}

fn assert_build_succeeds(dir: &Path, shared_target: &Path, label: &str) {
    let out = cargo_build(dir, shared_target);
    assert!(
        out.status.success(),
        "{label}: cargo build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The reproduction: with a shared `CARGO_TARGET_DIR`, a build script compiled for one
/// checkout is cached and handed to another whose manifest directory differs from the
/// one baked into the binary. What is built is the `build-script-probe` fixture, a
/// package of a few lines whose `build` key points at this repository's real
/// `build.rs`, so every assertion below exercises the script the crate actually ships.
/// Cargo records the build script's source as an absolute path and reuses the cached
/// binary while that recorded file exists with a matching mtime, so the two checkouts
/// carry identical `build.rs` mtimes and differ only by path. The sequence is: build
/// A, remove A, build B twice, then C and D in parallel. Removing A deletes the
/// source cargo recorded for the cached binary, so B recompiles the script instead of
/// reusing A's binary; C and D then reuse B's cached binary across checkout paths,
/// which is the sharing this guard relies on. Under the old `env!` implementation the
/// cached binary bakes the compiling checkout's manifest directory and ignores its
/// runtime environment, so the direct run of the cached binary below exposes it;
/// reading `CARGO_MANIFEST_DIR` at run time makes the cached binary serve whichever
/// checkout runs it.
///
/// The fixture is staged inside each `git worktree add --detach HEAD` checkout at the
/// same relative path it occupies in this repository, so its
/// `build = "../../../build.rs"` keeps pointing at that checkout's real `build.rs` (a
/// path to the file, never a copy) and `git rev-parse HEAD` inside the script succeeds
/// because the probe sits under the checkout's `.git`. The worktrees are required for
/// exactly that: four plain directories would build with `AUB_GIT_REVISION=unknown`.
#[test]
fn alternating_worktree_builds_with_a_shared_target_dir_never_fail() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmp = std::env::temp_dir().join(format!(
        "aub-xwx2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must be after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).expect("temp dir must be creatable");
    let shared = tmp.join("shared-target");
    let a = tmp.join("A");
    let b = tmp.join("B");
    let mut guard = WorktreeGuard {
        repo: repo.clone(),
        paths: vec![a.clone(), b.clone()],
    };

    worktree_add(&repo, &a);
    worktree_add(&repo, &b);
    stage_probe_fixture(&repo, &a);
    stage_probe_fixture(&repo, &b);

    // The mtime A's build.rs carries at build time is what the dep-info records; B's
    // build.rs must carry exactly that mtime or cargo recompiles the script instead of
    // reusing the cached binary.
    let a_modified = std::fs::metadata(a.join("build.rs"))
        .expect("A/build.rs metadata must be readable")
        .modified()
        .expect("A/build.rs mtime must be readable");

    assert_build_succeeds(&probe_dir(&a), &shared, "A (first)");

    // Removing A makes the path baked into the cached build-script binary stale.
    let out = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&a)
        .current_dir(&repo)
        .output()
        .expect("git must run");
    assert!(
        out.status.success(),
        "git worktree remove failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    File::open(b.join("build.rs"))
        .expect("B/build.rs must be readable")
        .set_times(std::fs::FileTimes::new().set_modified(a_modified))
        .expect("B/build.rs times must be settable");

    // Removing A deleted the script source cargo recorded, so this build recompiles
    // the script rather than reusing A's binary. C and D below are the builds that
    // reuse a cached binary across checkout paths.
    assert_build_succeeds(&probe_dir(&b), &shared, "B (after A removed)");
    assert_build_succeeds(&probe_dir(&b), &shared, "B (again)");

    // In parallel: two worktrees building at once against the same shared target
    // directory. Cargo serializes them on its target-directory lock; both must
    // succeed.
    let c = tmp.join("C");
    let d = tmp.join("D");
    guard.paths.push(c.clone());
    guard.paths.push(d.clone());
    worktree_add(&repo, &c);
    worktree_add(&repo, &d);
    stage_probe_fixture(&repo, &c);
    stage_probe_fixture(&repo, &d);
    std::thread::scope(|s| {
        let c_build = s.spawn(|| cargo_build(&probe_dir(&c), &shared));
        let d_build = s.spawn(|| cargo_build(&probe_dir(&d), &shared));
        let c_out = c_build.join().expect("C build thread must not panic");
        let d_out = d_build.join().expect("D build thread must not panic");
        assert!(
            c_out.status.success(),
            "C (parallel): cargo build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&c_out.stdout),
            String::from_utf8_lossy(&c_out.stderr)
        );
        assert!(
            d_out.status.success(),
            "D (parallel): cargo build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&d_out.stdout),
            String::from_utf8_lossy(&d_out.stderr)
        );
    });

    // The panic for a genuinely missing file must name the path that was tried, so
    // the next reader is not sent to a file that is correct. Run the cached
    // build-script binary with a manifest directory that has no toolchain file.
    let empty_manifest = tmp.join("empty-manifest");
    std::fs::create_dir_all(&empty_manifest).expect("empty manifest dir must be creatable");
    let script = find_build_script_binary(&shared);
    let out = Command::new(&script)
        .env("CARGO_MANIFEST_DIR", &empty_manifest)
        .output()
        .expect("build script binary must run");
    assert!(
        !out.status.success(),
        "build script must fail without a toolchain file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let expected = format!(
        "cannot read {}/rust-toolchain.toml",
        empty_manifest.display()
    );
    assert!(
        stderr.contains(&expected),
        "panic must name the path that was tried, got:\n{stderr}"
    );

    drop(guard);
    std::fs::remove_dir_all(&tmp).expect("temp dir must be removable");
}

/// The probe package as built: `tests/fixtures/build-script-probe` under the given
/// checkout, which is what keeps its `build = "../../../build.rs"` pointing at that
/// checkout's real build script.
fn probe_dir(checkout: &Path) -> PathBuf {
    checkout.join("tests/fixtures/build-script-probe")
}

/// Copies the probe package from the checkout running this test into the given
/// worktree checkout at the same relative path, so the copy's `build` key resolves to
/// that checkout's real `build.rs`, and stages that checkout's toolchain file beside
/// the probe's manifest, which is where the build script reads it from at run time.
/// Copying from the running checkout rather than relying on what the worktree checked
/// out keeps the test faithful to the working tree while the fixture itself is edited.
fn stage_probe_fixture(repo: &Path, checkout: &Path) {
    let src = repo.join("tests/fixtures/build-script-probe");
    let dst = probe_dir(checkout);
    if dst.exists() {
        std::fs::remove_dir_all(&dst).expect("stale staged probe must be removable");
    }
    std::fs::create_dir_all(dst.join("src")).expect("staged probe src dir must be creatable");
    for file in ["Cargo.toml", "src/main.rs"] {
        std::fs::copy(src.join(file), dst.join(file))
            .unwrap_or_else(|_| panic!("staged probe {file} must be copyable"));
    }
    std::fs::copy(
        repo.join("rust-toolchain.toml"),
        dst.join("rust-toolchain.toml"),
    )
    .expect("staged probe toolchain file must be copyable");
}

/// The compiled build-script binary under a shared target directory. The build
/// output directory holds several `build-script-probe-*` entries; the binary is the
/// one that carries `build-script-build`.
fn find_build_script_binary(shared_target: &Path) -> PathBuf {
    let build_dir = shared_target.join("debug/build");
    let mut found = None;
    for entry in std::fs::read_dir(&build_dir).expect("build dir must exist") {
        let entry = entry.expect("build dir must be readable");
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("build-script-probe-") {
            let candidate = entry.path().join("build-script-build");
            if candidate.is_file() {
                found = Some(candidate);
            }
        }
    }
    found.expect("a build-script-build binary must exist under the shared target dir")
}
