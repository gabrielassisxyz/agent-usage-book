//! `aub backup --scheduled` over its four cases (aub-7xmr): disabled by
//! the key, no destination configured, an unwritable destination, and a
//! healthy cut. Each case runs the real binary against scratch state, config
//! and destination directories, asserting the exit code, the stdout line and
//! whether an archive was written.
//!
//! The planted negative is the last test: with `backup.scheduled = false` a
//! manual `aub backup` must still write an archive, which a naive
//! implementation that gates every backup path on the key would fail.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// One test's scratch root, removed on drop. Distinct per test (process id
/// plus a counter), so parallel tests never share state, config or archives.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "aub-scheduled-backup-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("scratch root must be creatable");
        Self { root }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("aub.toml")
    }

    fn destination(&self) -> PathBuf {
        self.root.join("backups")
    }

    fn write_config(&self, body: &str) {
        std::fs::write(self.config_file(), body).expect("config must be writable");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The binary under test, pointed at this test's scratch state directory and
/// config file rather than the operator's live data.
fn aub(scratch: &Scratch) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aub"));
    command
        .env("HOME", scratch.home_dir())
        .env("AUB_STATE_DIR", scratch.state_dir())
        .env("AUB_CONFIG_FILE", scratch.config_file())
        .env("AUB_LOG_LEVEL", "off");
    command
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

/// A migrated ledger for the cases that cut a real archive: the shipped rate
/// book import is the cheapest production store user, the same one the
/// end-to-end backup case uses.
fn populate_ledger(scratch: &Scratch) {
    let book = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rate-book/rates.toml");
    let status = aub(scratch)
        .args(["rate-card", "import"])
        .arg(&book)
        .status()
        .expect("rate-card import must run");
    assert_eq!(status.code(), Some(0), "rate-card import must succeed");
}

fn archive_entries(destination: &std::path::Path) -> Vec<PathBuf> {
    let entries: Vec<PathBuf> = std::fs::read_dir(destination)
        .expect("destination must exist")
        .map(|entry| entry.expect("archive entry must be readable").path())
        .collect();
    entries
}

/// With `backup.scheduled = false` the scheduled run exits zero, names the
/// key on stdout, and writes no archive.
#[test]
fn scheduled_run_disabled_by_the_key_writes_no_archive() {
    let scratch = Scratch::new();
    let destination = scratch.destination();
    scratch.write_config(&format!(
        "[backup]\ndestination = \"{}\"\nscheduled = false\n",
        destination.display()
    ));
    let output = aub(&scratch)
        .args(["backup", "--scheduled"])
        .output()
        .expect("aub must run");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout_of(&output).contains("backup.scheduled"),
        "the disabled line must name the key: {}",
        stdout_of(&output)
    );
    assert!(
        !destination.exists(),
        "a disabled scheduled run must not create the destination"
    );
}

/// With no `backup.destination` configured the scheduled run exits zero,
/// says backups are not configured, and touches nothing.
#[test]
fn scheduled_run_without_a_destination_is_a_no_op() {
    let scratch = Scratch::new();
    scratch.write_config("");
    let output = aub(&scratch)
        .args(["backup", "--scheduled"])
        .output()
        .expect("aub must run");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout_of(&output).contains("backups are not configured"),
        "the unconfigured line must say so: {}",
        stdout_of(&output)
    );
    assert!(
        !scratch.state_dir().exists(),
        "an unconfigured scheduled run must not touch the state directory"
    );
}

/// With an unwritable destination the scheduled run fails exactly as a manual
/// run does today: exit class 5, the store class.
#[test]
fn scheduled_run_with_an_unwritable_destination_exits_five() {
    let scratch = Scratch::new();
    let blocker = scratch.root.join("blocker");
    std::fs::write(&blocker, "not a directory").expect("blocker must be writable");
    let destination = blocker.join("backups");
    scratch.write_config(&format!(
        "[backup]\ndestination = \"{}\"\n",
        destination.display()
    ));
    let scheduled = aub(&scratch)
        .args(["backup", "--scheduled"])
        .output()
        .expect("aub must run");
    assert_eq!(scheduled.status.code(), Some(5));
    let manual = aub(&scratch).arg("backup").output().expect("aub must run");
    assert_eq!(
        manual.status.code(),
        scheduled.status.code(),
        "the scheduled failure must keep the manual exit class"
    );
}

/// With a destination configured and the key left at its default, the
/// scheduled run cuts a verified archive like a manual run.
#[test]
fn scheduled_run_with_a_destination_writes_a_verified_archive() {
    let scratch = Scratch::new();
    let destination = scratch.destination();
    scratch.write_config(&format!(
        "[backup]\ndestination = \"{}\"\n",
        destination.display()
    ));
    populate_ledger(&scratch);
    let output = aub(&scratch)
        .args(["backup", "--scheduled"])
        .output()
        .expect("aub must run");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout_of(&output).contains("verified=true"),
        "the scheduled cut must verify: {}",
        stdout_of(&output)
    );
    assert!(
        !archive_entries(&destination).is_empty(),
        "the scheduled cut must leave an archive under the destination"
    );
}

/// A manual `aub backup` ignores `backup.scheduled` entirely: with the key
/// false it still writes a verified archive. A naive implementation that
/// gates the shared cut path on the key fails this test while passing the
/// disabled scheduled-run test above.
#[test]
fn manual_backup_ignores_the_scheduled_key() {
    let scratch = Scratch::new();
    let destination = scratch.destination();
    scratch.write_config(&format!(
        "[backup]\ndestination = \"{}\"\nscheduled = false\n",
        destination.display()
    ));
    populate_ledger(&scratch);
    let output = aub(&scratch).arg("backup").output().expect("aub must run");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout_of(&output).contains("verified=true"),
        "the manual cut must verify despite the key: {}",
        stdout_of(&output)
    );
    assert!(
        !archive_entries(&destination).is_empty(),
        "the manual cut must leave an archive despite the key"
    );
}
