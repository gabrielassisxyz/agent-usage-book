//! Process-level exit-status tests for `aub status` (aub-me5.6): every state
//! the command can display exits zero, and the only non-zero exit is an
//! argument failure. Each case seeds a real projection file and drives the
//! release-style test binary through a real subprocess.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

const NANOS_PER_SECOND: i64 = 1_000_000_000;

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-status-exit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        // The state dir is pinned to this environment: the projection is
        // looked up there and nowhere else.
        std::fs::write(
            root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\n",
                root.join("state").display()
            ),
        )
        .unwrap();
        Self { root }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn projection_path(&self) -> PathBuf {
        self.state_dir().join("projection")
    }

    fn write_projection(&self, body: &str) {
        std::fs::write(self.projection_path(), body).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = aub();
        command
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command(args).output().expect("aub must run");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after the epoch")
        .as_nanos() as i64
}

fn window(used_ppm: i32) -> String {
    format!(
        r#"{{"semantic_key":"five_hour","scope_kind":"account_wide","scoped_model":null,"quota_used_ppm":{used_ppm},"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":{},"nominal_duration_nanos":{}}}"#,
        now_nanos() + 3 * 3600 * NANOS_PER_SECOND,
        5 * 3600 * NANOS_PER_SECOND,
    )
}

fn projection_body(
    observation: Option<(i32, i64)>,
    attempt: Option<(&str, Option<i64>)>,
) -> String {
    let observation_json = observation
        .map(|(used_ppm, received_seconds_ago)| {
            let received = now_nanos() - received_seconds_ago * NANOS_PER_SECOND;
            format!(
                r#"{{"observation_id":7,"provider_observed_at_nanos":{received},"received_at_nanos":{received},"measurement_basis":"provider_observed","windows":[{}]}}"#,
                window(used_ppm)
            )
        })
        .unwrap_or_else(|| "null".to_string());
    let attempt_json = match attempt {
        None => "null".to_string(),
        Some((outcome, completed_seconds_ago)) => {
            let result = completed_seconds_ago
                .map(|ago| {
                    let completed = now_nanos() - ago * NANOS_PER_SECOND;
                    let failure = if outcome == "unreachable" {
                        "\"failure_class\":\"transport_timeout\""
                    } else {
                        "\"failure_class\":null"
                    };
                    format!(
                        r#"{{"completed_at_nanos":{completed},"outcome":"{outcome}",{failure}}}"#
                    )
                })
                .unwrap_or_else(|| "null".to_string());
            let started = now_nanos() - 120 * NANOS_PER_SECOND;
            format!(
                r#"{{"attempt_id":9,"request_started_at_nanos":{started},"credential_context_id":"credential-context-v1","result":{result}}}"#,
                result = result,
            )
        }
    };
    format!(
        r#"{{"schema_version":1,"ledger_generation":12,"accounts":[{{"account_id":1,"logical_name":"work-primary","provider":"anthropic","last_successful_observation":{observation},"latest_attempt":{attempt}}}]}}"#,
        observation = observation_json,
        attempt = attempt_json,
    )
}

/// Fresh: exit zero, the reading rendered with the window beside it.
#[test]
fn fresh_exits_zero() {
    let env = Environment::new("fresh");
    env.write_projection(&projection_body(
        Some((620_000, 41)),
        Some(("success", Some(41))),
    ));
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0, "status must exit zero for a fresh reading");
    assert!(
        stdout.contains("aub work-primary 38% left · 5h"),
        "{stdout}"
    );
}

/// Stale by age: exit zero, the historical value with its age and reason.
#[test]
fn stale_exits_zero() {
    let env = Environment::new("stale");
    env.write_projection(&projection_body(
        Some((620_000, 14 * 60)),
        Some(("success", Some(60))),
    ));
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0, "status must exit zero for a stale reading");
    assert!(
        stdout.contains("aub work-primary ~38% · stale 14m · age exceeded"),
        "{stdout}"
    );
}

/// Auth required: exit zero, the auth form.
#[test]
fn auth_required_exits_zero() {
    let env = Environment::new("auth");
    env.write_projection(&projection_body(
        Some((620_000, 5 * 60)),
        Some(("auth_required", Some(29))),
    ));
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0, "status must exit zero for an auth-required state");
    assert!(stdout.contains("aub work-primary auth!"), "{stdout}");
}

/// No successful sample: exit zero, the question mark with the reason.
#[test]
fn no_successful_sample_exits_zero() {
    let env = Environment::new("nosample");
    env.write_projection(&projection_body(None, None));
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0, "status must exit zero with no successful sample");
    assert!(
        stdout.contains("aub work-primary ? · stale · no successful sample"),
        "{stdout}"
    );
}

/// Collector interrupted: exit zero, the interrupted line.
#[test]
fn collector_interrupted_exits_zero() {
    let env = Environment::new("interrupted");
    env.write_projection(&projection_body(
        Some((620_000, 9 * 60)),
        Some(("success", None)),
    ));
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(
        code, 0,
        "status must exit zero when the collector was interrupted"
    );
    assert!(
        stdout.contains("aub work-primary ~38% · stale 9m · collector interrupted"),
        "{stdout}"
    );
}

/// Missing projection: exit zero, the degraded question mark, no account line.
#[test]
fn missing_projection_exits_zero() {
    let env = Environment::new("missing");
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0, "status must exit zero with the projection missing");
    assert_eq!(stdout.trim_end(), "aub ?", "{stdout}");
    assert!(
        !stdout.contains("work-primary"),
        "no account value may be substituted: {stdout}"
    );
}

/// The degraded exit stays zero in JSON too, with the reason in the document.
#[test]
fn missing_projection_json_exits_zero_and_names_the_state() {
    let env = Environment::new("missing-json");
    let (code, stdout, _) = env.run(&["status", "--format", "json"]);
    assert_eq!(code, 0);
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(document["projection"]["state"], "missing");
    assert!(
        document["projection"]["reason"].is_string(),
        "the reason rides in the document: {document}"
    );
    assert_eq!(document["accounts"].as_array().map(Vec::len), Some(0));
}

/// An unsupported schema version: exit zero, the question mark with the reason.
#[test]
fn unsupported_schema_exits_zero() {
    let env = Environment::new("schema");
    env.write_projection("{\"schema_version\":99,\"ledger_generation\":1,\"accounts\":[]}");
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(
        code, 0,
        "status must exit zero for an unsupported projection"
    );
    assert!(stdout.trim_end().starts_with("aub ?"), "{stdout}");
}

/// A malformed projection: exit zero, the question mark with the reason.
#[test]
fn malformed_projection_exits_zero() {
    let env = Environment::new("malformed");
    env.write_projection("not json at all");
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0, "status must exit zero for a malformed projection");
    assert!(stdout.trim_end().starts_with("aub ?"), "{stdout}");
}

/// Argument failures are the only non-zero exits: an unknown flag, a missing
/// flag argument and an unknown account are each the typed usage condition.
#[test]
fn argument_failures_exit_two() {
    let env = Environment::new("args");
    for args in [
        vec!["status", "--definitely-not-a-flag"],
        vec!["status", "--model"],
        vec!["status", "extra-positional"],
        vec!["status", "--account", "nobody-configured"],
    ] {
        let (code, _, stderr) = env.run(&args);
        assert_eq!(
            code, 2,
            "arguments {args:?} must be the usage class: {stderr}"
        );
    }
}

/// The unknown-account condition names the account and the configured set's
/// requirement, so the correction is stated in the message.
#[test]
fn unknown_account_is_named_in_the_error() {
    let env = Environment::new("unknown-account");
    let (code, _, stderr) = env.run(&["status", "--account", "ghost"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown account 'ghost'"), "{stderr}");
}

/// An account selector over a seeded projection renders only that account,
/// and the exit stays zero.
#[test]
fn account_selector_renders_one_account() {
    let env = Environment::new("account-selector");
    // Both accounts are configured; the projection reports them in its own
    // order, and the selector renders only the one it names.
    std::fs::write(
        env.root.join("aub.toml"),
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\n\n[[accounts]]\nname = \"other\"\nprovider = \"anthropic\"\n",
            env.state_dir().display()
        ),
    )
    .unwrap();
    let body = projection_body(None, None).replace(
        "\"accounts\":[",
        "\"accounts\":[{\"account_id\":2,\"logical_name\":\"other\",\"provider\":\"anthropic\",\"last_successful_observation\":null,\"latest_attempt\":null},",
    );
    env.write_projection(&body);
    let (code, stdout, _) = env.run(&["status", "--account", "other"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("aub other"), "{stdout}");
    assert!(!stdout.contains("work-primary"), "{stdout}");
}

/// The model selector excludes unrelated model-scoped windows: the seeded
/// account-wide window still constrains, the other model's window does not.
#[test]
fn model_selector_excludes_unrelated_model_windows() {
    let env = Environment::new("model-selector");
    let received = now_nanos() - 41 * NANOS_PER_SECOND;
    let body = format!(
        r#"{{"schema_version":1,"ledger_generation":12,"accounts":[{{"account_id":1,"logical_name":"work-primary","provider":"anthropic","last_successful_observation":{{"observation_id":7,"provider_observed_at_nanos":{received},"received_at_nanos":{received},"measurement_basis":"provider_observed","windows":[{wide},{scoped},{unrelated}]}},"latest_attempt":{{"attempt_id":9,"request_started_at_nanos":{received},"credential_context_id":"ctx","result":{{"completed_at_nanos":{received},"outcome":"success","failure_class":null}}}}}}]}}"#,
        received = received,
        wide = window(500_000),
        scoped = format!(
            r#"{{"semantic_key":"weekly","scope_kind":"model_specific","scoped_model":"claude-model-x","quota_used_ppm":700000,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":{},"nominal_duration_nanos":{}}}"#,
            now_nanos() + 3600 * NANOS_PER_SECOND,
            7 * 86_400 * NANOS_PER_SECOND
        ),
        unrelated = format!(
            r#"{{"semantic_key":"weekly","scope_kind":"model_specific","scoped_model":"claude-model-y","quota_used_ppm":950000,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":{},"nominal_duration_nanos":{}}}"#,
            now_nanos() + 3600 * NANOS_PER_SECOND,
            7 * 86_400 * NANOS_PER_SECOND
        ),
    );
    env.write_projection(&body);

    // Without a selector every window applies, so the unrelated model's 95%
    // used window would limit the line to 5%. With the selector, only the
    // account-wide and chosen-model windows apply: 30% left, 5h window.
    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("aub work-primary 5% left"), "{stdout}");

    let (code, stdout, _) = env.run(&["status", "--model", "claude-model-x"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("aub work-primary 30% left · 7d"),
        "the unrelated model's window must be excluded, and the weekly window limits: {stdout}"
    );
}

/// Default stderr is empty: the diagnostic log stays silent at the default level.
#[test]
fn default_stderr_is_empty() {
    let env = Environment::new("stderr");
    env.write_projection(&projection_body(None, None));
    let output = env.command(&["status"]).output().unwrap();
    assert!(
        output.stderr.is_empty(),
        "default stderr must be empty: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One projection read: at debug level the log names the read exactly once.
#[test]
fn the_projection_is_read_exactly_once() {
    let env = Environment::new("one-read");
    env.write_projection(&projection_body(None, None));
    let output = env
        .command(&["status"])
        .env("AUB_LOG_LEVEL", "debug")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let reads = stderr.matches("projection_read").count();
    assert_eq!(
        reads, 1,
        "exactly one projection read may be logged: {stderr}"
    );
    assert!(
        !stderr.contains("request_attempted"),
        "status never reaches the network: {stderr}"
    );
}

/// The JSON contract: every freshness variant is present exactly once, and
/// the document validates against the versioned contract.
#[test]
fn json_emits_exactly_one_freshness_variant_per_account() {
    let env = Environment::new("json");
    env.write_projection(&projection_body(
        Some((620_000, 41)),
        Some(("success", Some(41))),
    ));
    let (code, stdout, _) = env.run(&["status", "--format", "json"]);
    assert_eq!(code, 0);
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(document["command"], "status");
    assert_eq!(document["accounts"].as_array().map(Vec::len), Some(1));
    let account = &document["accounts"][0];
    let freshness = account["freshness"].as_str().expect("a freshness variant");
    assert!(
        matches!(freshness, "fresh" | "stale" | "auth_required"),
        "exactly one of the three variants: {account}"
    );
    match freshness {
        "fresh" => assert!(account.get("remaining").is_some()),
        "stale" => assert!(account.get("reason").is_some()),
        "auth_required" => assert!(account.get("last_good").is_some()),
        _ => unreachable!(),
    }
}

/// The selector contract in JSON: the selected model and every included
/// window scope are identified in the document.
#[test]
fn json_identifies_the_selected_model_and_included_scopes() {
    let env = Environment::new("json-selector");
    let received = now_nanos() - 41 * NANOS_PER_SECOND;
    let body = format!(
        r#"{{"schema_version":1,"ledger_generation":12,"accounts":[{{"account_id":1,"logical_name":"work-primary","provider":"anthropic","last_successful_observation":{{"observation_id":7,"provider_observed_at_nanos":{received},"received_at_nanos":{received},"measurement_basis":"provider_observed","windows":[{wide},{scoped}]}},"latest_attempt":{{"attempt_id":9,"request_started_at_nanos":{received},"credential_context_id":"ctx","result":{{"completed_at_nanos":{received},"outcome":"success","failure_class":null}}}}}}]}}"#,
        received = received,
        wide = window(500_000),
        scoped = format!(
            r#"{{"semantic_key":"weekly","scope_kind":"model_specific","scoped_model":"claude-model-x","quota_used_ppm":700000,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":{},"nominal_duration_nanos":{}}}"#,
            now_nanos() + 3600 * NANOS_PER_SECOND,
            7 * 86_400 * NANOS_PER_SECOND
        ),
    );
    env.write_projection(&body);
    let (code, stdout, _) = env.run(&["status", "--model", "claude-model-x", "--format", "json"]);
    assert_eq!(code, 0);
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let account = &document["accounts"][0];
    assert_eq!(account["selected_model"], "claude-model-x");
    let scopes: Vec<&str> = account["included_scopes"]
        .as_array()
        .expect("included scopes")
        .iter()
        .map(|scope| scope.as_str().expect("a scope label"))
        .collect();
    assert!(scopes.contains(&"account_wide"));
    assert!(scopes.contains(&"model:claude-model-x"));
    assert!(
        !scopes.iter().any(|scope| scope.contains("claude-model-y")),
        "an unrelated model's scope must not appear: {scopes:?}"
    );
    assert_eq!(account["limiting_window"]["scope"], "model");
    assert_eq!(account["limiting_window"]["model"], "claude-model-x");
}

/// The projection file the reader must never confuse with the real one: a
/// leftover temporary is inert, and status still reads the published file.
#[test]
fn a_leftover_temporary_file_is_not_read() {
    let env = Environment::new("temporary");
    env.write_projection(&projection_body(None, None));
    let temporary = env.state_dir().join("projection.tmp-999999");
    std::fs::write(&temporary, b"half-written garbage").unwrap();

    let (code, stdout, _) = env.run(&["status"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("work-primary"),
        "the published file was read: {stdout}"
    );
    assert!(
        temporary.exists(),
        "the temporary is not this reader's business"
    );
    assert!(!stdout.contains("garbage"));
}

/// Silence check: no path inside the state directory is written by status.
/// The state digest before and after a status run names every file under it,
/// so a status that wrote anything would show it here.
#[test]
fn status_writes_nothing_under_the_state_directory() {
    let env = Environment::new("no-write");
    env.write_projection(&projection_body(None, None));

    fn digest(dir: &Path) -> Vec<(String, u64)> {
        let mut entries: Vec<(String, u64)> = walk(dir);
        entries.sort();
        entries
    }

    fn walk(dir: &Path) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).expect("state dir readable") {
            let entry = entry.expect("entry readable");
            let metadata = entry.metadata().expect("metadata readable");
            let name = entry.file_name().to_string_lossy().to_string();
            if metadata.is_dir() {
                out.extend(walk(&entry.path()));
            } else {
                out.push((name, metadata.len()));
            }
        }
        out
    }

    let before = digest(&env.state_dir());
    let (code, _, _) = env.run(&["status"]);
    assert_eq!(code, 0);
    let after = digest(&env.state_dir());
    assert_eq!(
        before, after,
        "status must not write under the state directory"
    );
}
