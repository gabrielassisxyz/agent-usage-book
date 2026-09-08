//! The status-line tee: records the Claude Code status-line payload's
//! rate-limit windows per account, then hands the payload on untouched.
//!
//! Claude Code runs its `statusLine` command on every render and pipes one
//! JSON document to it. This module owns everything past the pass-through:
//! which account a payload is attributed to (the `SHALLOW_PROFILE` the caam
//! launcher set on the session, matched against the configured Anthropic
//! accounts), which windows the payload admits (any object under
//! `rate_limits` carrying a numeric `used_percentage`), the record line's
//! shape, and the per-session dedup that keeps renders from flooding the
//! file. The command in `cli.rs` owns stdin-to-stdout and swallows every
//! [`NoRecord`] reported here: a status line that breaks because aub's side
//! broke is the one outcome this module exists to prevent.
//!
//! May not depend on:
//! - provider adapters (attribution is by configured account name, never by
//!   a credential file, a `$HOME` guess or an endpoint)
//! - the store or the ledger: the record is append-only JSONL files, and
//!   reading them is a later bead's job

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use crate::config::Config;
use crate::domain::time::UtcTimestamp;

/// The only provider a status-line payload can be attributed to. The payload
/// is the Claude Code status line's own document, so an account on any other
/// provider can never have produced one; matching the provider here is what
/// keeps an identically named account of another provider from receiving
/// another account's meter readings.
const ANTHROPIC_PROVIDER: &str = "anthropic";

/// The directory under the state directory holding the per-account record
/// files (`<account>.jsonl`) and the per-session last-value files.
pub(crate) const RECORD_DIR_NAME: &str = "statusline";

/// Why one payload left no record. The command swallows all of these: the
/// payload still reached the renderer and the exit code is 0. But each
/// names the reason, so a test (or a debugging session) can tell "the
/// environment named no account" from "the state directory refused writes".
#[derive(Debug)]
pub(crate) enum NoRecord {
    /// No `SHALLOW_PROFILE` in the environment: nothing names the profile,
    /// so nothing names the account. Guessing one from `$HOME` or a
    /// credential file is the mistake this tee refuses to make; recording
    /// nothing is the honest alternative.
    NoProfile,
    /// `SHALLOW_PROFILE` named no configured `provider = "anthropic"` account.
    UnknownProfile {
        #[allow(dead_code)]
        // the name is for the reader of the value (tests, debugging); production swallows it whole
        name: String,
    },
    /// The matching account's name cannot serve as a record file name (a
    /// path separator in it would write outside the record directory).
    UnusableAccountName {
        #[allow(dead_code)]
        // the name is for the reader of the value (tests, debugging); production swallows it whole
        name: String,
    },
    /// The payload did not parse as one JSON object.
    PayloadNotAnObject,
    /// The payload named no session. The record file is a per-session meter
    /// log: without a session id there is nothing to dedup against and
    /// nothing for the reader to join a line to later, which is the same
    /// conclusion quota-ledger reached for the same file shape.
    NoSessionId,
    /// No window under `rate_limits` carried a numeric `used_percentage`.
    /// `rate_limits` is absent until the session's first API response, so an
    /// early render carries no reading at all.
    NoAdmittedWindow,
    /// A filesystem operation on aub's own side failed.
    Io(
        #[allow(dead_code)]
        // the error is for the reader of the value (tests, debugging); production swallows it whole
        std::io::Error,
    ),
}

/// One meter window as the dedup compares it: the percentage used and the
/// epoch second the window resets at, both normalized so a payload that
/// spells the same percentage as `40` and then `40.0` is one held value,
/// not a change.
#[derive(Debug, Clone, PartialEq)]
struct WindowReading {
    used_percentage: f64,
    resets_at: Option<i64>,
}

/// Records the payload's meter windows for `profile`'s account, appending one
/// line to `<state.dir>/statusline/<account>.jsonl` when the windows moved for
/// the session. `payload` is the raw bytes the command already passed through;
/// the tee never writes them anywhere.
///
/// Every failure is reported as [`NoRecord`] rather than an error class,
/// because the caller's only response to any of them is to swallow it and
/// leave the status line working.
pub(crate) fn record(
    config: &Config,
    profile: Option<&str>,
    payload: &[u8],
    received_at: UtcTimestamp,
) -> Result<(), NoRecord> {
    let Some(profile) = profile else {
        return Err(NoRecord::NoProfile);
    };
    // The name must match exactly. A case-folded or prefix match would
    // attribute a render to an account the environment did not name, and a
    // wrong attribution is the one defect this tee must never produce.
    let Some(account) = config
        .accounts
        .iter()
        .find(|account| account.name == profile && account.provider == ANTHROPIC_PROVIDER)
    else {
        return Err(NoRecord::UnknownProfile {
            name: profile.to_string(),
        });
    };
    let Some(stem) = single_component_stem(&account.name) else {
        return Err(NoRecord::UnusableAccountName {
            name: account.name.clone(),
        });
    };

    let parsed: Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(_) => return Err(NoRecord::PayloadNotAnObject),
    };
    let Some(payload_object) = parsed.as_object() else {
        return Err(NoRecord::PayloadNotAnObject);
    };
    // `session_id` is the payload's own spelling; `sessionId` is the
    // alternate Claude Code has used. An empty string is no session.
    let session_id = payload_object
        .get("session_id")
        .or_else(|| payload_object.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty());
    let Some(session_id) = session_id else {
        return Err(NoRecord::NoSessionId);
    };
    let cwd = payload_object
        .get("cwd")
        .or_else(|| {
            payload_object
                .get("workspace")
                .and_then(|workspace| workspace.get("current_dir"))
        })
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty());

    let windows = admitted_windows(payload_object)?;
    if windows.is_empty() {
        return Err(NoRecord::NoAdmittedWindow);
    }

    let record_dir = config.state.dir.join(RECORD_DIR_NAME);
    fs::create_dir_all(&record_dir).map_err(NoRecord::Io)?;

    let last_path = record_dir.join(format!("session-{}.last", file_stem(session_id)));
    if !windows_moved_since(&last_path, &windows) {
        return Ok(());
    }

    let line = record_line(received_at, session_id, cwd, &windows);
    let record_path = record_dir.join(format!("{stem}.jsonl"));
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&record_path)
        .map_err(NoRecord::Io)?;
    use std::io::Write as _;
    file.write_all(line.as_bytes()).map_err(NoRecord::Io)?;
    // The last value is written after the record: a run that dies between the
    // two writes loses dedup state, and the next render appends a duplicate
    // line with identical meter values, which a reader can collapse. A run
    // that wrote the last value first could lose the only record of a meter
    // change instead, which is the worse direction to fail in.
    fs::write(&last_path, Value::Object(windows.clone()).to_string()).map_err(NoRecord::Io)?;
    Ok(())
}

/// Extracts the windows the payload admits, as the record line renders them.
///
/// Admission is by shape, not by name: any object under `rate_limits`
/// carrying a numeric `used_percentage` is a meter window, so a model-scoped
/// window appears in the file without a release. The map's keys are sorted,
/// which is what makes the dedup comparison order-independent.
fn admitted_windows(payload: &Map<String, Value>) -> Result<Map<String, Value>, NoRecord> {
    let Some(rate_limits) = payload.get("rate_limits").and_then(Value::as_object) else {
        return Ok(Map::new());
    };
    let mut windows = Map::new();
    for (name, entry) in rate_limits {
        let Some(window) = admitted_window(entry) else {
            continue;
        };
        windows.insert(name.clone(), window);
    }
    Ok(windows)
}

/// Renders one window object as the record keeps it, or `None` when the
/// object does not carry the reading's shape. `resets_at` arrives as an
/// epoch-second string; a number is accepted as the same quantity and
/// anything else records as null.
fn admitted_window(entry: &Value) -> Option<Value> {
    let object = entry.as_object()?;
    let used_percentage = object.get("used_percentage")?.as_number()?.clone();
    let resets_at = match object.get("resets_at") {
        Some(Value::String(text)) => text
            .parse::<i64>()
            .ok()
            .map(Value::from)
            .unwrap_or(Value::Null),
        Some(Value::Number(number)) => number.as_i64().map(Value::from).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let mut window = Map::new();
    window.insert(
        "used_percentage".to_string(),
        Value::Number(used_percentage.clone()),
    );
    window.insert("resets_at".to_string(), resets_at);
    Some(Value::Object(window))
}

/// The record line for one admitted reading: the tee's own receive instant,
/// the session, the working directory, and the windows. Nothing else from
/// the payload is kept, and the payload itself is never written.
fn record_line(
    received_at: UtcTimestamp,
    session_id: &str,
    cwd: Option<&str>,
    windows: &Map<String, Value>,
) -> String {
    let mut line = Map::new();
    line.insert(
        "received_at".to_string(),
        Value::String(rfc3339_utc_seconds(received_at)),
    );
    line.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    line.insert(
        "cwd".to_string(),
        cwd.map_or(Value::Null, |cwd| Value::String(cwd.to_string())),
    );
    line.insert("windows".to_string(), Value::Object(windows.clone()));
    let mut text = Value::Object(line).to_string();
    text.push('\n');
    text
}

/// True when the windows differ from the session's last recorded value, which
/// is the only condition that earns a line. An unreadable or unparseable last
/// file counts as no last value: losing dedup state records a duplicate, not
/// a gap.
fn windows_moved_since(last_path: &Path, windows: &Map<String, Value>) -> bool {
    let last = match fs::read_to_string(last_path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => readings_of(&value),
            Err(_) => None,
        },
        Err(_) => None,
    };
    last != readings_of(&Value::Object(windows.clone()))
}

/// Reads a recorded `windows` object back into the comparable form. A value
/// that is not an object of window readings reads as nothing, so a corrupted
/// last file can never suppress a real change.
fn readings_of(value: &Value) -> Option<BTreeMap<String, WindowReading>> {
    let object = value.as_object()?;
    let mut readings = BTreeMap::new();
    for (name, window) in object {
        let window_object = window.as_object()?;
        let used_percentage = window_object.get("used_percentage")?.as_f64()?;
        let resets_at = window_object.get("resets_at").and_then(Value::as_i64);
        readings.insert(
            name.clone(),
            WindowReading {
                used_percentage,
                resets_at,
            },
        );
    }
    Some(readings)
}

/// The session's last-value file stem: the session id reduced to characters a
/// file name can carry. Real session ids are UUIDs and pass through
/// unchanged; a payload that put a path in `session_id` must not point the
/// last-value file outside the record directory, so every other character
/// collapses to `_` and the stem is capped. Two capped ids that share a
/// prefix would share a dedup state, which suppresses a line at worst.
fn file_stem(session_id: &str) -> String {
    let sanitized: String = session_id
        .chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == "." || sanitized == ".." {
        "_".to_string()
    } else {
        sanitized
    }
}

/// The account name as a record file stem, or `None` when the name cannot be
/// one file-system component (a `/` in it would write outside the record
/// directory, and the config is the only thing standing between the tee and
/// that path).
fn single_component_stem(name: &str) -> Option<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        None
    } else {
        Some(name)
    }
}

/// The tee's own receive instant as RFC 3339 UTC at second precision
/// (`2026-09-08T02:34:56Z`). The calendar arithmetic is the crate's own
/// (`UtcDate::iso`), so no zone or month table is re-derived here.
fn rfc3339_utc_seconds(instant: UtcTimestamp) -> String {
    let total_seconds = instant.unix_nanos().div_euclid(1_000_000_000);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        seconds_of_day / 3_600,
        seconds_of_day % 3_600 / 60,
        seconds_of_day % 60,
    );
    format!(
        "{}T{hour:02}:{minute:02}:{second:02}Z",
        instant.utc_date().iso()
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::config::{Config, Overrides};
    use crate::domain::time::{Clock as _, FakeClock, UtcTimestamp};

    use super::NoRecord;

    /// A fixed instant the tests can name: 2026-09-08T02:34:56Z.
    const RECEIVED_AT: i64 = 1_788_834_896;

    fn received_at() -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(RECEIVED_AT * 1_000_000_000)
    }

    /// A config resolved from the crate's own resolver: one account of the
    /// given provider, and the state directory the test owns. Built through
    /// `config::resolve` rather than by hand, so the tee is exercised against
    /// the configuration shape the operator's file actually produces.
    fn config_with_account(state_dir: &std::path::Path, name: &str, provider: &str) -> Config {
        let toml = format!(
            "[state]\ndir = {state_dir:?}\n\n[[accounts]]\nname = {name:?}\nprovider = {provider:?}\ncredential = {{ kind = \"file\", path = \"scratch-credential.json\" }}\n"
        );
        let (config, _) = crate::config::resolve(
            &Overrides::new(),
            &crate::config::FakeEnv::new(),
            Some(&toml),
            "test.toml",
        )
        .expect("a minimal single-account config must resolve");
        config
    }

    /// The canonical fixture payload: two windows, a cost field the record
    /// must never carry, and the payload's own epoch-string form of
    /// `resets_at`. Loaded from the committed fixture, not rebuilt inline,
    /// so the unit tests and the integration tests pipe byte-identical
    /// payloads through the tee.
    fn payload_five_seven() -> Vec<u8> {
        include_bytes!("../tests/fixtures/statusline/payload-five-seven.json").to_vec()
    }

    /// The same session with `seven_day.used_percentage` moved: the payload
    /// the dedup must let through after the canonical one.
    fn payload_seven_day_moved() -> Vec<u8> {
        include_bytes!("../tests/fixtures/statusline/payload-seven-day-moved.json").to_vec()
    }

    /// A payload with a model-scoped window the shape rule admits under its
    /// own name.
    fn payload_extra_window() -> Vec<u8> {
        include_bytes!("../tests/fixtures/statusline/payload-extra-window.json").to_vec()
    }

    fn read_lines(state_dir: &std::path::Path, account: &str) -> Vec<String> {
        let path = state_dir
            .join(super::RECORD_DIR_NAME)
            .join(format!("{account}.jsonl"));
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_profiled_payload_is_recorded_once_with_its_windows() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        super::record(&config, Some("gmail"), &payload_five_seven(), clock.now())
            .expect("a profiled payload records");

        let lines = read_lines(state.path(), "gmail");
        assert_eq!(lines.len(), 1, "one payload, one line");
        let line: Value = serde_json::from_str(&lines[0]).expect("the line is one JSON object");

        assert_eq!(
            line.get("received_at"),
            Some(&json!("2026-09-08T02:34:56Z")),
            "received_at is the tee's own clock, RFC 3339 UTC"
        );
        assert_eq!(
            line.get("session_id"),
            Some(&json!("8cd9c60a-e10a-4d44-857e-6b2b931b4d9d"))
        );
        assert_eq!(line.get("cwd"), Some(&json!("/tmp/worktree/project")));

        let windows = line.get("windows").and_then(Value::as_object).unwrap();
        assert_eq!(windows.len(), 2, "both admitted windows are recorded");
        let five = &windows["five_hour"];
        assert_eq!(five["used_percentage"], json!(40));
        assert_eq!(
            five["resets_at"],
            json!(1_786_834_200),
            "resets_at is an integer epoch second, not the payload's string"
        );
        let seven = &windows["seven_day"];
        assert_eq!(seven["used_percentage"], json!(12));
        assert_eq!(seven["resets_at"], json!(1_786_920_000));
    }

    #[test]
    fn the_same_windows_for_a_session_record_nothing_and_a_moved_window_records_one() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        super::record(&config, Some("gmail"), &payload_five_seven(), clock.now())
            .expect("the first payload records");
        assert_eq!(read_lines(state.path(), "gmail").len(), 1);

        // Identical payload: the meter did not move, so no line.
        super::record(&config, Some("gmail"), &payload_five_seven(), clock.now())
            .expect("a repeated payload is suppressed, not a failure");
        assert_eq!(
            read_lines(state.path(), "gmail").len(),
            1,
            "a second identical payload appends nothing"
        );

        // seven_day moved: exactly one more line.
        let moved = payload_seven_day_moved();
        super::record(&config, Some("gmail"), &moved, clock.now()).expect("a moved window records");
        let lines = read_lines(state.path(), "gmail");
        assert_eq!(lines.len(), 2, "one line per meter change");
        let second: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(second["windows"]["seven_day"]["used_percentage"], json!(13));

        // And the moved state holds: the same payload again appends nothing.
        super::record(&config, Some("gmail"), &moved, clock.now()).expect("held state suppresses");
        assert_eq!(read_lines(state.path(), "gmail").len(), 2);
    }

    #[test]
    fn a_resets_that_moved_without_a_percentage_change_still_records() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let first = r#"
        {
          "session_id": "8cd9c60a-e10a-4d44-857e-6b2b931b4d9d",
          "rate_limits": {"five_hour": {"used_percentage": 0, "resets_at": "1786834200"}}
        }
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &first, clock.now()).expect("first records");

        // The window rolled over: same zero percentage, new anchor.
        let rolled = r#"
        {
          "session_id": "8cd9c60a-e10a-4d44-857e-6b2b931b4d9d",
          "rate_limits": {"five_hour": {"used_percentage": 0, "resets_at": "1787439000"}}
        }
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &rolled, clock.now())
            .expect("a moved anchor records");
        assert_eq!(
            read_lines(state.path(), "gmail").len(),
            2,
            "resets_at is part of the meter movement the dedup watches"
        );
    }

    #[test]
    fn no_profile_and_an_unknown_profile_record_nothing() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let result = super::record(&config, None, &payload_five_seven(), clock.now());
        assert!(
            matches!(result, Err(NoRecord::NoProfile)),
            "a payload with no SHALLOW_PROFILE is not recorded: {result:?}"
        );
        assert!(
            !state.path().join(super::RECORD_DIR_NAME).exists(),
            "no record directory is created without a profile"
        );

        let result = super::record(&config, Some("nobody"), &payload_five_seven(), clock.now());
        assert!(
            matches!(result, Err(NoRecord::UnknownProfile { .. })),
            "a profile matching no configured account is not recorded: {result:?}"
        );
        assert!(
            !state.path().join(super::RECORD_DIR_NAME).exists(),
            "an unknown profile creates no file"
        );
    }

    #[test]
    fn a_same_named_account_of_another_provider_is_not_attributed() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "codex");
        let clock = FakeClock::new(received_at());

        let result = super::record(&config, Some("gmail"), &payload_five_seven(), clock.now());
        assert!(
            matches!(result, Err(NoRecord::UnknownProfile { .. })),
            "attribution is to the configured anthropic account, never to a same-named account of another provider: {result:?}"
        );
        assert!(!state.path().join(super::RECORD_DIR_NAME).exists());
    }

    #[test]
    fn malformed_json_records_nothing() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let result = super::record(&config, Some("gmail"), b"{not json", clock.now());
        assert!(
            matches!(result, Err(NoRecord::PayloadNotAnObject)),
            "{result:?}"
        );
        assert!(
            !state.path().join(super::RECORD_DIR_NAME).exists(),
            "a malformed payload writes nothing"
        );

        // Valid JSON that is not an object is the same shape failure.
        let result = super::record(&config, Some("gmail"), b"[1,2,3]", clock.now());
        assert!(
            matches!(result, Err(NoRecord::PayloadNotAnObject)),
            "{result:?}"
        );
        assert!(!state.path().join(super::RECORD_DIR_NAME).exists());
    }

    #[test]
    fn an_unwritable_state_directory_reports_io_and_writes_nothing() {
        let state = test_support::StateDir::new();
        // The would-be state directory is a regular file, so no child of it
        // can be created: the shape an unwritable state directory takes here.
        std::fs::write(state.path().join("blocked"), "not a directory").unwrap();
        let config = config_with_account(&state.path().join("blocked"), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let result = super::record(&config, Some("gmail"), &payload_five_seven(), clock.now());
        assert!(matches!(result, Err(NoRecord::Io(_))), "{result:?}");
        assert!(
            !state
                .path()
                .join("blocked")
                .join(super::RECORD_DIR_NAME)
                .exists(),
            "nothing is written to an unwritable state directory"
        );
    }

    #[test]
    fn a_payload_without_a_session_or_without_windows_records_nothing() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let no_session = r#"
        {"cwd": "/tmp/x", "rate_limits": {"five_hour": {"used_percentage": 1}}}
        "#
        .to_string()
        .into_bytes();
        let result = super::record(&config, Some("gmail"), &no_session, clock.now());
        assert!(matches!(result, Err(NoRecord::NoSessionId)), "{result:?}");
        assert!(!state.path().join(super::RECORD_DIR_NAME).exists());

        let no_windows = r#"{"session_id": "s1", "cwd": "/tmp/x"}"#;
        let result = super::record(&config, Some("gmail"), no_windows.as_bytes(), clock.now());
        assert!(
            matches!(result, Err(NoRecord::NoAdmittedWindow)),
            "{result:?}"
        );
        assert!(!state.path().join(super::RECORD_DIR_NAME).exists());

        // rate_limits present but carrying no window-shaped object.
        let shapeless = r#"
        {"session_id": "s1", "rate_limits": {"five_hour": 40, "other": {"no_percentage": true}}}
        "#
        .to_string()
        .into_bytes();
        let result = super::record(&config, Some("gmail"), &shapeless, clock.now());
        assert!(
            matches!(result, Err(NoRecord::NoAdmittedWindow)),
            "{result:?}"
        );
        assert!(!state.path().join(super::RECORD_DIR_NAME).exists());
    }

    #[test]
    fn a_window_of_any_name_is_recorded_under_its_own_name() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let payload = payload_extra_window();
        super::record(&config, Some("gmail"), &payload, clock.now())
            .expect("a shape-admitted window records");

        let lines = read_lines(state.path(), "gmail");
        assert_eq!(lines.len(), 1);
        let line: Value = serde_json::from_str(&lines[0]).unwrap();
        let opus = &line["windows"]["seven_day_opus"];
        assert_eq!(opus["used_percentage"], json!(9));
        assert_eq!(opus["resets_at"], json!(1_786_920_001));
    }

    #[test]
    fn the_recorded_line_never_carries_the_payloads_other_fields() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        super::record(&config, Some("gmail"), &payload_five_seven(), clock.now())
            .expect("the payload records");
        let line = &read_lines(state.path(), "gmail")[0];
        // The fixture's cost field, which the payload carries and the record
        // must not: grepping for the value catches a field copied by value or
        // by accident, which checking only the key set would miss.
        assert!(
            !line.contains("12.345678"),
            "the cost value leaked into the record: {line}"
        );
        assert!(!line.contains("cost"), "the cost key leaked: {line}");
        assert!(
            !line.contains("total_cost_usd"),
            "the cost key leaked: {line}"
        );
    }

    #[test]
    fn the_camel_case_session_and_the_workspace_cwd_are_read() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let payload = r#"
        {
          "sessionId": "8cd9c60a-e10a-4d44-857e-6b2b931b4d9d",
          "workspace": {"current_dir": "/tmp/elsewhere"},
          "rate_limits": {"five_hour": {"used_percentage": 5}}
        }
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &payload, clock.now()).expect("records");

        let line: Value = serde_json::from_str(&read_lines(state.path(), "gmail")[0]).unwrap();
        assert_eq!(
            line["session_id"],
            json!("8cd9c60a-e10a-4d44-857e-6b2b931b4d9d")
        );
        assert_eq!(line["cwd"], json!("/tmp/elsewhere"));
    }

    #[test]
    fn a_window_without_resets_at_records_null_and_still_dedups() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let payload = r#"
        {"session_id": "s1", "rate_limits": {"five_hour": {"used_percentage": 7}}}
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &payload, clock.now()).expect("records");
        let line: Value = serde_json::from_str(&read_lines(state.path(), "gmail")[0]).unwrap();
        assert_eq!(
            line["windows"]["five_hour"]["resets_at"],
            Value::Null,
            "a missing resets_at is null, never a guessed instant"
        );

        super::record(&config, Some("gmail"), &payload, clock.now()).expect("suppressed");
        assert_eq!(read_lines(state.path(), "gmail").len(), 1);
    }

    #[test]
    fn a_corrupted_last_file_cannot_suppress_a_change() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        super::record(&config, Some("gmail"), &payload_five_seven(), clock.now())
            .expect("first records");
        let last_path = state
            .path()
            .join(super::RECORD_DIR_NAME)
            .join("session-8cd9c60a-e10a-4d44-857e-6b2b931b4d9d.last");
        std::fs::write(&last_path, "not json at all").unwrap();

        super::record(&config, Some("gmail"), &payload_five_seven(), clock.now())
            .expect("records against a corrupted last file");
        assert_eq!(
            read_lines(state.path(), "gmail").len(),
            2,
            "a corrupted last value reads as no last value"
        );
    }

    #[test]
    fn a_session_id_that_is_not_a_file_name_cannot_leave_the_directory() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let payload = r#"
        {"session_id": "../up/over", "rate_limits": {"five_hour": {"used_percentage": 3}}}
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &payload, clock.now())
            .expect("the session id is recorded; only its last-value stem is sanitized");

        let dir_path = state.path().join(super::RECORD_DIR_NAME);
        let mut names: Vec<String> = std::fs::read_dir(&dir_path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "gmail.jsonl".to_string(),
                "session-.._up_over.last".to_string(),
            ],
            "the last-value file stays inside the record directory: {names:?}"
        );
        assert!(!state.path().join("up").exists());
    }

    #[test]
    fn received_at_renders_rfc3339_utc_at_second_precision() {
        assert_eq!(
            super::rfc3339_utc_seconds(received_at()),
            "2026-09-08T02:34:56Z"
        );
        // Sub-second input truncates: the receive stamp is a label, not a
        // quantity.
        let fractional = UtcTimestamp::from_unix_nanos(RECEIVED_AT * 1_000_000_000 + 999_999_999);
        assert_eq!(
            super::rfc3339_utc_seconds(fractional),
            "2026-09-08T02:34:56Z"
        );
        // A pre-epoch instant still renders a nameable UTC instant.
        assert_eq!(
            super::rfc3339_utc_seconds(UtcTimestamp::from_unix_nanos(-1)),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn an_account_name_that_cannot_be_a_file_stem_is_refused() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "escape/../valve", "anthropic");
        let clock = FakeClock::new(received_at());

        let result = super::record(
            &config,
            Some("escape/../valve"),
            &payload_five_seven(),
            clock.now(),
        );
        assert!(
            matches!(result, Err(NoRecord::UnusableAccountName { .. })),
            "an account name with a separator never becomes a record path: {result:?}"
        );
        assert!(!state.path().join(super::RECORD_DIR_NAME).exists());
    }

    #[test]
    fn a_percentage_spelled_differently_is_one_held_value() {
        let state = test_support::StateDir::new();
        let config = config_with_account(state.path(), "gmail", "anthropic");
        let clock = FakeClock::new(received_at());

        let as_integer = r#"
        {"session_id": "s1", "rate_limits": {"five_hour": {"used_percentage": 40, "resets_at": "1786834200"}}}
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &as_integer, clock.now()).expect("records");

        let as_float = r#"
        {"session_id": "s1", "rate_limits": {"five_hour": {"used_percentage": 40.0, "resets_at": "1786834200"}}}
        "#
        .to_string()
        .into_bytes();
        super::record(&config, Some("gmail"), &as_float, clock.now())
            .expect("suppressed, not a failure");
        assert_eq!(
            read_lines(state.path(), "gmail").len(),
            1,
            "40 and 40.0 are the same percentage held, not a meter change"
        );
    }
}
