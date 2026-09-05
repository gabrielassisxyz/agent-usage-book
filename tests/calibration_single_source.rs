//! Integration tests for `aub-c0b.13`: a single stored calibration record is the
//! only source both `calibrate show` and the calibrated spend-to-window
//! conversion (`aub spend --window-equivalent`) read from, and the same holds
//! for the active cost model both `calibrate show` and plain `spend --credits`
//! read from (PLAN.md 3.3, 24, 34.20).
//!
//! An absence check (no literal in source) cannot prove consumers resolve one
//! shared record: a consumer that copied a coefficient into a local constant
//! would pass that check and still be wrong. Each test here seeds one record
//! with a conspicuous synthetic value through the real store chain (the
//! `__calibration-fixture` / `__cost-model-fixture` hooks, the only production
//! path into these tables from outside the crate), reads both consumers
//! through the release binary, supersedes the record append-only, and reads
//! both consumers again with no source or configuration edit in between. A
//! consumer that cached or copied the value instead of resolving it through
//! the repository would keep reporting the superseded number.

use std::process::Command;

use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::time::{MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{self, AccessMode, PragmaPolicy};
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
};
use test_support::StateDir;

fn aub(state: &StateDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aub"));
    cmd.env("HOME", state.path().join("home"))
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off");
    cmd
}

/// Runs one `aub` invocation to completion and returns its captured stdout,
/// panicking with both streams on a non-zero exit so a failing step names
/// itself instead of surfacing as a confusing later assertion failure.
fn run(state: &StateDir, args: &[&str]) -> String {
    let output = aub(state)
        .args(args)
        .output()
        .expect("the aub binary must be spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "aub {args:?} must succeed, got {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    stdout
}

/// One transcript message: a session id, which token kind it reports, and how
/// many. Keeping every other kind at zero isolates each scenario's credits
/// contribution to one cost-model term.
struct Message {
    session_id: &'static str,
    kind: &'static str,
    count: u64,
}

fn usage_line(message: &Message) -> String {
    let fields = [
        (
            "input_tokens",
            if message.kind == "input" {
                message.count
            } else {
                0
            },
        ),
        (
            "output_tokens",
            if message.kind == "output" {
                message.count
            } else {
                0
            },
        ),
        (
            "cache_read_input_tokens",
            if message.kind == "cache_read" {
                message.count
            } else {
                0
            },
        ),
        (
            "cache_creation_input_tokens",
            if message.kind == "cache_write" {
                message.count
            } else {
                0
            },
        ),
    ];
    let usage = fields
        .iter()
        .map(|(name, value)| format!("\"{name}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-08-25T10:00:00.000Z\",\"sessionId\":\"{}\",\"message\":{{\"id\":\"m-{}\",\"model\":\"claude-3-5-sonnet\",\"usage\":{{{usage}}}}}}}\n",
        message.session_id, message.session_id
    )
}

/// Writes the config and a transcript carrying one message per given session,
/// then ingests it. Every session still needs its own account marker
/// (`mark_account`) before `spend` can attribute it.
fn seed_and_ingest(state: &StateDir, messages: &[Message]) {
    let corpus = state.path().join("transcripts/claude-code/project");
    std::fs::create_dir_all(&corpus).unwrap();
    let body = messages.iter().map(usage_line).collect::<Vec<_>>().concat();
    std::fs::write(corpus.join("sessions.jsonl"), body).unwrap();

    let config = format!(
        "state.dir = \"{}\"\n\n[[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\npattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n",
        state.path().display(),
        state.path().join("transcripts/claude-code").display(),
    );
    std::fs::write(state.path().join("aub.toml"), config).unwrap();

    run(state, &["ingest", "transcripts"]);
}

/// Marks `session_id` as belonging to account `work` at the strongest evidence
/// rank, through the real repository function rather than hand-written SQL
/// against a schema this file does not own.
fn mark_account(state: &StateDir, session_id: &str) {
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let conn = connection::open(
        &path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("the ingested ledger must already exist and open");
    insert_marker(
        &conn,
        &NewSessionAccountMarker {
            session_id: SessionId::new(
                SourceNamespace::new("claude-code"),
                NativeSessionId::new(session_id),
            ),
            observed_at: UtcTimestamp::parse_rfc3339("2026-08-25T09:00:00Z")
                .expect("the marker timestamp must parse"),
            source_ordering_key: None,
            logical_account: "work".to_string(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        },
    )
    .expect("the account marker must insert");
}

const SPEND_WINDOW_ARGS: [&str; 6] = [
    "--since",
    "2026-08-25",
    "--days",
    "1",
    "--group-by",
    "account",
];

/// The window-equivalent line for one calibrated account child, so a test can
/// assert on the exact interval and calibration id together rather than on
/// two separately matched substrings that could belong to different lines.
fn window_equivalent_line<'a>(spend_text: &'a str, account: &str) -> &'a str {
    let account_marker = format!("account={account}");
    let account_at = spend_text
        .find(&account_marker)
        .unwrap_or_else(|| panic!("account={account} must appear in:\n{spend_text}"));
    spend_text[account_at..]
        .lines()
        .find(|line| line.contains("window equivalent"))
        .unwrap_or_else(|| {
            panic!("no window-equivalent line after account={account} in:\n{spend_text}")
        })
}

/// The `- <kind> ...` cost-model coverage line for one token kind, so a test
/// can assert whether that kind reads `modeled` or `missing`.
fn cost_model_kind_line<'a>(show_text: &'a str, kind: &str) -> &'a str {
    show_text
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("- {kind}")))
        .unwrap_or_else(|| panic!("no cost-model coverage line for {kind} in:\n{show_text}"))
}

/// Criterion: one seeded calibration identifier appears in `calibrate show`
/// and calibrated spend-conversion provenance; append-only supersession moves
/// both Phase 8 calibration consumers in the same test run; no source or
/// configuration file changes between the two halves.
#[test]
fn calibration_supersession_moves_calibrate_show_and_spend_window_equivalent_together() {
    let state = StateDir::new();
    seed_and_ingest(
        &state,
        &[Message {
            session_id: "s-single-source-work",
            kind: "input",
            count: 1_000_000,
        }],
    );
    mark_account(&state, "s-single-source-work");
    run(&state, &["__cost-model-fixture", "complete"]);

    // First half: a conspicuous synthetic coefficient. 1,000,000 input tokens
    // at the complete model's 3 micros/token price 3,000,000 micros of
    // credits; divided by this coefficient that is exactly 10.0000 percentage
    // points, a value chosen for a clean division rather than realism.
    run(&state, &["__calibration-fixture", "five_hour", "30"]);

    let show_before = run(&state, &["calibrate", "show"]);
    assert!(
        show_before.contains("active window calibration five_hour-fixture-calibration\n"),
        "{show_before}"
    );
    assert!(
        show_before.contains("fitted:          30 micros/point"),
        "{show_before}"
    );

    let mut spend_before_args = vec!["spend"];
    spend_before_args.extend(SPEND_WINDOW_ARGS);
    spend_before_args.extend(["--window-equivalent", "five_hour", "--refresh", "never"]);
    let spend_before = run(&state, &spend_before_args);
    let line_before = window_equivalent_line(&spend_before, "work");
    assert!(spend_before.contains("3.00 credits"), "{spend_before}");
    assert!(
        line_before.contains("[10.0000, 10.0000] percentage points"),
        "{line_before}"
    );
    assert!(
        line_before.contains("calibration five_hour-fixture-calibration)"),
        "{line_before}"
    );

    // Second half: append-only supersession, no source or configuration edit.
    // The new coefficient is a third of the first, so the same 3,000,000
    // micros of credits now convert to exactly 30.0000 percentage points.
    run(&state, &["__calibration-fixture", "five_hour", "10"]);

    let show_after = run(&state, &["calibrate", "show"]);
    assert!(
        show_after.contains("active window calibration five_hour-fixture-calibration-1"),
        "{show_after}"
    );
    assert!(
        show_after.contains("fitted:          10 micros/point"),
        "{show_after}"
    );
    assert!(
        !show_after.contains("active window calibration five_hour-fixture-calibration\n"),
        "the superseded record must no longer be the active one:\n{show_after}"
    );

    let mut spend_after_args = vec!["spend"];
    spend_after_args.extend(SPEND_WINDOW_ARGS);
    spend_after_args.extend(["--window-equivalent", "five_hour", "--refresh", "never"]);
    let spend_after = run(&state, &spend_after_args);
    let line_after = window_equivalent_line(&spend_after, "work");
    assert!(
        line_after.contains("[30.0000, 30.0000] percentage points"),
        "{line_after}"
    );
    assert!(
        line_after.contains("calibration five_hour-fixture-calibration-1)"),
        "{line_after}"
    );

    // Both consumers named the successor and neither still named the
    // predecessor: the coefficient moved with the repository row, not with a
    // per-consumer copy.
    assert_ne!(line_before, line_after);
}

/// Criterion: the same structure proves all Phase 8 cost-model consumers move
/// together. `calibrate show`'s cost-model coverage and plain `spend
/// --credits`'s computed credits both trace the active cost model row.
///
/// The "before" half never has an active calibration: PLAN.md 23.8's
/// cache-write completeness rule refuses to activate any window calibration
/// whose referenced cost model is incomplete, regardless of workload, so a
/// calibration referencing the incomplete model does not exist to show. That
/// refusal is itself the single-source guarantee working as designed (a
/// second, independent consumer cannot be handed a stale coefficient by a
/// calibration that should never have activated), so `calibrate show`'s
/// "before" state is the honest empty one rather than a populated wrong-
/// coverage one. Supersession then both completes the cost model and, for
/// the first time, lets a calibration referencing it activate.
#[test]
fn cost_model_supersession_moves_calibrate_show_and_spend_credits_together() {
    let state = StateDir::new();
    seed_and_ingest(
        &state,
        &[Message {
            session_id: "s-single-source-cache-write",
            kind: "cache_write",
            count: 1_000_000,
        }],
    );
    mark_account(&state, "s-single-source-cache-write");

    // First half: the incomplete model has no cache_write term, so it prices
    // nothing of this workload, and no calibration can activate against it.
    run(&state, &["__cost-model-fixture", "incomplete"]);

    let show_before = run(&state, &["calibrate", "show"]);
    assert!(
        show_before.contains("no active calibration"),
        "{show_before}"
    );

    let mut spend_before_args = vec!["spend"];
    spend_before_args.extend(SPEND_WINDOW_ARGS);
    spend_before_args.extend(["--credits", "--refresh", "never"]);
    let spend_before = run(&state, &spend_before_args);
    assert!(
        spend_before.contains("credits unavailable: cache_write rate"),
        "{spend_before}"
    );

    // Second half: append-only supersession to the complete model, which
    // covers the workload, followed by the calibration that can only activate
    // now that its referenced cost model is complete. No source or
    // configuration edit accompanies either step.
    run(&state, &["__cost-model-fixture", "complete"]);
    run(&state, &["__calibration-fixture", "five_hour", "1000000"]);

    let show_after = run(&state, &["calibrate", "show"]);
    assert!(
        show_after.contains("cost model:      anthropic-claude-messages-v1"),
        "{show_after}"
    );
    assert!(
        cost_model_kind_line(&show_after, "cache_write").contains("modeled"),
        "{show_after}"
    );

    let mut spend_after_args = vec!["spend"];
    spend_after_args.extend(SPEND_WINDOW_ARGS);
    spend_after_args.extend(["--credits", "--refresh", "never"]);
    let spend_after = run(&state, &spend_after_args);
    // 1,000,000 cache_write tokens at 3.75 micros/token now that the active
    // model carries that term.
    assert!(spend_after.contains("3.75 credits"), "{spend_after}");
    assert!(
        !spend_after.contains("credits unavailable"),
        "the account total must no longer refuse for the previously-missing term:\n{spend_after}"
    );
}
