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
/// second one is the client secret the production code reads).
const FAKE_AGY_BINARY: &[u8] = b"FAKE_AGY_BINARY_FOR_TESTS\n\
client_id=107222333444-fakeclientidabcXYZ.apps.googleusercontent.com\n\
secret1=GOCSPX-unused-first-secret\n\
secret2=GOCSPX-real-second-secret-value\n";

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
        std::fs::write(root.join("fake-agy-binary"), FAKE_AGY_BINARY).unwrap();
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
