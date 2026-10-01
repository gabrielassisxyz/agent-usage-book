//! Credential isolation through the release binary (aub-0ere).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use test_support::StateDir;
use test_support::synthetic_server::{ScriptedOutcome, ScriptedResponseBody, SyntheticServer};

type Marker = (&'static str, &'static str);
const PRIMARY: Marker = ("primary token", "boundary-primary-11111");
const SECONDARY: Marker = ("secondary token", "boundary-secondary-22222");
const AMBIENT: &[Marker] = &[
    ("ambient OAuth token", "ambient-oauth-secret-99999"),
    ("ambient API key", "ambient-secondary-marker-88888"),
];
const RESPONSE: &[Marker] = &[
    ("response authorization", "super-secret-should-be-removed"),
    ("response Bearer", "abc123-secret-token"),
];
const MISSING_FIELD_BODY: &[u8] = br#"{"authorization":"super-secret-should-be-removed","note":"Bearer abc123-secret-token","seven_day":{"utilization":91.0,"resets_at":"2026-09-06T12:00:00.000Z"}}"#;

// Ordinary cargo test must exercise the release binary too. Cargo releases its
// build lock before starting integration tests; one build is shared by this suite.
fn release_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let target = Path::new(env!("CARGO_BIN_EXE_aub"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let status = Command::new(env!("CARGO"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("CARGO_TARGET_DIR", target)
            .args(["build", "--release", "--bin", "aub"])
            .status()
            .expect("release build must run");
        check(
            status.success(),
            "build",
            "all",
            "release binary",
            "build failed",
        );
        target.join("release/aub")
    })
}

fn check(ok: bool, surface: &str, account: &str, marker: &str, reason: &str) {
    assert!(
        ok,
        "surface={surface} account={account} marker={marker}: {reason}"
    );
}

// Never format captured streams, headers, rows or JSON in assertion messages.
fn assert_absent(bytes: &[u8], surface: &str, account: &str, markers: &[Marker]) {
    for (name, value) in markers {
        check(
            !bytes
                .windows(value.len())
                .any(|part| part == value.as_bytes()),
            surface,
            account,
            name,
            "secret marker escaped",
        );
    }
}

fn scan_json(value: &serde_json::Value, surface: &str, account: &str, markers: &[Marker]) {
    match value {
        serde_json::Value::String(text) => {
            assert_absent(text.as_bytes(), surface, account, markers)
        }
        serde_json::Value::Array(values) => {
            // Retained bodies serialize bytes as JSON numbers. Searching the
            // serialized file alone would miss an unredacted body entirely.
            if let Ok(bytes) = serde_json::from_value::<Vec<u8>>(value.clone()) {
                assert_absent(&bytes, surface, account, markers);
            }
            for child in values {
                scan_json(child, surface, account, markers);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, child) in fields {
                assert_absent(key.as_bytes(), surface, account, markers);
                scan_json(child, surface, account, markers);
            }
        }
        _ => {}
    }
}

fn scan(bytes: &[u8], surface: &str, account: &str, markers: &[Marker]) {
    assert_absent(bytes, surface, account, markers);
    if let Ok(value) = serde_json::from_slice(bytes) {
        scan_json(&value, surface, account, markers);
    }
}

struct Environment {
    root: StateDir,
}

impl Environment {
    fn new(accounts: &[(&str, Marker)]) -> Self {
        let root = StateDir::new();
        std::fs::create_dir(root.path().join("home")).unwrap();
        let mut config = format!(
            "state.dir = {:?}\n[sampling]\nmax_concurrent_requests = 1\n",
            root.path().join("state")
        );
        for (name, (_, token)) in accounts {
            let credential = root.path().join(format!("{name}.json"));
            std::fs::write(&credential, format!(r#"{{"accessToken":"{token}"}}"#)).unwrap();
            config.push_str(&format!(
                "\n[[accounts]]\nname = {name:?}\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = {credential:?} }}\n"
            ));
        }
        std::fs::write(root.path().join("aub.toml"), config).unwrap();
        Self { root }
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state().join("ledger.db")).expect("open scratch ledger")
    }

    fn sample(&self, server: &SyntheticServer) -> Output {
        Command::new(release_binary())
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("AUB_CONFIG_FILE", self.root.path().join("aub.toml"))
            .env("AUB_ANTHROPIC_ENDPOINT", server.url())
            .env("AUB_LOG_LEVEL", "trace")
            .env("CLAUDE_CODE_OAUTH_TOKEN", AMBIENT[0].1)
            .env("ANTHROPIC_API_KEY", AMBIENT[1].1)
            .arg("sample")
            .output()
            .expect("sample must execute")
    }

    fn refuse_observation_commit(&self) {
        // A constraint failure happens after the real spool write. No timing
        // race or production fault-injection hook is needed to preserve it.
        self.db()
            .execute_batch(
                "CREATE TRIGGER refuse_observation BEFORE INSERT ON meter_observation
             BEGIN SELECT RAISE(ABORT, 'credential boundary commit refusal'); END;",
            )
            .expect("install scratch ledger refusal");
    }

    fn inspect(&self, output: &Output, account: &str, markers: &[Marker]) {
        for (surface, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            check(
                !bytes.is_empty(),
                surface,
                account,
                "presence",
                "surface is empty",
            );
            scan(bytes, surface, account, markers);
        }
        let events: Vec<serde_json::Value> = output
            .stderr
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .filter(|value| value.get("event").is_some())
            .collect();
        check(
            !events.is_empty(),
            "structured run log",
            account,
            "presence",
            "no trace-level events",
        );
        for event in &events {
            scan_json(event, "structured run log", account, markers);
        }
        let attempts: i64 = self
            .db()
            .query_row("SELECT count(*) FROM meter_attempt", [], |r| r.get(0))
            .unwrap();
        check(
            attempts > 0,
            "ledger",
            account,
            "presence",
            "no attempt rows",
        );
        for name in ["ledger.db", "ledger.db-wal", "ledger.db-shm"] {
            let path = self.state().join(name);
            if path.exists() {
                scan(&std::fs::read(path).unwrap(), "ledger", account, markers);
            }
        }
        let conn = self.db();
        let mut statement = conn
            .prepare("SELECT evidence_capsule FROM meter_response_evidence")
            .unwrap();
        for row in statement.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            scan(
                row.unwrap().as_bytes(),
                "ledger evidence capsule",
                account,
                markers,
            );
        }
        for dir in ["pending", "retained-bodies"] {
            scan_files(&self.state().join(dir), dir, account, markers);
        }
    }

    fn assert_pending(&self, account: &str, markers: &[Marker]) {
        let count = scan_files(
            &self.state().join("pending"),
            "pending spool",
            account,
            markers,
        );
        check(
            count > 0,
            "pending spool",
            account,
            "presence",
            "no pending files",
        );
        for entry in std::fs::read_dir(self.state().join("pending")).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).expect("pending record is JSON");
            check(
                value.get("attempt_id").is_some(),
                "pending spool",
                account,
                "attempt",
                "not a terminal bundle",
            );
            check(
                value["windows"].as_array().is_some_and(|v| !v.is_empty()),
                "pending spool",
                account,
                "reading",
                "no measured windows",
            );
        }
    }

    fn assert_evidence(&self, account: &str) {
        let count: i64 = self
            .db()
            .query_row("SELECT count(*) FROM meter_response_evidence", [], |r| {
                r.get(0)
            })
            .unwrap();
        check(
            count > 0,
            "ledger evidence capsule",
            account,
            "presence",
            "no retained response rows",
        );
    }
}

fn scan_files(path: &Path, surface: &str, account: &str, markers: &[Marker]) -> usize {
    if !path.exists() {
        return 0;
    }
    let mut count = 0;
    for entry in std::fs::read_dir(path).expect("read scratch surface") {
        let path = entry.unwrap().path();
        if path.is_dir() {
            count += scan_files(&path, surface, account, markers);
        } else {
            let bytes = std::fs::read(path).expect("read every surface file");
            check(
                !bytes.is_empty(),
                surface,
                account,
                "presence",
                "empty surface file",
            );
            scan(&bytes, surface, account, markers);
            if surface == "retained provider body" {
                let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let body: Vec<u8> = serde_json::from_value(record["body_bytes"].clone()).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                check(
                    body.get("seven_day").is_some(),
                    surface,
                    account,
                    "quota window",
                    "retained body lost its non-secret content",
                );
            }
            count += 1;
        }
    }
    count
}

fn response(primary: u32, secondary: u32, secrets: bool) -> Vec<u8> {
    let mut body = serde_json::json!({
        "five_hour": {"utilization": primary, "resets_at": "2026-10-02T06:00:00Z"},
        "seven_day": {"utilization": secondary, "resets_at": "2026-10-08T00:00:00Z"}
    });
    if secrets {
        body["authorization"] = RESPONSE[0].1.into();
        body["note"] = format!("Bearer {}", RESPONSE[1].1).into();
    }
    serde_json::to_vec(&body).unwrap()
}

fn server(bodies: Vec<Vec<u8>>) -> SyntheticServer {
    SyntheticServer::start(
        bodies
            .into_iter()
            .map(|body| ScriptedOutcome::Success(ScriptedResponseBody::json_ok(body)))
            .collect(),
    )
    .expect("synthetic server must start")
}

fn assert_wire(server: &SyntheticServer, accounts: &[(&str, Marker)]) {
    let requests = server.requests();
    check(
        requests.len() == accounts.len(),
        "wire",
        "configured accounts",
        "request count",
        "expected one request per sample",
    );
    for (request, (account, marker)) in requests.iter().zip(accounts) {
        check(
            request.authorization() == Some(format!("Bearer {}", marker.1).as_str()),
            "wire",
            account,
            marker.0,
            "request did not carry its own credential",
        );
        for (other, other_marker) in accounts.iter().filter(|(other, _)| other != account) {
            check(
                request.authorization() != Some(format!("Bearer {}", other_marker.1).as_str()),
                "wire",
                account,
                other,
                "request carried another account's credential",
            );
        }
    }
}

#[test]
fn credential_resolution_boundary_never_leaks_or_mixes_ambient_token() {
    let env = Environment::new(&[("work-primary", PRIMARY)]);
    let server = server(vec![response(11, 12, false), response(11, 12, false)]);
    let markers = [PRIMARY, AMBIENT[0], AMBIENT[1]];
    let committed = env.sample(&server);
    env.inspect(&committed, "work-primary", &markers);
    check(
        committed.status.success(),
        "process",
        "work-primary",
        "commit",
        "sample failed",
    );
    env.assert_evidence("work-primary");
    env.refuse_observation_commit();
    let spooled = env.sample(&server);
    env.inspect(&spooled, "work-primary", &markers);
    env.assert_pending("work-primary", &markers);
    check(
        spooled.status.code() == Some(8),
        "process",
        "work-primary",
        "commit refusal",
        "expected ingest-incomplete exit class",
    );
    assert_wire(
        &server,
        &[("work-primary", PRIMARY), ("work-primary", PRIMARY)],
    );
}

#[test]
fn credential_shaped_provider_response_reaches_no_surface_through_the_binary() {
    let env = Environment::new(&[("work-primary", PRIMARY)]);
    let server = server(vec![
        MISSING_FIELD_BODY.to_vec(),
        response(13, 91, true),
        response(13, 91, true),
    ]);
    let markers = [RESPONSE[0], RESPONSE[1], PRIMARY, AMBIENT[0], AMBIENT[1]];
    let failed_parse = env.sample(&server);
    env.inspect(&failed_parse, "work-primary", &markers);
    check(
        failed_parse.status.success(),
        "process",
        "work-primary",
        "parse failure",
        "evidence was not recorded",
    );
    let retained = scan_files(
        &env.state().join("retained-bodies"),
        "retained provider body",
        "work-primary",
        &markers,
    );
    check(
        retained > 0,
        "retained provider body",
        "work-primary",
        "presence",
        "parse failure retained no body",
    );
    // The missing-field fixture cannot yield a measured observation. Add its
    // required window to reach both committed evidence and the pending spool.
    let committed = env.sample(&server);
    env.inspect(&committed, "work-primary", &markers);
    check(
        committed.status.success(),
        "process",
        "work-primary",
        "commit",
        "sample failed",
    );
    env.assert_evidence("work-primary");
    env.refuse_observation_commit();
    let spooled = env.sample(&server);
    env.inspect(&spooled, "work-primary", &markers);
    env.assert_pending("work-primary", &markers);
    check(
        spooled.status.code() == Some(8),
        "process",
        "work-primary",
        "commit refusal",
        "expected ingest-incomplete exit class",
    );
    assert_wire(&server, &[("work-primary", PRIMARY); 3]);
}

#[test]
fn two_accounts_each_send_only_their_own_credential_and_own_their_observations() {
    let accounts = [("work-primary", PRIMARY), ("work-secondary", SECONDARY)];
    let env = Environment::new(&accounts);
    // One worker pairs script and request order. Validate that pairing on the
    // wire before using each response's distinct utilization as ledger proof.
    let server = server(vec![response(11, 12, false), response(77, 78, false)]);
    let output = env.sample(&server);
    for (account, token) in accounts {
        env.inspect(&output, account, &[token, AMBIENT[0], AMBIENT[1]]);
    }
    check(
        output.status.success(),
        "process",
        "both accounts",
        "sample",
        "sample failed",
    );
    assert_wire(&server, &accounts);
    let conn = env.db();
    let mut statement = conn
        .prepare(
            "SELECT a.logical_name, w.semantic_key, w.quota_used_ppm
         FROM meter_window w JOIN meter_observation o ON o.id = w.observation_id
         JOIN account a ON a.id = o.account_id ORDER BY a.logical_name, w.semantic_key",
        )
        .unwrap();
    let rows: Vec<(String, String, i64)> = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    check(
        rows.len() == 4,
        "ledger",
        "both accounts",
        "window count",
        "expected two windows per account",
    );
    for (account, window, ppm) in [
        ("work-primary", "five_hour", 110_000),
        ("work-primary", "seven_day", 120_000),
        ("work-secondary", "five_hour", 770_000),
        ("work-secondary", "seven_day", 780_000),
    ] {
        check(
            rows.iter()
                .any(|row| row.0 == account && row.1 == window && row.2 == ppm),
            "ledger",
            account,
            window,
            "observation does not match the credential's response",
        );
    }
}
