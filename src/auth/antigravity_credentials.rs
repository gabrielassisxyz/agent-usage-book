//! Refreshing an expired Antigravity OAuth access token in place (aub-6qay).
//!
//! Antigravity stores its OAuth material as `.token.{access_token,
//! refresh_token,expiry}`. The refresh token is stable, unlike Anthropic's
//! rotating pair, but the advisory lock and post-lock re-read still prevent two
//! samplers from needlessly issuing the same exchange while another process has
//! already renewed the file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::time::UtcTimestamp;

/// How long before the stored `expiry` a token is already treated as expired.
///
/// Deliberately smaller than Anthropic's five-minute lead
/// (`credentials_lock::DEFAULT_EXPIRY_LEAD`): the observed Antigravity access
/// token lifetime is about an hour, and `aub` samples roughly every five
/// minutes, so a five-minute lead would trigger a refresh on every normal
/// sampling cadence boundary rather than only near genuine expiry. One minute
/// still comfortably covers clock skew between this machine and the provider
/// while leaving most cadence boundaries untouched. The token lifetime itself
/// is not a constant here: it is an observation that sized this number, not a
/// value the refresh path computes with.
pub const EXPIRY_LEAD: Duration = Duration::from_secs(60);
pub const CLASSIFICATION_TOKEN_REFRESHED: &str = "token_refreshed";
pub const CLASSIFICATION_REFRESH_REJECTED: &str = "refresh_rejected";
pub const CLASSIFICATION_REFRESH_CONFIGURATION_FAILED: &str = "refresh_configuration_failed";
pub const CLASSIFICATION_REFRESH_PERSIST_FAILED: &str = "refresh_persist_failed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshError {
    InvalidGrant,
    Configuration(String),
    Endpoint(String),
}

pub trait TokenEndpoint {
    fn exchange(&self, refresh_token: &str) -> Result<(String, i64), RefreshError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    NotNeeded,
    Refreshed,
    AlreadyFreshOnDisk,
    Rejected,
    ConfigurationFailed(String),
    EndpointUnreachable(String),
    PersistFailed(String),
    FileUnreadable(String),
}

impl RefreshOutcome {
    pub fn attempt_classification(&self) -> Option<&'static str> {
        match self {
            Self::Rejected => Some(CLASSIFICATION_REFRESH_REJECTED),
            Self::ConfigurationFailed(_) => Some(CLASSIFICATION_REFRESH_CONFIGURATION_FAILED),
            Self::PersistFailed(_) => Some(CLASSIFICATION_REFRESH_PERSIST_FAILED),
            Self::NotNeeded
            | Self::Refreshed
            | Self::AlreadyFreshOnDisk
            | Self::EndpointUnreachable(_)
            | Self::FileUnreadable(_) => None,
        }
    }

    pub fn stops_sampling(&self) -> bool {
        matches!(self, Self::PersistFailed(_))
    }
}

struct Credentials {
    document: serde_json::Value,
    expiry_nanos: i64,
    refresh_token: String,
}

impl Credentials {
    fn parse(raw: &str) -> Option<Self> {
        let document: serde_json::Value = serde_json::from_str(raw).ok()?;
        let token = document.get("token")?.as_object()?;
        let expiry_nanos =
            UtcTimestamp::parse_rfc3339(token.get("expiry")?.as_str()?)?.unix_nanos();
        let refresh_token = token.get("refresh_token")?.as_str()?.trim().to_owned();
        (!refresh_token.is_empty()).then_some(Self {
            document,
            expiry_nanos,
            refresh_token,
        })
    }

    fn expired(&self, now_nanos: i64) -> bool {
        now_nanos.saturating_add(EXPIRY_LEAD.as_nanos().try_into().unwrap_or(i64::MAX))
            >= self.expiry_nanos
    }

    fn refreshed(
        &self,
        access_token: String,
        expires_in_secs: i64,
        now_nanos: i64,
    ) -> serde_json::Value {
        let mut document = self.document.clone();
        let expiry_nanos = now_nanos.saturating_add(expires_in_secs.saturating_mul(1_000_000_000));
        if let Some(token) = document
            .get_mut("token")
            .and_then(serde_json::Value::as_object_mut)
        {
            token.insert(
                "access_token".into(),
                serde_json::Value::String(access_token),
            );
            token.insert(
                "expiry".into(),
                serde_json::Value::String(rfc3339_utc_seconds(expiry_nanos)),
            );
        }
        document
    }
}

/// Refreshes only a recognisable, expired Antigravity credential. A 401 never
/// reaches this function: the stored expiry field is the sole trigger.
pub fn refresh_if_expired(
    path: &Path,
    now_nanos: i64,
    endpoint: &dyn TokenEndpoint,
) -> RefreshOutcome {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => return RefreshOutcome::FileUnreadable(error.to_string()),
    };
    let Some(credentials) = Credentials::parse(&raw) else {
        return RefreshOutcome::NotNeeded;
    };
    if !credentials.expired(now_nanos) {
        return RefreshOutcome::NotNeeded;
    }
    let Some(parent) = path.parent() else {
        return RefreshOutcome::PersistFailed("credential path has no parent directory".into());
    };
    let lock = match acquire_lock(&sibling(path, ".lock")) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return RefreshOutcome::EndpointUnreachable("credential lock stayed busy".into());
        }
        Err(error) => return RefreshOutcome::PersistFailed(format!("lock: {error}")),
    };
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => return RefreshOutcome::FileUnreadable(error.to_string()),
    };
    let Some(credentials) = Credentials::parse(&raw) else {
        return RefreshOutcome::NotNeeded;
    };
    if !credentials.expired(now_nanos) {
        return RefreshOutcome::AlreadyFreshOnDisk;
    }
    let (access_token, expires_in_secs) = match endpoint.exchange(&credentials.refresh_token) {
        Ok(tokens) => tokens,
        Err(RefreshError::InvalidGrant) => return RefreshOutcome::Rejected,
        Err(RefreshError::Configuration(error)) => {
            return RefreshOutcome::ConfigurationFailed(error);
        }
        Err(RefreshError::Endpoint(error)) => return RefreshOutcome::EndpointUnreachable(error),
    };
    let document = credentials.refreshed(access_token, expires_in_secs, now_nanos);
    if let Err(error) = persist(path, parent, &raw, &document) {
        return RefreshOutcome::PersistFailed(error.to_string());
    }
    drop(lock);
    RefreshOutcome::Refreshed
}

fn persist(
    path: &Path,
    parent: &Path,
    previous: &str,
    document: &serde_json::Value,
) -> io::Result<()> {
    write_private(&sibling(path, ".bak"), previous.as_bytes())?;
    let temp = parent.join(format!(".antigravity-oauth.tmp.{}", std::process::id()));
    write_private(
        &temp,
        &serde_json::to_vec(document).map_err(io::Error::other)?,
    )?;
    fs::rename(temp, path)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(unix)]
fn acquire_lock(path: &Path) -> io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    for _ in 0..40 {
        let result = unsafe { flock(file.as_raw_fd(), 2 | 4) };
        if result == 0 {
            return Ok(Some(file));
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
            return Err(io::Error::last_os_error());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

#[cfg(not(unix))]
fn acquire_lock(_path: &Path) -> io::Result<Option<File>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "refresh locking requires unix",
    ))
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
}

fn rfc3339_utc_seconds(nanos: i64) -> String {
    let seconds = nanos.div_euclid(1_000_000_000);
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3_600,
        (day_seconds % 3_600) / 60,
        day_seconds % 60,
    )
}

// Howard Hinnant's public-domain civil-date conversion, with 1970-01-01 as
// day zero. Keeping formatting here avoids introducing a time dependency just
// to write the provider's RFC3339 credential field.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * doy + 2) / 153;
    let day = doy - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Endpoint {
        calls: Cell<usize>,
        reply: Result<(String, i64), RefreshError>,
    }
    impl TokenEndpoint for Endpoint {
        fn exchange(&self, _refresh: &str) -> Result<(String, i64), RefreshError> {
            self.calls.set(self.calls.get() + 1);
            self.reply.clone()
        }
    }

    fn path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aub-agy-refresh-{tag}-{}", std::process::id()))
    }
    fn credential(expiry: &str) -> String {
        format!(
            r#"{{"token":{{"access_token":"old","refresh_token":"never-render-this","expiry":"{expiry}"}},"auth_method":"consumer"}}"#
        )
    }
    const NOW: i64 = 1_800_000_000_000_000_000;

    #[test]
    fn expired_token_refreshes_from_response_lifetime_and_keeps_a_private_backup() {
        // A private scratch directory, not the shared flat helper: the
        // write-back temp file name embeds only the process id (matching
        // `credentials_lock`'s own scheme), so two tests writing concurrently
        // into the *same* directory can race each other's rename.
        let scratch = Scratch::new("expired");
        let path = scratch.credentials();
        let original = credential("2020-01-01T00:00:00Z");
        fs::write(&path, &original).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("new-access".into(), 3599)),
        };
        assert_eq!(
            refresh_if_expired(&path, NOW, &endpoint),
            RefreshOutcome::Refreshed
        );
        assert_eq!(endpoint.calls.get(), 1);
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("new-access"));
        assert!(written.contains("2027-01-15T08:59:59Z"));
        assert_eq!(
            fs::read_to_string(sibling(&path, ".bak")).unwrap(),
            original
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_not_yet_expired_token_is_a_no_op() {
        let path = path("fresh");
        fs::write(&path, credential("2030-01-01T00:00:00Z")).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("unused".into(), 3600)),
        };
        assert_eq!(
            refresh_if_expired(&path, NOW, &endpoint),
            RefreshOutcome::NotNeeded
        );
        assert_eq!(endpoint.calls.get(), 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn an_unrecognised_shape_is_a_no_op() {
        let path = path("unrecognised");
        fs::write(&path, r#"{"token":{"access_token":"only"}}"#).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("unused".into(), 3600)),
        };
        assert_eq!(
            refresh_if_expired(&path, NOW, &endpoint),
            RefreshOutcome::NotNeeded
        );
        assert_eq!(endpoint.calls.get(), 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_token_expiring_exactly_at_the_lead_is_expired() {
        // now + EXPIRY_LEAD >= expiry is the boundary the production check
        // uses (`>=`), so a token expiring exactly at the lead must refresh.
        let expiry_nanos = NOW + i64::try_from(EXPIRY_LEAD.as_nanos()).unwrap();
        let scratch = Scratch::new("lead-boundary-expired");
        let path = scratch.credentials();
        fs::write(&path, credential(&rfc3339_utc_seconds(expiry_nanos))).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("new-access".into(), 60)),
        };
        assert_eq!(
            refresh_if_expired(&path, NOW, &endpoint),
            RefreshOutcome::Refreshed
        );
        assert_eq!(endpoint.calls.get(), 1);
    }

    #[test]
    fn a_token_expiring_one_second_beyond_the_lead_is_not_yet_expired() {
        let expiry_nanos = NOW + i64::try_from(EXPIRY_LEAD.as_nanos()).unwrap() + 1_000_000_000;
        let path = path("lead-boundary-fresh");
        fs::write(&path, credential(&rfc3339_utc_seconds(expiry_nanos))).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("unused".into(), 60)),
        };
        assert_eq!(
            refresh_if_expired(&path, NOW, &endpoint),
            RefreshOutcome::NotNeeded
        );
        assert_eq!(endpoint.calls.get(), 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_grant_keeps_the_credential_byte_identical_and_is_classified() {
        let path = path("rejected");
        let original = credential("2020-01-01T00:00:00Z");
        fs::write(&path, &original).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Err(RefreshError::InvalidGrant),
        };
        let outcome = refresh_if_expired(&path, NOW, &endpoint);
        assert_eq!(outcome, RefreshOutcome::Rejected);
        assert_eq!(
            outcome.attempt_classification(),
            Some(CLASSIFICATION_REFRESH_REJECTED)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!format!("{outcome:?}").contains("never-render-this"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_rejected_client_pairing_surfaces_as_configuration_failed_not_an_ordinary_failure() {
        // The endpoint distinguishes a rejected client id/secret pair from a
        // rejected refresh token: the former is a configuration fault (re-extract
        // the credentials), the latter an auth failure (log in again). A pairing
        // rejection must not fold into `Rejected` or `EndpointUnreachable`.
        let path = path("config-failed");
        let original = credential("2020-01-01T00:00:00Z");
        fs::write(&path, &original).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Err(RefreshError::Configuration(
                "the OAuth client credentials extracted from the agy binary were rejected".into(),
            )),
        };
        let outcome = refresh_if_expired(&path, NOW, &endpoint);
        assert!(
            matches!(outcome, RefreshOutcome::ConfigurationFailed(_)),
            "{outcome:?}"
        );
        assert_eq!(
            outcome.attempt_classification(),
            Some(CLASSIFICATION_REFRESH_CONFIGURATION_FAILED)
        );
        assert_ne!(outcome, RefreshOutcome::Rejected);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn the_refresh_token_is_never_rendered_by_the_outcome() {
        let scratch = Scratch::new("no-leak");
        let path = scratch.credentials();
        fs::write(&path, credential("2020-01-01T00:00:00Z")).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("new-access".into(), 3600)),
        };
        let outcome = refresh_if_expired(&path, NOW, &endpoint);
        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert!(!format!("{outcome:?}").contains("never-render-this"));
    }

    #[test]
    fn a_missing_file_is_left_for_resolve_to_surface() {
        let path = path("missing");
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("unused".into(), 3600)),
        };
        let outcome = refresh_if_expired(&path, NOW, &endpoint);
        assert!(matches!(outcome, RefreshOutcome::FileUnreadable(_)));
        assert_eq!(outcome.attempt_classification(), None);
        assert_eq!(endpoint.calls.get(), 0);
    }

    /// A scratch directory that removes itself on drop, needed wherever a test
    /// puts a lock file beside the credential or chmods the parent: both would
    /// disturb every other test's file if done in the shared flat temp dir the
    /// tests above use.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aub-agy-refresh-dir-{tag}-{}-{}",
                std::process::id(),
                next_id()
            ));
            fs::create_dir_all(&dir).expect("scratch dir");
            Self { dir }
        }

        fn credentials(&self) -> PathBuf {
            self.dir.join(".token")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn next_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    #[cfg(unix)]
    fn a_file_rewritten_by_a_third_party_between_read_and_lock_triggers_no_second_refresh() {
        use std::os::unix::io::AsRawFd;

        let scratch = Scratch::new("post-lock-reread");
        let path = scratch.credentials();
        let lock_path = sibling(&path, ".lock");
        fs::write(&path, credential("2020-01-01T00:00:00Z")).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("should-not-be-used".into(), 3600)),
        };

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let path_for_thread = path.clone();
        let rotator = std::thread::spawn(move || {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .unwrap();
            // SAFETY: fd is valid for the duration of the call.
            let rc = unsafe { flock(file.as_raw_fd(), 2) }; // LOCK_EX
            assert_eq!(rc, 0);
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(40));
            // Another refresher rotated the file to a fresh pair while we
            // waited on the lock.
            fs::write(&path_for_thread, credential("2030-01-01T00:00:00Z")).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            drop(file);
        });

        locked_rx.recv().unwrap();
        let outcome = refresh_if_expired(&path, NOW, &endpoint);

        assert_eq!(outcome, RefreshOutcome::AlreadyFreshOnDisk);
        assert_eq!(endpoint.calls.get(), 0);
        rotator.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_held_lock_times_out_without_a_refresh() {
        use std::os::unix::io::AsRawFd;

        let scratch = Scratch::new("lock-busy");
        let path = scratch.credentials();
        let lock_path = sibling(&path, ".lock");
        fs::write(&path, credential("2020-01-01T00:00:00Z")).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("unused".into(), 3600)),
        };

        let holder = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        // SAFETY: fd is valid for the duration of the call.
        let rc = unsafe { flock(holder.as_raw_fd(), 2) }; // LOCK_EX
        assert_eq!(rc, 0);

        let outcome = refresh_if_expired(&path, NOW, &endpoint);

        assert_eq!(
            outcome,
            RefreshOutcome::EndpointUnreachable("credential lock stayed busy".into())
        );
        assert_eq!(outcome.attempt_classification(), None);
        assert_eq!(endpoint.calls.get(), 0);
        drop(holder);
    }

    #[test]
    #[cfg(unix)]
    fn a_persist_failure_after_a_successful_exchange_stops_sampling() {
        // The parent directory is read-only, so the write-back fails after the
        // endpoint has already spent the refresh token.
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("persist-failed");
        let path = scratch.credentials();
        fs::write(&path, credential("2020-01-01T00:00:00Z")).unwrap();
        let endpoint = Endpoint {
            calls: Cell::new(0),
            reply: Ok(("new-access".into(), 3600)),
        };

        fs::set_permissions(&scratch.dir, fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = refresh_if_expired(&path, NOW, &endpoint);
        fs::set_permissions(&scratch.dir, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            matches!(outcome, RefreshOutcome::PersistFailed(_)),
            "{outcome:?}"
        );
        assert!(outcome.stops_sampling());
        assert_eq!(
            outcome.attempt_classification(),
            Some(CLASSIFICATION_REFRESH_PERSIST_FAILED)
        );
    }
}
