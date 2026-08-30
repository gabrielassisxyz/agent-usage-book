//! State-directory readiness: permissions and locality, checked before anything
//! irreplaceable is written and before any network request is made.
//!
//! This module owns the checks themselves, not their aggregation, rendering or exit
//! classification: `aub-n27.7`'s `aub doctor` is the eventual consumer of the typed
//! facts and repair capabilities this module exposes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Filesystem types this project refuses to hold state on. SQLite's WAL mode assumes
/// `mmap`/`fsync` semantics a network filesystem does not reliably give it (`AGENTS.md`
/// "Persistence"; `docs/PLAN.md` line 685: "The state directory must be on a local
/// filesystem."). Matched case-insensitively against the second field of a
/// `/proc/mounts` line.
const REJECTED_FILESYSTEM_TYPES: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb2",
    "smbfs",
    "9p",
    "afs",
    "fuse.sshfs",
    "davfs",
];

/// Looks up the filesystem type mounted at (or above) a path. A trait so a network
/// mount can be simulated in a test without a real share: production uses
/// [`ProcMounts`], tests use [`FakeMountTable`].
pub trait MountTable {
    /// The filesystem type of the mount point that owns `path`, or `None` when no
    /// entry matches. `None` is treated as "could not be determined" by
    /// [`ensure_state_dir_ready`], which does not accept it as proof of locality.
    fn filesystem_type_for(&self, path: &Path) -> Option<String>;
}

/// Reads the real `/proc/mounts`.
pub struct ProcMounts;

impl MountTable for ProcMounts {
    fn filesystem_type_for(&self, path: &Path) -> Option<String> {
        let contents = fs::read_to_string("/proc/mounts").ok()?;
        filesystem_type_from_mounts(&contents, path)
    }
}

/// Parses `/proc/mounts`-shaped text (`device mount_point fstype options freq
/// passno`) and returns the type at the longest matching mount-point prefix of
/// `path`, mirroring the "most specific mount wins" rule `df` itself uses. Free of
/// filesystem access so [`ProcMounts`] and its own parser test share one
/// implementation.
fn filesystem_type_from_mounts(mounts_text: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mounts_text.lines() {
        let mut fields = line.split_whitespace();
        let _device = fields.next()?;
        let mount_point = fields.next()?;
        let fstype = fields.next()?;
        if path.starts_with(mount_point) {
            let len = mount_point.len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, fstype.to_string()));
            }
        }
    }
    best.map(|(_, fstype)| fstype)
}

/// A fixed lookup table, for tests: no real mount is needed to prove the rejection
/// logic runs, only that a filesystem-type answer of a given shape is rejected.
#[derive(Default)]
pub struct FakeMountTable(BTreeMap<PathBuf, String>);

impl FakeMountTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mount(mut self, path: impl Into<PathBuf>, filesystem_type: impl Into<String>) -> Self {
        self.0.insert(path.into(), filesystem_type.into());
        self
    }
}

impl MountTable for FakeMountTable {
    fn filesystem_type_for(&self, path: &Path) -> Option<String> {
        self.0
            .iter()
            .filter(|(mount_point, _)| path.starts_with(mount_point))
            .max_by_key(|(mount_point, _)| mount_point.as_os_str().len())
            .map(|(_, fstype)| fstype.clone())
    }
}

fn is_rejected(filesystem_type: &str) -> bool {
    REJECTED_FILESYSTEM_TYPES.contains(&filesystem_type.to_lowercase().as_str())
}

/// The nearest ancestor of `path` that exists, used to probe the filesystem type
/// before the state directory itself has necessarily been created.
fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return candidate.to_path_buf();
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return candidate.to_path_buf(),
        }
    }
}

/// Refuses when the state directory's own path is a symlink: the permission and
/// filesystem checks in this module would otherwise silently apply to wherever the
/// link resolves rather than to the path that was configured, which defeats both
/// checks without either one failing.
fn reject_symlinked_state_dir(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Store(format!(
            "state directory {path:?} is a symlink; refusing to follow it rather than apply \
             permission and filesystem checks to a location the configured path does not name"
        ))),
        _ => Ok(()),
    }
}

/// Creates `path` (and its ancestors) if missing, then forces it to mode 0700. An
/// existing directory keeps its content but still has its mode forced: a directory
/// that was ever created wider than intended is the hostile case this bead exists to
/// close, not one to leave in place once found.
#[cfg(unix)]
fn ensure_dir_mode_0700(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path).map_err(|error| {
        Error::Store(format!("cannot create state directory {path:?}: {error}"))
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        Error::Store(format!(
            "cannot set state directory {path:?} to mode 0700: {error}"
        ))
    })
}

#[cfg(not(unix))]
fn ensure_dir_mode_0700(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path)
        .map_err(|error| Error::Store(format!("cannot create state directory {path:?}: {error}")))
}

/// Opens (creating if absent) a file inside the state directory at mode 0600, "where
/// the platform permits" (`docs/PLAN.md` line 4775). The database, projection and
/// spool files this project's later beads create should go through this rather than
/// each re-deriving the permission mode by hand.
///
/// `.mode(0o600)` only takes effect when this call is the one that creates the file
/// (POSIX `open(2)`): a pre-existing file keeps whatever mode it already had. Rather
/// than refuse a database found at a wider mode, this repairs it in place, the same
/// policy `ensure_dir_mode_0700` above applies to the state directory itself
/// (`aub-sth.2`): a file that was ever created wider than intended is the case this
/// bead exists to close, not one to leave standing once found.
#[cfg(unix)]
pub fn create_file_mode_0600(path: &Path) -> Result<fs::File, Error> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::Store(format!("cannot create {path:?} at mode 0600: {error}")))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| Error::Store(format!("cannot set {path:?} to mode 0600: {error}")))?;
    Ok(file)
}

#[cfg(not(unix))]
pub fn create_file_mode_0600(path: &Path) -> Result<fs::File, Error> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| Error::Store(format!("cannot create {path:?}: {error}")))
}

/// Forces an existing file to mode 0600, the same repair-not-refuse policy
/// [`create_file_mode_0600`] applies to the main database file. For SQLite's `-wal`
/// and `-shm` sidecars, which this project cannot create itself (SQLite owns their
/// creation) and so cannot pass a mode to at creation time. A no-op when `path` does
/// not exist: a sidecar is conditional on journal mode and page-cache activity, and
/// its absence is not a defect.
#[cfg(unix)]
pub fn force_file_mode_0600(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Store(format!(
            "cannot set {path:?} to mode 0600: {error}"
        ))),
    }
}

#[cfg(not(unix))]
pub fn force_file_mode_0600(_path: &Path) -> Result<(), Error> {
    Ok(())
}

/// Proves the directory is actually writable by this process, rather than trusting
/// its mode bits: mode 0700 can still sit on a read-only mount, and a bit pattern is
/// not the same claim as a successful write. Writes and removes a probe file.
fn verify_writable(path: &Path) -> Result<(), Error> {
    let probe = path.join(".aub-write-probe");
    fs::write(&probe, b"").map_err(|error| {
        Error::Store(format!("state directory {path:?} is not writable: {error}"))
    })?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// The full startup check, in order: refuse a symlinked path, refuse a rejected
/// filesystem (probed at the nearest existing ancestor, so this runs even before the
/// directory itself exists), create the directory at mode 0700, then prove it is
/// actually writable. Every failure is [`Error::Store`], the class this project
/// reserves for durable-state failures, never a generic one.
pub fn ensure_state_dir_ready(path: &Path, mounts: &dyn MountTable) -> Result<(), Error> {
    reject_symlinked_state_dir(path)?;

    let probe_point = nearest_existing_ancestor(path);
    if let Some(filesystem_type) = mounts.filesystem_type_for(&probe_point)
        && is_rejected(&filesystem_type)
    {
        return Err(Error::Store(format!(
            "state directory {path:?} is on a {filesystem_type} filesystem, which this \
             project refuses: the state directory must be on a local filesystem \
             (docs/PLAN.md \"Persistence\": WAL is not safe on a network filesystem)"
        )));
    }

    ensure_dir_mode_0700(path)?;
    verify_writable(path)
}

/// Runs `then` only after [`ensure_state_dir_ready`] succeeds, so any code inside
/// `then` is provably reachable only once the state directory is verified local,
/// present and writable. Later mutating commands (`aub-eun.6`'s `aub sample` and its
/// kin) wrap their first network-touching call in this rather than checking the
/// state directory and making the call independently, which is what makes the
/// ordering a property of the call graph instead of a convention every call site has
/// to remember on its own.
pub fn run_after_state_check<T>(
    path: &Path,
    mounts: &dyn MountTable,
    then: impl FnOnce() -> T,
) -> Result<T, Error> {
    ensure_state_dir_ready(path, mounts)?;
    Ok(then())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop. Distinct
    /// from `test_support::StateDir`: these tests need to construct hostile
    /// permission states this crate must refuse, not the production mode the shared
    /// helper deliberately locks in.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("aub-startup-test-{}-{suffix}", std::process::id()));
            fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            // Best effort: tighten before removal in case a test left it
            // unreadable/unwritable, or `remove_dir_all` cannot descend into it.
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // --- ensure_dir_mode_0700 -------------------------------------------------

    #[test]
    fn ensure_dir_mode_0700_creates_a_missing_directory_at_mode_0700() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("new-state-dir");
        ensure_dir_mode_0700(&target).unwrap();
        assert_eq!(mode_of(&target), 0o700);
    }

    #[test]
    fn ensure_dir_mode_0700_tightens_an_existing_wider_directory() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("wide-state-dir");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(mode_of(&target), 0o777);

        ensure_dir_mode_0700(&target).unwrap();
        assert_eq!(mode_of(&target), 0o700);
    }

    // --- create_file_mode_0600 -------------------------------------------------

    #[test]
    fn create_file_mode_0600_sets_the_mode_on_a_new_file() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("spool.bin");
        create_file_mode_0600(&target).unwrap();
        assert_eq!(mode_of(&target), 0o600);
    }

    /// Planted negative: a pre-existing file found at a wider mode is repaired to
    /// 0600 rather than left as-is, since `.mode()` only takes effect at creation
    /// and a naive implementation would silently keep the wider mode.
    #[test]
    fn create_file_mode_0600_repairs_a_preexisting_wider_file() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("spool.bin");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode_of(&target), 0o644);

        create_file_mode_0600(&target).unwrap();
        assert_eq!(mode_of(&target), 0o600);
    }

    // --- force_file_mode_0600 -----------------------------------------------------

    #[test]
    fn force_file_mode_0600_tightens_an_existing_wider_file() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("meter.db-wal");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        force_file_mode_0600(&target).unwrap();
        assert_eq!(mode_of(&target), 0o600);
    }

    #[test]
    fn force_file_mode_0600_is_a_no_op_when_the_file_does_not_exist() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("meter.db-shm");
        force_file_mode_0600(&target).unwrap();
        assert!(!target.exists());
    }

    // --- verify_writable ---------------------------------------------------------

    #[test]
    fn verify_writable_fails_when_the_directory_lacks_the_write_bit() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("read-only-dir");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o500)).unwrap();

        let err = verify_writable(&target).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Store);
    }

    #[test]
    fn verify_writable_succeeds_on_a_normal_writable_directory() {
        let scratch = ScratchDir::new();
        assert!(verify_writable(scratch.path()).is_ok());
    }

    // --- filesystem_type_from_mounts (parser) -------------------------------------

    #[test]
    fn filesystem_type_from_mounts_picks_the_longest_matching_mount_point() {
        let mounts = "\
/dev/sda1 / ext4 rw,relatime 0 0
fileserver:/export /home nfs rw,relatime 0 0
";
        assert_eq!(
            filesystem_type_from_mounts(mounts, Path::new("/home/gabriel/.local/state/aub")),
            Some("nfs".to_string())
        );
        assert_eq!(
            filesystem_type_from_mounts(mounts, Path::new("/var/lib/aub")),
            Some("ext4".to_string())
        );
    }

    #[test]
    fn filesystem_type_from_mounts_returns_none_when_no_mount_point_matches() {
        let mounts = "/dev/sda1 /srv ext4 rw,relatime 0 0\n";
        assert_eq!(
            filesystem_type_from_mounts(mounts, Path::new("/home/gabriel/.local/state/aub")),
            None
        );
    }

    // --- ensure_state_dir_ready: rejected / accepted filesystem type --------------

    #[test]
    fn ensure_state_dir_ready_refuses_and_names_a_rejected_filesystem_type() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("mounted-elsewhere");
        let mounts = FakeMountTable::new().mount(scratch.path(), "nfs");

        let err = ensure_state_dir_ready(&target, &mounts).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("nfs"), "{message}");
        assert!(message.contains("local filesystem"), "{message}");
    }

    #[test]
    fn ensure_state_dir_ready_accepts_a_recognized_local_filesystem_type() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("local-state-dir");
        let mounts = FakeMountTable::new().mount(scratch.path(), "ext4");

        assert!(ensure_state_dir_ready(&target, &mounts).is_ok());
        assert_eq!(mode_of(&target), 0o700);
    }

    // --- ensure_state_dir_ready: symlinked path -----------------------------------

    #[test]
    fn ensure_state_dir_ready_refuses_a_symlinked_state_directory() {
        let scratch = ScratchDir::new();
        let real_target = scratch.path().join("real-elsewhere");
        fs::create_dir(&real_target).unwrap();
        let link = scratch.path().join("state-dir-link");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let mounts = FakeMountTable::new();
        let err = ensure_state_dir_ready(&link, &mounts).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Store);
        assert!(err.to_string().contains("symlink"), "{}", err);
    }

    // --- ensure_state_dir_ready: unwritable (blocked parent) ----------------------

    /// Blocks the *parent* rather than the leaf: `ensure_dir_mode_0700` forces the
    /// leaf's own mode to 0700 regardless of what it finds, which is the correct,
    /// self-healing behaviour for the "permissions wider than intended" case above,
    /// but it would just as happily "heal" a leaf that was deliberately made
    /// unreadable. A parent with no execute bit cannot be traversed by this process
    /// (uid 1000, no `CAP_DAC_OVERRIDE`) regardless of who owns it, which is the
    /// genuinely unrecoverable case the acceptance criteria mean by "unwritable".
    fn blocked_parent() -> (ScratchDir, PathBuf) {
        let scratch = ScratchDir::new();
        let blocked = scratch.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let target = blocked.join("aub");
        (scratch, target)
    }

    #[test]
    fn ensure_state_dir_ready_refuses_when_the_directory_cannot_be_created() {
        let (_scratch, target) = blocked_parent();
        let mounts = FakeMountTable::new();

        let err = ensure_state_dir_ready(&target, &mounts).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Store);
    }

    // --- run_after_state_check: the network-gate ordering proof -------------------

    #[test]
    fn run_after_state_check_never_invokes_its_closure_when_the_state_dir_check_fails() {
        let (_scratch, target) = blocked_parent();
        let mounts = FakeMountTable::new();
        let called = Cell::new(false);

        let result = run_after_state_check(&target, &mounts, || {
            called.set(true);
        });

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().exit_class(),
            crate::error::ExitClass::Store
        );
        assert!(
            !called.get(),
            "the closure representing a network call ran despite the state-dir check failing"
        );
    }

    /// The permitted counterpart to the refusal above (RH-12): an identically shaped
    /// call over a state directory that passes every check must still reach the
    /// closure, so the refusal test above is proven to be about the failure, not
    /// about `run_after_state_check` never calling `then` at all.
    #[test]
    fn run_after_state_check_invokes_its_closure_when_the_state_dir_check_succeeds() {
        let scratch = ScratchDir::new();
        let target = scratch.path().join("ready-state-dir");
        let mounts = FakeMountTable::new().mount(scratch.path(), "ext4");
        let called = Cell::new(false);

        let result = run_after_state_check(&target, &mounts, || {
            called.set(true);
        });

        assert!(result.is_ok());
        assert!(called.get());
    }
}
