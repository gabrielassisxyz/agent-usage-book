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
        let path = path("expired");
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
    fn fresh_or_unrecognised_tokens_never_call_the_endpoint() {
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
        fs::write(&path, r#"{"token":{"access_token":"only"}}"#).unwrap();
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
}
