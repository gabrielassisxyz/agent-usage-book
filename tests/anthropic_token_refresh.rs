//! End-to-end: an expired Anthropic OAuth token is refreshed under
//! `.credentials.lock` before `aub now` samples, and the refresh token never
//! reaches the ledger (aub-79gp).

use std::path::PathBuf;
use std::process::Command;

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

const ANTHROPIC_SUCCESS_BODY: &[u8] = br#"{
    "five_hour": { "utilization": 25.0, "resets_at": "2030-01-01T00:00:00Z" },
    "seven_day": { "utilization": 50.0, "resets_at": "2030-01-01T00:00:00Z" }
}"#;

const OLD_REFRESH: &str = "old-refresh-token-do-not-leak";
const NEW_REFRESH: &str = "new-refresh-token-also-secret";
const NEW_ACCESS: &str = "new-access-token-abc";

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str, credential_json: &str) -> Self {
        let root = std::env::temp_dir().join(format!("aub-refresh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("creds")).unwrap();
        std::fs::write(root.join("creds/.credentials.json"), credential_json).unwrap();
        std::fs::write(
            root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n\
                 [[accounts]]\nname = \"work-a\"\nprovider = \"anthropic\"\n\
                 credential = {{ kind = \"file\", path = \"{}\" }}\n",
                root.join("state").display(),
                root.join("creds/.credentials.json").display(),
            ),
        )
        .unwrap();
        Self { root }
    }

    fn credential_path(&self) -> PathBuf {
        self.root.join("creds/.credentials.json")
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("state/ledger.db")
    }

    fn run(&self, usage_url: &str, token_url: &str) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_aub"))
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .env("AUB_ANTHROPIC_ENDPOINT", usage_url)
            .env("AUB_ANTHROPIC_TOKEN_ENDPOINT", token_url)
            .args(["-v", "now", "--account", "work-a"])
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

fn credential_json(refresh: &str, expires_at_ms: i64) -> String {
    format!(
        r#"{{"claudeAiOauth":{{"accessToken":"stale-access","refreshToken":"{refresh}","expiresAt":{expires_at_ms},"scopes":["user:inference"],"subscriptionType":"max"}}}}"#
    )
}

/// Criterion 1 and 5: an expired token is refreshed once, the sample uses the
/// new token, the file holds the rotated pair with `subscriptionType`
/// preserved, and neither refresh token appears in the ledger evidence or the
/// process output.
#[test]
fn an_expired_token_is_refreshed_once_and_the_refresh_token_never_reaches_the_ledger() {
    let env = Environment::new("expired", &credential_json(OLD_REFRESH, 1_000_000_000_000));

    let usage = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();
    let token_body = format!(
        r#"{{"access_token":"{NEW_ACCESS}","refresh_token":"{NEW_REFRESH}","expires_in":3600}}"#
    );
    let token = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(token_body.into_bytes()),
    )])
    .unwrap();

    let (code, stdout, stderr) = env.run(&usage.url(), &token.url());
    assert_eq!(code, 0, "aub now must exit 0: {stderr}");

    // Exactly one refresh request, carrying the old refresh token and the
    // Claude Code client id.
    assert_eq!(
        token.request_count(),
        1,
        "one and only one token-endpoint call"
    );
    let refresh_request = &token.requests()[0];
    assert_eq!(refresh_request.method, "POST");
    let refresh_body = String::from_utf8_lossy(&refresh_request.body);
    assert!(refresh_body.contains(OLD_REFRESH), "body: {refresh_body}");
    assert!(
        refresh_body.contains("9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
        "body: {refresh_body}"
    );

    // The sample went out with the new access token.
    assert_eq!(usage.request_count(), 1);
    assert_eq!(
        usage.requests()[0].authorization(),
        Some(format!("Bearer {NEW_ACCESS}").as_str())
    );

    // The file on disk holds the rotated pair; other fields are unchanged.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.credential_path()).unwrap()).unwrap();
    let oauth = &written["claudeAiOauth"];
    assert_eq!(oauth["accessToken"], NEW_ACCESS);
    assert_eq!(oauth["refreshToken"], NEW_REFRESH);
    assert!(oauth["expiresAt"].as_i64().unwrap() > 1_000_000_000_000);
    assert_eq!(oauth["subscriptionType"], "max");
    assert_eq!(oauth["scopes"][0], "user:inference");

    // Criterion 5: no refresh token, old or new, in the ledger's evidence or
    // in anything the process printed.
    for capsule in env.evidence_capsules() {
        assert!(
            !capsule.contains(OLD_REFRESH),
            "capsule leaked the old refresh token"
        );
        assert!(
            !capsule.contains(NEW_REFRESH),
            "capsule leaked the new refresh token"
        );
    }
    assert!(!stdout.contains(OLD_REFRESH) && !stdout.contains(NEW_REFRESH));
    assert!(!stderr.contains(OLD_REFRESH) && !stderr.contains(NEW_REFRESH));
    // The verbose diagnostic names the classification without the secret.
    assert!(stderr.contains("token_refreshed"), "stderr: {stderr}");
}

/// Criterion 3: a fresh token is never refreshed, even when the usage endpoint
/// answers 429. The only trigger is the stored token's own expiry.
#[test]
fn a_rate_limited_sample_with_a_fresh_token_makes_no_token_endpoint_call() {
    let env = Environment::new(
        "fresh-429",
        &credential_json(OLD_REFRESH, 4_102_444_800_000),
    );

    let usage = SyntheticServer::start(vec![ScriptedOutcome::TooManyRequests429 {
        retry_after_seconds: Some(30),
    }])
    .unwrap();
    let token = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{}".to_vec()),
    )])
    .unwrap();

    let (_code, stdout, stderr) = env.run(&usage.url(), &token.url());

    assert_eq!(
        token.request_count(),
        0,
        "a 429 must never trigger a refresh: token endpoint calls"
    );
    // The credential file is untouched.
    let on_disk = std::fs::read_to_string(env.credential_path()).unwrap();
    assert!(on_disk.contains(OLD_REFRESH));
    assert!(!on_disk.contains(NEW_REFRESH));
    assert!(!stdout.contains(OLD_REFRESH) && !stderr.contains(OLD_REFRESH));
}

/// Criterion 4: `invalid_grant` from the token endpoint leaves the file
/// untouched and the attempt records `refresh_rejected`.
#[test]
fn invalid_grant_leaves_the_file_untouched_and_records_refresh_rejected() {
    let env = Environment::new(
        "invalid-grant",
        &credential_json(OLD_REFRESH, 1_000_000_000_000),
    );
    let original = std::fs::read_to_string(env.credential_path()).unwrap();

    let usage = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let token = SyntheticServer::start(vec![ScriptedOutcome::Response {
        status: 400,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: br#"{"error":"invalid_grant"}"#.to_vec(),
    }])
    .unwrap();

    let (_code, _stdout, stderr) = env.run(&usage.url(), &token.url());

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
