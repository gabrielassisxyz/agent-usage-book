//! Integration: the credential context identifier is stable across a re-read
//! of the same credential file and different after a credential replacement,
//! against the real filesystem. The unit tests in src/auth.rs prove the
//! derivation logic with an injected filesystem; this test proves the real
//! filesystem's modification time actually behaves the way the derivation
//! assumes (changes when the file is rewritten).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_usage_book::auth::{RealFs, resolve};
use agent_usage_book::config::{AccountConfig, AccountExclusivityPolicy};

fn account(path: &str) -> AccountConfig {
    AccountConfig {
        name: "primary".to_string(),
        provider: "anthropic".to_string(),
        credential_kind: "file".to_string(),
        credential_detail: path.to_string(),
        exclusivity_policy: AccountExclusivityPolicy::ForbidPassive,
    }
}

fn unique_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aub-eun1-{}-{nanos}.json", std::process::id()))
}

#[test]
fn context_id_is_stable_across_re_reads_and_changes_after_replacement() {
    let path = unique_path();
    let path_str = path.to_str().expect("temp path must be valid UTF-8");

    std::fs::write(&path, "token-a").expect("test credential file must be writable");
    let first = resolve(&account(path_str), &RealFs, false).expect("first resolution must succeed");
    let second =
        resolve(&account(path_str), &RealFs, false).expect("second resolution must succeed");
    assert_eq!(
        first.context_id, second.context_id,
        "a re-read of the same file must not change the context id"
    );

    std::fs::write(&path, "token-b").expect("test credential file must be rewritable");
    let replaced = resolve(&account(path_str), &RealFs, false)
        .expect("resolution after replacement must succeed");
    assert_ne!(
        first.context_id, replaced.context_id,
        "a credential replacement must change the context id"
    );

    let _ = std::fs::remove_file(&path);
}
