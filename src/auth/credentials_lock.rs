//! Refreshing an expired Anthropic OAuth access token in place, under the
//! profile's `.credentials.lock` (aub-79gp).
//!
//! Claude Code stores its subscription OAuth pair in
//! `<profile>/.credentials.json` and renews it only when that profile is next
//! used interactively. `aub` samples the Anthropic usage endpoint with the same
//! pair, so an access token that expired since Claude Code last ran costs a run
//! of `auth_required` attempts until a human opens Claude Code. This module
//! renews it: when the stored token is past `expiresAt` (or within a short
//! lead), it takes `<profile>/.credentials.lock` with `flock` (the same lock
//! Claude Code coordinates its own refreshes through), re-reads the file under
//! the lock, POSTs the rotating refresh token to the OAuth token endpoint, and
//! writes the rotated pair back with a temp-then-rename in the same directory.
//!
//! The refresh token is single-use and rotating (anthropics/claude-code issue
//! 27933): two processes refreshing from one file race, and the loser's next
//! refresh is rejected and that Claude Code profile is logged out. The lock, and
//! the re-read after acquiring it, are what make that safe. A 429 never reaches
//! this module: the only trigger is the stored token's own expiry, never a
//! response, so a rate limit cannot spend a single-use refresh token to buy a
//! fresh bucket.
//!
//! This module reads no system clock of its own (boundary rule 18): the caller
//! passes the wall-clock instant from its injected `Clock`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Claude Code OAuth client id: the value Claude Code and every out-of-band
/// refresher on this machine present (`ai-usagebar`, onWatch). Defined once here
/// and read by the production endpoint implementation, never copied
/// (correctness invariant 3).
pub const CLAUDE_CODE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// The default OAuth token endpoint. `console.anthropic.com` is what onWatch
/// uses; `platform.claude.com/v1/oauth/token` is the documented alternative to
/// switch to if this one stops answering.
pub const DEFAULT_TOKEN_ENDPOINT: &str = "https://console.anthropic.com/v1/oauth/token";

/// How long before the stored `expiresAt` a token is already treated as
/// expired, covering clock skew between this machine and the provider. Five
/// minutes matches onWatch's one-hour lead scaled to how often `aub` samples.
pub const DEFAULT_EXPIRY_LEAD: Duration = Duration::from_secs(5 * 60);

/// The `sanitized_error_classification` an attempt records when a refresh
/// rotated the token before it sampled.
pub const CLASSIFICATION_TOKEN_REFRESHED: &str = "token_refreshed";
/// The classification when `.credentials.lock` stayed held for the whole
/// bounded wait and no refresh was attempted.
pub const CLASSIFICATION_LOCK_BUSY: &str = "credential_lock_busy";
/// The classification when the token endpoint rejected the stored refresh
/// token (`invalid_grant`). The credential file is left exactly as found.
pub const CLASSIFICATION_REFRESH_REJECTED: &str = "refresh_rejected";
/// The classification when the endpoint rotated the token but the new pair
/// could not be persisted. The on-disk pair is now dead.
pub const CLASSIFICATION_REFRESH_PERSIST_FAILED: &str = "refresh_persist_failed";

/// The rotated OAuth pair a successful refresh returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotatedTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until the new access token expires, as the endpoint reported it.
    pub expires_in_secs: i64,
}

/// Why a call to the OAuth token endpoint did not yield a rotated pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshEndpointError {
    /// `error = "invalid_grant"`: the stored refresh token is already spent,
    /// usually because Claude Code rotated it underneath us. Never retried.
    InvalidGrant,
    /// Any other HTTP status from the token endpoint.
    HttpStatus(u16),
    /// The request never completed (DNS, connect, timeout).
    Transport(String),
    /// The endpoint answered 200 with a body missing a required field.
    MalformedResponse,
}

/// The OAuth token endpoint, injected so the lock-and-write logic is testable
/// without a network. The one production implementation lives in `crate::cli`,
/// which owns the transport wiring (boundary rule 12 keeps `ureq` out of here).
pub trait OAuthRefreshEndpoint {
    /// Exchanges a refresh token for a rotated pair.
    fn exchange(&self, refresh_token: &str) -> Result<RotatedTokens, RefreshEndpointError>;
}

/// The bounded wait for `.credentials.lock`. A crashed refresher must not be
/// able to wedge sampling, so the wait is short and a timeout is an outcome,
/// not an error. Expressed as an attempt count and a sleep rather than a
/// deadline so this module reads no clock (boundary rule 18); the default is
/// about two seconds.
#[derive(Debug, Clone, Copy)]
pub struct LockWait {
    pub attempts: u32,
    pub sleep: Duration,
}

impl Default for LockWait {
    fn default() -> Self {
        Self {
            attempts: 40,
            sleep: Duration::from_millis(50),
        }
    }
}

/// What one refresh attempt did. Every arm is a fact the caller records rather
/// than an error it propagates: a rejected refresh and a busy lock both end as
/// `auth_required` on the sampling attempt, with the arm naming why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The stored token is not a Claude Code OAuth pair, or it is not close to
    /// expiry, or it carries no refresh token: nothing to do, no lock taken.
    NotNeeded,
    /// The token was expired, the lock was taken, and the rotated pair is now
    /// on disk. Sampling reads it fresh on the next line.
    Refreshed,
    /// The token was expired but the file already held a fresh pair once the
    /// lock was acquired: another refresher won the race. Nothing was written
    /// and no token-endpoint request was sent.
    AlreadyFreshOnDisk,
    /// The lock was held by another process for the whole bounded wait.
    LockBusy,
    /// The token endpoint rejected the stored refresh token (`invalid_grant`).
    /// The file is untouched.
    Rejected,
    /// The token endpoint could not be reached or answered with a status other
    /// than success or `invalid_grant`. The file is untouched; the sampling
    /// attempt surfaces its own `auth_required` as before.
    EndpointUnreachable(String),
    /// The refresh succeeded at the endpoint but the rotated pair could not be
    /// written back. The on-disk pair is now dead.
    PersistFailed(String),
    /// The credential file could not be read. Left for `auth::resolve` to
    /// surface as it already does.
    FileUnreadable(String),
}

impl RefreshOutcome {
    /// The `sanitized_error_classification` this outcome contributes to the
    /// sampling attempt, or `None` when the attempt's own outcome classifies
    /// it. `EndpointUnreachable`, `NotNeeded`, `AlreadyFreshOnDisk` and
    /// `FileUnreadable` add nothing: the attempt that follows records its own
    /// reason.
    pub fn attempt_classification(&self) -> Option<&'static str> {
        match self {
            RefreshOutcome::Refreshed => Some(CLASSIFICATION_TOKEN_REFRESHED),
            RefreshOutcome::LockBusy => Some(CLASSIFICATION_LOCK_BUSY),
            RefreshOutcome::Rejected => Some(CLASSIFICATION_REFRESH_REJECTED),
            RefreshOutcome::PersistFailed(_) => Some(CLASSIFICATION_REFRESH_PERSIST_FAILED),
            RefreshOutcome::NotNeeded
            | RefreshOutcome::AlreadyFreshOnDisk
            | RefreshOutcome::EndpointUnreachable(_)
            | RefreshOutcome::FileUnreadable(_) => None,
        }
    }

    /// Whether this outcome means the account must not be sampled this run: the
    /// on-disk pair is dead and an operator has to re-authenticate the profile.
    pub fn stops_sampling(&self) -> bool {
        matches!(self, RefreshOutcome::PersistFailed(_))
    }
}

/// The `claudeAiOauth` view of a Claude Code credential file: the parsed
/// document kept whole so a write-back preserves every field, plus the three
/// values the refresh reasons about.
struct ClaudeCredentials {
    document: serde_json::Value,
    expires_at_ms: i64,
    refresh_token: String,
}

impl ClaudeCredentials {
    /// Parses `raw`, or returns `None` when it is not a Claude Code OAuth pair
    /// with an `expiresAt` and a `refreshToken` (an `env`-kind credential, a
    /// bare token string, or a shape this module does not refresh).
    fn parse(raw: &str) -> Option<Self> {
        let document: serde_json::Value = serde_json::from_str(raw).ok()?;
        let oauth = document.get("claudeAiOauth")?.as_object()?;
        let expires_at_ms = oauth.get("expiresAt")?.as_i64()?;
        let refresh_token = oauth.get("refreshToken")?.as_str()?.trim().to_string();
        if refresh_token.is_empty() {
            return None;
        }
        Some(Self {
            document,
            expires_at_ms,
            refresh_token,
        })
    }

    /// Whether the token is expired or within `lead` of expiry at `now_ms`.
    fn is_expired(&self, now_ms: i64, lead: Duration) -> bool {
        let lead_ms = i64::try_from(lead.as_millis()).unwrap_or(i64::MAX);
        now_ms.saturating_add(lead_ms) >= self.expires_at_ms
    }

    /// The document with the rotated pair written into `claudeAiOauth`, every
    /// other field untouched. `now_ms` plus the endpoint's `expires_in`
    /// becomes the new `expiresAt` (Unix milliseconds).
    fn with_rotated(&self, rotated: &RotatedTokens, now_ms: i64) -> serde_json::Value {
        let mut document = self.document.clone();
        if let Some(oauth) = document
            .get_mut("claudeAiOauth")
            .and_then(serde_json::Value::as_object_mut)
        {
            oauth.insert(
                "accessToken".to_string(),
                serde_json::Value::String(rotated.access_token.clone()),
            );
            oauth.insert(
                "refreshToken".to_string(),
                serde_json::Value::String(rotated.refresh_token.clone()),
            );
            let new_expiry = now_ms.saturating_add(rotated.expires_in_secs.saturating_mul(1000));
            oauth.insert(
                "expiresAt".to_string(),
                serde_json::Value::Number(new_expiry.into()),
            );
        }
        document
    }
}

/// Refreshes the access token in `credentials_path` when it is expired.
///
/// `now_unix_millis` is the wall clock from the caller's injected `Clock`.
pub fn refresh_if_expired(
    credentials_path: &Path,
    now_unix_millis: i64,
    expiry_lead: Duration,
    endpoint: &dyn OAuthRefreshEndpoint,
    lock_wait: LockWait,
) -> RefreshOutcome {
    let raw = match fs::read_to_string(credentials_path) {
        Ok(raw) => raw,
        Err(cause) => return RefreshOutcome::FileUnreadable(cause.to_string()),
    };
    let Some(parsed) = ClaudeCredentials::parse(&raw) else {
        return RefreshOutcome::NotNeeded;
    };
    if !parsed.is_expired(now_unix_millis, expiry_lead) {
        return RefreshOutcome::NotNeeded;
    }

    let Some(parent) = credentials_path.parent() else {
        return RefreshOutcome::PersistFailed(
            "credential path has no parent directory".to_string(),
        );
    };
    let lock_path = parent.join(".credentials.lock");
    let lock = match acquire_lock(&lock_path, lock_wait) {
        Ok(Some(lock)) => lock,
        Ok(None) => return RefreshOutcome::LockBusy,
        Err(cause) => return RefreshOutcome::PersistFailed(format!("lock: {cause}")),
    };

    // Re-read under the lock: another refresher may have rotated the file while
    // we waited, in which case its token is the one to use and we refresh
    // nothing.
    let raw_locked = match fs::read_to_string(credentials_path) {
        Ok(raw) => raw,
        Err(cause) => return RefreshOutcome::FileUnreadable(cause.to_string()),
    };
    let Some(parsed_locked) = ClaudeCredentials::parse(&raw_locked) else {
        return RefreshOutcome::NotNeeded;
    };
    if !parsed_locked.is_expired(now_unix_millis, expiry_lead) {
        return RefreshOutcome::AlreadyFreshOnDisk;
    }

    let rotated = match endpoint.exchange(&parsed_locked.refresh_token) {
        Ok(rotated) => rotated,
        Err(RefreshEndpointError::InvalidGrant) => return RefreshOutcome::Rejected,
        Err(RefreshEndpointError::HttpStatus(status)) => {
            return RefreshOutcome::EndpointUnreachable(format!("token endpoint status {status}"));
        }
        Err(RefreshEndpointError::Transport(detail)) => {
            return RefreshOutcome::EndpointUnreachable(detail);
        }
        Err(RefreshEndpointError::MalformedResponse) => {
            return RefreshOutcome::EndpointUnreachable(
                "token endpoint response missing a required field".to_string(),
            );
        }
    };

    // The refresh token is spent now: any failure past this point leaves the
    // on-disk pair dead, so every branch below is PersistFailed rather than a
    // recoverable state.
    let document = parsed_locked.with_rotated(&rotated, now_unix_millis);
    if let Err(cause) = persist(credentials_path, parent, &raw_locked, &document) {
        return RefreshOutcome::PersistFailed(cause.to_string());
    }

    drop(lock);
    RefreshOutcome::Refreshed
}

/// Writes `document` over `path` atomically: the previous bytes to `<path>.bak`
/// first (the recovery path reads it), then a temp file in the same directory,
/// fsynced, renamed into place. The temp and the target share a directory so
/// the rename is atomic on POSIX.
fn persist(
    path: &Path,
    parent: &Path,
    previous_raw: &str,
    document: &serde_json::Value,
) -> io::Result<()> {
    let serialized = serde_json::to_vec(document)
        .map_err(|cause| io::Error::new(io::ErrorKind::InvalidData, cause))?;

    let backup_path = sibling_with_suffix(path, ".bak");
    write_private(&backup_path, previous_raw.as_bytes())?;

    let temp_path = parent.join(format!(".credentials.json.tmp.{}", std::process::id()));
    write_private(&temp_path, &serialized)?;
    fs::rename(&temp_path, path)
}

/// Creates or truncates `path` with mode 0600, writes `bytes`, and fsyncs.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// `<path>` with `suffix` appended to its file name (`.credentials.json` ->
/// `.credentials.json.bak`), not replacing the extension.
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Opens (creating if absent, mode 0600) `lock_path` and takes an exclusive
/// advisory `flock`, retrying `LOCK_NB` up to `lock_wait.attempts` times with a
/// sleep between. `Ok(None)` means the lock stayed held for the whole wait.
#[cfg(unix)]
fn acquire_lock(lock_path: &Path, lock_wait: LockWait) -> io::Result<Option<LockGuard>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    let fd = file.as_raw_fd();

    let mut attempt = 0;
    loop {
        // SAFETY: `fd` is a valid open file descriptor owned by `file` for the
        // duration of this call; `flock` reads only the descriptor and the
        // operation flags.
        let rc = unsafe { sys::flock(fd, sys::LOCK_EX | sys::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(LockGuard { file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err);
        }
        attempt += 1;
        if attempt >= lock_wait.attempts {
            return Ok(None);
        }
        std::thread::sleep(lock_wait.sleep);
    }
}

/// Holds the `flock` for its lifetime; the lock releases when the file is
/// closed on drop.
#[cfg(unix)]
struct LockGuard {
    #[allow(dead_code)] // held for its Drop; the field is the lock
    file: File,
}

#[cfg(not(unix))]
fn acquire_lock(_lock_path: &Path, _lock_wait: LockWait) -> io::Result<Option<()>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "credential lock refresh is only implemented on unix",
    ))
}

/// The one C entry point the credential lock needs, declared here because the
/// repository takes explicit dependencies only and `libc` is not one of them
/// (the same pattern as `presentation::style`). `LOCK_SH`/`LOCK_EX`/`LOCK_NB`/
/// `LOCK_UN` have carried the values 1/2/4/8 across every unix since 4.2BSD.
#[cfg(unix)]
mod sys {
    use std::ffi::c_int;

    unsafe extern "C" {
        pub fn flock(fd: c_int, operation: c_int) -> c_int;
    }

    pub const LOCK_EX: c_int = 2;
    pub const LOCK_NB: c_int = 4;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::mpsc;

    /// A scratch directory that removes itself on drop.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-credlock-{}-{}-{tag}",
                std::process::id(),
                next_id()
            ));
            fs::create_dir_all(&path).expect("scratch dir");
            Self { path }
        }

        fn credentials(&self) -> PathBuf {
            self.path.join(".credentials.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn next_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// An endpoint that records every call and answers from a fixed script.
    struct SpyEndpoint {
        calls: RefCell<Vec<String>>,
        response: Result<RotatedTokens, RefreshEndpointError>,
    }

    impl SpyEndpoint {
        fn ok(access: &str, refresh: &str, expires_in_secs: i64) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                response: Ok(RotatedTokens {
                    access_token: access.to_string(),
                    refresh_token: refresh.to_string(),
                    expires_in_secs,
                }),
            }
        }

        fn err(error: RefreshEndpointError) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                response: Err(error),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl OAuthRefreshEndpoint for SpyEndpoint {
        fn exchange(&self, refresh_token: &str) -> Result<RotatedTokens, RefreshEndpointError> {
            self.calls.borrow_mut().push(refresh_token.to_string());
            self.response.clone()
        }
    }

    fn credential_json(access: &str, refresh: &str, expires_at_ms: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":{expires_at_ms},"scopes":["user:inference"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}}}"#
        )
    }

    const NOW_MS: i64 = 1_760_000_000_000;
    const HOUR_MS: i64 = 3_600_000;
    const NO_LEAD: Duration = Duration::from_secs(0);

    fn fast_wait() -> LockWait {
        LockWait {
            attempts: 200,
            sleep: Duration::from_millis(2),
        }
    }

    // --- criterion 1: an expired token is refreshed once and written back -----

    #[test]
    fn expired_token_is_refreshed_and_written_back_preserving_other_fields() {
        let scratch = Scratch::new("refresh-writeback");
        let path = scratch.credentials();
        fs::write(
            &path,
            credential_json("old-access", "old-refresh", NOW_MS - HOUR_MS),
        )
        .unwrap();
        let endpoint = SpyEndpoint::ok("new-access", "new-refresh", 3600);

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert_eq!(endpoint.call_count(), 1);
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let oauth = &written["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "new-access");
        assert_eq!(oauth["refreshToken"], "new-refresh");
        assert_eq!(oauth["expiresAt"], NOW_MS + 3600 * 1000);
        // Every other field the file held is unchanged.
        assert_eq!(oauth["scopes"][0], "user:inference");
        assert_eq!(oauth["subscriptionType"], "max");
        assert_eq!(oauth["rateLimitTier"], "default_claude_max_20x");
    }

    #[test]
    fn refresh_leaves_a_bak_with_the_previous_pair() {
        let scratch = Scratch::new("refresh-bak");
        let path = scratch.credentials();
        let original = credential_json("old-access", "old-refresh", NOW_MS - HOUR_MS);
        fs::write(&path, &original).unwrap();
        let endpoint = SpyEndpoint::ok("new-access", "new-refresh", 3600);

        refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        let bak = sibling_with_suffix(&path, ".bak");
        assert_eq!(fs::read_to_string(&bak).unwrap(), original);
    }

    // --- criterion 2: the post-lock re-read detects an already-rotated file ---

    #[test]
    #[cfg(unix)]
    fn a_file_rotated_while_we_waited_for_the_lock_triggers_no_second_refresh() {
        let scratch = Scratch::new("post-lock-reread");
        let path = scratch.credentials();
        let lock_path = scratch.path.join(".credentials.lock");
        fs::write(
            &path,
            credential_json("old-access", "old-refresh", NOW_MS - HOUR_MS),
        )
        .unwrap();
        let endpoint = SpyEndpoint::ok("should-not-be-used", "nope", 3600);

        let (locked_tx, locked_rx) = mpsc::channel();
        let path_for_thread = path.clone();
        let rotator = std::thread::spawn(move || {
            use std::os::unix::io::AsRawFd;
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .unwrap();
            // SAFETY: fd is valid for the duration of the call.
            let rc = unsafe { sys::flock(file.as_raw_fd(), sys::LOCK_EX) };
            assert_eq!(rc, 0);
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(40));
            // Another refresher rotated the file to a fresh pair.
            fs::write(
                &path_for_thread,
                credential_json("rotated-by-peer", "peer-refresh", NOW_MS + HOUR_MS),
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(5));
            drop(file);
        });

        // The lock is held and the file is still expired: our pre-lock read
        // sees the expired token, then we block on the lock.
        locked_rx.recv().unwrap();
        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::AlreadyFreshOnDisk);
        assert_eq!(endpoint.call_count(), 0);
        rotator.join().unwrap();
    }

    // --- criterion 3: expiry is the only trigger; a fresh token never asks ---

    #[test]
    fn a_fresh_token_is_never_refreshed_whatever_the_provider_would_answer() {
        let scratch = Scratch::new("fresh-noop");
        let path = scratch.credentials();
        fs::write(
            &path,
            credential_json("live-access", "live-refresh", NOW_MS + HOUR_MS),
        )
        .unwrap();
        let endpoint = SpyEndpoint::ok("unused", "unused", 3600);

        let outcome =
            refresh_if_expired(&path, NOW_MS, DEFAULT_EXPIRY_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::NotNeeded);
        assert_eq!(
            endpoint.call_count(),
            0,
            "a rate limit or any other response can never reach the token endpoint: \
             the only trigger is the stored token's own expiry"
        );
    }

    // --- criterion 4: invalid_grant classifies as refresh_rejected, file kept -

    #[test]
    fn invalid_grant_is_refresh_rejected_and_leaves_the_file_untouched() {
        let scratch = Scratch::new("invalid-grant");
        let path = scratch.credentials();
        let original = credential_json("old-access", "spent-refresh", NOW_MS - HOUR_MS);
        fs::write(&path, &original).unwrap();
        let endpoint = SpyEndpoint::err(RefreshEndpointError::InvalidGrant);

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::Rejected);
        assert_eq!(
            outcome.attempt_classification(),
            Some(CLASSIFICATION_REFRESH_REJECTED)
        );
        assert_eq!(endpoint.call_count(), 1);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "the credential file must be byte-identical after a rejected refresh"
        );
        assert!(
            !sibling_with_suffix(&path, ".bak").exists(),
            "a rejected refresh writes no .bak"
        );
    }

    // --- criterion 5: the refresh token never leaves this module -------------

    #[test]
    fn the_refresh_token_is_never_rendered_by_the_outcome() {
        let scratch = Scratch::new("no-leak");
        let path = scratch.credentials();
        fs::write(
            &path,
            credential_json("old-access", "secret-refresh-abc123", NOW_MS - HOUR_MS),
        )
        .unwrap();
        let endpoint = SpyEndpoint::ok("new-access", "new-secret-refresh", 3600);

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert!(!format!("{outcome:?}").contains("secret-refresh"));
        assert!(!format!("{outcome:?}").contains("new-secret-refresh"));
    }

    // --- the shapes that do nothing ----------------------------------------

    #[test]
    fn a_bare_token_file_is_not_refreshed() {
        let scratch = Scratch::new("bare-token");
        let path = scratch.credentials();
        fs::write(&path, "sk-ant-oat01-bare").unwrap();
        let endpoint = SpyEndpoint::ok("unused", "unused", 3600);

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::NotNeeded);
        assert_eq!(endpoint.call_count(), 0);
    }

    #[test]
    fn an_env_shaped_json_without_oauth_is_not_refreshed() {
        let scratch = Scratch::new("no-oauth");
        let path = scratch.credentials();
        fs::write(&path, r#"{"accessToken":"just-a-token"}"#).unwrap();
        let endpoint = SpyEndpoint::ok("unused", "unused", 3600);

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert_eq!(outcome, RefreshOutcome::NotNeeded);
        assert_eq!(endpoint.call_count(), 0);
    }

    #[test]
    fn a_missing_file_is_left_for_resolve_to_surface() {
        let scratch = Scratch::new("missing");
        let path = scratch.credentials();
        let endpoint = SpyEndpoint::ok("unused", "unused", 3600);

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        assert!(matches!(outcome, RefreshOutcome::FileUnreadable(_)));
        assert_eq!(outcome.attempt_classification(), None);
        assert_eq!(endpoint.call_count(), 0);
    }

    #[test]
    fn a_held_lock_times_out_as_lock_busy_without_a_refresh() {
        let scratch = Scratch::new("lock-busy");
        let path = scratch.credentials();
        let lock_path = scratch.path.join(".credentials.lock");
        fs::write(
            &path,
            credential_json("old-access", "old-refresh", NOW_MS - HOUR_MS),
        )
        .unwrap();
        let endpoint = SpyEndpoint::ok("unused", "unused", 3600);

        use std::os::unix::io::AsRawFd;
        let holder = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let rc = unsafe { sys::flock(holder.as_raw_fd(), sys::LOCK_EX) };
        assert_eq!(rc, 0);

        let outcome = refresh_if_expired(
            &path,
            NOW_MS,
            NO_LEAD,
            &endpoint,
            LockWait {
                attempts: 3,
                sleep: Duration::from_millis(1),
            },
        );

        assert_eq!(outcome, RefreshOutcome::LockBusy);
        assert_eq!(
            outcome.attempt_classification(),
            Some(CLASSIFICATION_LOCK_BUSY)
        );
        assert_eq!(endpoint.call_count(), 0);
        drop(holder);
    }

    #[test]
    fn a_persist_failure_after_a_successful_exchange_stops_sampling() {
        // The parent directory is read-only, so the temp write fails after the
        // endpoint has already spent the refresh token.
        let scratch = Scratch::new("persist-failed");
        let path = scratch.credentials();
        fs::write(
            &path,
            credential_json("old-access", "old-refresh", NOW_MS - HOUR_MS),
        )
        .unwrap();
        let endpoint = SpyEndpoint::ok("new-access", "new-refresh", 3600);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&scratch.path, fs::Permissions::from_mode(0o500)).unwrap();
        }

        let outcome = refresh_if_expired(&path, NOW_MS, NO_LEAD, &endpoint, fast_wait());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&scratch.path, fs::Permissions::from_mode(0o700)).unwrap();
        }

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
