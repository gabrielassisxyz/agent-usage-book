//! End-to-end: an expired Antigravity OAuth token is refreshed before `aub
//! now` samples the quota endpoint, and the refresh token never reaches the
//! ledger (aub-6qay). Mirrors `tests/anthropic_token_refresh.rs`.

use std::path::PathBuf;
use std::process::Command;

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

const ANTIGRAVITY_SUCCESS_BODY: &[u8] = br#"{
    "groups": [
        {
            "displayName": "Gemini Models",
            "buckets": [
                { "window": "weekly", "resetTime": "2030-01-01T00:00:00Z", "remainingFraction": 0.5 },
                { "window": "5h", "resetTime": "2030-01-01T00:00:00Z", "remainingFraction": 0.5 }
            ]
        }
    ]
}"#;

const OLD_REFRESH: &str = "old-agy-refresh-token-do-not-leak";
const NEW_ACCESS: &str = "new-agy-access-token-abc";

/// Bytes a fake `agy` binary needs so `AntigravityTokenEndpoint::client_material`
/// (`src/auth/token_endpoint.rs`) can extract an OAuth client id and secret
/// without touching a real installation: one run starting `107` and ending
/// `.apps.googleusercontent.com`, and *two* runs starting `GOCSPX-` (the
/// second one is the client secret the production code reads). Both secrets
/// are `GOCSPX-` plus 28 characters, matching the bounded Google format the
/// extractor requires. They are assembled at run time: the literal shape is
/// what GitHub push protection matches as a Google OAuth client secret, and a
/// synthetic one in the source is refused the same as a real one.
fn fake_agy_binary() -> Vec<u8> {
    format!(
        "FAKE_AGY_BINARY_FOR_TESTS\n\
client_id=107222333444-fakeclientidabcXYZ.apps.googleusercontent.com\n\
secret1=GOCSPX-{}\n\
secret2=GOCSPX-{}\n",
        "A".repeat(28),
        "B".repeat(28),
    )
    .into_bytes()
}

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str, credential_json: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-agy-refresh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("creds")).unwrap();
        std::fs::write(root.join("creds/.token"), credential_json).unwrap();
        std::fs::write(root.join("fake-agy-binary"), fake_agy_binary()).unwrap();
        std::fs::write(
            root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n\
                 [[accounts]]\nname = \"agy-a\"\nprovider = \"agy\"\n\
                 credential = {{ kind = \"file\", path = \"{}\" }}\n",
                root.join("state").display(),
                root.join("creds/.token").display(),
            ),
        )
        .unwrap();
        Self { root }
    }

    fn credential_path(&self) -> PathBuf {
        self.root.join("creds/.token")
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("state/ledger.db")
    }

    fn run(&self, quota_url: &str, token_url: &str) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_aub"))
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .env("AUB_AGY_ENDPOINT", quota_url)
            .env("AUB_AGY_TOKEN_ENDPOINT", token_url)
            .env("AUB_AGY_BINARY", self.root.join("fake-agy-binary"))
            .args(["-v", "now", "--account", "agy-a"])
            .output()
            .expect("aub must run");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Runs the same command as `run` and returns its resident-set high-water
    /// mark in kilobytes, read by `wait4` for that child alone, so no other test
    /// running in this binary moves the figure.
    fn run_measuring_peak_kb(&self, quota_url: &str, token_url: &str) -> (i32, i64, String) {
        let stderr_path = self.root.join("stderr.txt");
        #[allow(
            clippy::zombie_processes,
            reason = "reaped by the wait4 below, which is what returns its rusage"
        )]
        let child = Command::new(env!("CARGO_BIN_EXE_aub"))
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .env("AUB_AGY_ENDPOINT", quota_url)
            .env("AUB_AGY_TOKEN_ENDPOINT", token_url)
            .env("AUB_AGY_BINARY", self.root.join("fake-agy-binary"))
            .args(["-v", "now", "--account", "agy-a"])
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .expect("aub must run");
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let mut status: libc::c_int = 0;
        // SAFETY: an all-zero rusage is a valid value of that plain C struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `pid` is this process's own unreaped child, and both out
        // pointers are live locals for the duration of the call.
        let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        assert_eq!(reaped, pid, "wait4 must reap the aub child");
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        let stderr = std::fs::read_to_string(stderr_path).unwrap_or_default();
        (code, usage.ru_maxrss, stderr)
    }

    fn evidence_capsules(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open ledger");
        let mut stmt = conn
            .prepare("SELECT evidence_capsule FROM meter_response_evidence")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn credential_json(refresh: &str, expiry: &str) -> String {
    format!(
        r#"{{"token":{{"access_token":"stale-access","refresh_token":"{refresh}","expiry":"{expiry}"}},"auth_method":"consumer"}}"#
    )
}

/// Criterion 1: an expired token is refreshed once, the sample uses the new
/// token, and the verification request pins `User-Agent: antigravity-cli`
/// (the acceptance criterion the bead calls out separately: omitting it
/// yields a 403 whose body names no header).
#[test]
fn an_expired_token_is_refreshed_once_and_the_refresh_token_never_reaches_the_ledger() {
    let env = Environment::new(
        "expired",
        &credential_json(OLD_REFRESH, "2020-01-01T00:00:00Z"),
    );

    let quota = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTIGRAVITY_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();
    let token_body = format!(r#"{{"access_token":"{NEW_ACCESS}","expires_in":3600}}"#);
    let token = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(token_body.into_bytes()),
    )])
    .unwrap();

    let (code, stdout, stderr) = env.run(&quota.url(), &token.url());
    assert_eq!(code, 0, "aub now must exit 0: {stderr}");

    // Exactly one refresh request, carrying the old refresh token.
    assert_eq!(
        token.request_count(),
        1,
        "one and only one token-endpoint call"
    );
    let refresh_request = &token.requests()[0];
    assert_eq!(refresh_request.method, "POST");
    let refresh_body = String::from_utf8_lossy(&refresh_request.body);
    assert!(refresh_body.contains(OLD_REFRESH), "body: {refresh_body}");

    // The sample went out with the new access token and the required
    // User-Agent header the quota endpoint demands.
    assert_eq!(quota.request_count(), 1);
    let quota_request = &quota.requests()[0];
    assert_eq!(
        quota_request.authorization(),
        Some(format!("Bearer {NEW_ACCESS}").as_str())
    );
    let user_agent = quota_request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.as_str());
    assert_eq!(user_agent, Some("antigravity-cli"));

    // The file on disk holds the refreshed access token.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.credential_path()).unwrap()).unwrap();
    assert_eq!(written["token"]["access_token"], NEW_ACCESS);
    assert!(
        written["token"]["expiry"].as_str().unwrap() > "2020-01-01T00:00:00Z",
        "expiry must have moved forward"
    );

    // The refresh token never appears in the ledger evidence or anything the
    // process printed.
    for capsule in env.evidence_capsules() {
        assert!(
            !capsule.contains(OLD_REFRESH),
            "capsule leaked the refresh token"
        );
    }
    assert!(!stdout.contains(OLD_REFRESH) && !stderr.contains(OLD_REFRESH));
    assert!(stderr.contains("token_refreshed"), "stderr: {stderr}");
}

/// Criterion 3: a fresh token is never refreshed, even when the quota
/// endpoint answers 429. The only trigger is the stored token's own expiry.
#[test]
fn a_rate_limited_sample_with_a_fresh_token_makes_no_token_endpoint_call() {
    let env = Environment::new(
        "fresh-429",
        &credential_json(OLD_REFRESH, "2030-01-01T00:00:00Z"),
    );

    let quota = SyntheticServer::start(vec![ScriptedOutcome::TooManyRequests429 {
        retry_after_seconds: Some(30),
    }])
    .unwrap();
    let token = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{}".to_vec()),
    )])
    .unwrap();

    let (_code, stdout, stderr) = env.run(&quota.url(), &token.url());

    assert_eq!(
        token.request_count(),
        0,
        "a 429 must never trigger a refresh: token endpoint calls"
    );
    // The credential file is untouched.
    let on_disk = std::fs::read_to_string(env.credential_path()).unwrap();
    assert!(on_disk.contains(OLD_REFRESH));
    assert!(!stdout.contains(OLD_REFRESH) && !stderr.contains(OLD_REFRESH));
}

/// Criterion 4: `invalid_grant` from the token endpoint leaves the file
/// untouched and the attempt records `refresh_rejected`.
#[test]
fn invalid_grant_leaves_the_file_untouched_and_records_refresh_rejected() {
    let env = Environment::new(
        "invalid-grant",
        &credential_json(OLD_REFRESH, "2020-01-01T00:00:00Z"),
    );
    let original = std::fs::read_to_string(env.credential_path()).unwrap();

    let quota = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let token = SyntheticServer::start(vec![ScriptedOutcome::Response {
        status: 400,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: br#"{"error":"invalid_grant"}"#.to_vec(),
    }])
    .unwrap();

    let (_code, _stdout, stderr) = env.run(&quota.url(), &token.url());

    assert_eq!(token.request_count(), 1);
    assert_eq!(
        std::fs::read_to_string(env.credential_path()).unwrap(),
        original,
        "the credential file must be byte-identical after invalid_grant"
    );
    assert!(
        stderr.contains("rejected the stored refresh token"),
        "stderr: {stderr}"
    );

    let conn = rusqlite::Connection::open(env.db_path()).unwrap();
    let classification: String = conn
        .query_row(
            "SELECT sanitized_error_classification FROM meter_attempt_result",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(classification, "refresh_rejected");
}

/// The pairing half of the bead's criterion: when the token endpoint rejects
/// the OAuth *client* credentials (`invalid_client` / `unauthorized_client`,
/// RFC 6749 5.2) rather than the refresh token, the attempt records
/// `refresh_configuration_failed`, not the ordinary auth/unreachable
/// classification `invalid_grant` would produce. The two remediations differ:
/// a rejected client pair is re-extracted from the `agy` binary, a rejected
/// refresh token is re-authenticated.
#[test]
fn a_rejected_client_pairing_records_refresh_configuration_failed() {
    let env = Environment::new(
        "invalid-client",
        &credential_json(OLD_REFRESH, "2020-01-01T00:00:00Z"),
    );
    let original = std::fs::read_to_string(env.credential_path()).unwrap();

    let quota = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let token = SyntheticServer::start(vec![ScriptedOutcome::Response {
        status: 401,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body:
            br#"{"error":"invalid_client","error_description":"The OAuth client was not found."}"#
                .to_vec(),
    }])
    .unwrap();

    let (_code, _stdout, stderr) = env.run(&quota.url(), &token.url());

    assert_eq!(token.request_count(), 1);
    assert_eq!(
        std::fs::read_to_string(env.credential_path()).unwrap(),
        original,
        "the credential file must be byte-identical when the client pairing is rejected"
    );
    assert!(
        stderr.contains("could not configure Antigravity OAuth refresh"),
        "stderr: {stderr}"
    );

    let conn = rusqlite::Connection::open(env.db_path()).unwrap();
    let classification: String = conn
        .query_row(
            "SELECT sanitized_error_classification FROM meter_attempt_result",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(classification, "refresh_configuration_failed");
}

/// aub-i2i9: the client material is extracted from an `agy` binary of about
/// 200 MB on every tick whose stored token is expired, and one that stays
/// expired (the endpoint keeps refusing the refresh) makes that every tick.
/// Reading the binary whole and copying it into a lossy UTF-8 string peaked
/// those ticks near 500 MB. This fake binary is 64 MB of bytes that are not
/// UTF-8 with the material at its end, so the whole-file read would peak near
/// 64 MB plus a lossy copy of about 192 MB; a streaming scan stays at what a
/// tick needs without it. The request body proves the material was still
/// found behind the 64 MB.
#[test]
fn extracting_client_material_from_a_large_binary_does_not_hold_it_in_memory() {
    const FILLER_BYTES: usize = 64 * 1024 * 1024;
    const PEAK_BOUND_KB: i64 = 48 * 1024;
    let env = Environment::new(
        "large-binary",
        &credential_json(OLD_REFRESH, "2020-01-01T00:00:00Z"),
    );
    // Written a megabyte at a time: `ru_maxrss` of a child started through
    // `posix_spawn` carries the parent's own high-water mark, so a 64 MB buffer
    // held here would be counted against aub.
    {
        use std::io::Write;
        let mut binary = std::fs::File::create(env.root.join("fake-agy-binary")).unwrap();
        let megabyte = vec![0xFF_u8; 1024 * 1024];
        for _ in 0..FILLER_BYTES / megabyte.len() {
            binary.write_all(&megabyte).unwrap();
        }
        binary.write_all(&fake_agy_binary()).unwrap();
    }

    let quota = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let token = SyntheticServer::start(vec![ScriptedOutcome::Response {
        status: 401,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: br#"{"error":"invalid_client"}"#.to_vec(),
    }])
    .unwrap();

    let (_code, peak_kb, stderr) = env.run_measuring_peak_kb(&quota.url(), &token.url());

    assert_eq!(token.request_count(), 1, "stderr: {stderr}");
    let refresh_body = String::from_utf8_lossy(&token.requests()[0].body).into_owned();
    assert!(
        refresh_body.contains("107222333444-fakeclientidabcXYZ.apps.googleusercontent.com")
            && refresh_body.contains(&format!("GOCSPX-{}", "B".repeat(28))),
        "the material behind the filler must still be extracted: {refresh_body}"
    );
    assert!(
        peak_kb < PEAK_BOUND_KB,
        "aub peaked at {peak_kb} kB extracting client material from a {FILLER_BYTES}-byte binary; bound {PEAK_BOUND_KB} kB"
    );
}
