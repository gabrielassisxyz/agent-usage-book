//! The per-account selection block of status JSON (`aub-vkv7`).
//!
//! An orchestrator ranking accounts reads one number instead of re-deriving
//! it from windows. This module builds that block from the account's stored
//! windows at the report instant: the spend scalar beside the runway and
//! confidence that qualify it. It is always present; the scalar and the
//! usable seconds travel only when the signal justifies them.

use super::json::json_string;
use crate::domain::time::UtcTimestamp;

/// Close one status account object: append its selection block and render.
/// The tail lives here beside the block so `json.rs` stays under its size
/// ceiling; the field set is unchanged either way.
pub(crate) fn close_account_object(
    mut fields: Vec<String>,
    account: &crate::report::MeterAccount,
    now: UtcTimestamp,
) -> String {
    fields.push(selection_fragment(account, now));
    format!("{{{}}}", fields.join(","))
}

/// One `accounts[].selection` value for the account: the runway and
/// confidence labels with the spend scalar beside them when a window was
/// measurable, and the usable runway seconds when the runway is finite. The
/// scalar is absent, never zero or null, when unmeasurable, so a consumer
/// never ranks on a placeholder.
fn selection_fragment(account: &crate::report::MeterAccount, now: UtcTimestamp) -> String {
    let windows = account
        .windows
        .iter()
        .map(|window| {
            crate::domain::selection::SelectionWindow::new(
                window.quota_used.as_ppm().get(),
                window.reset_state,
                window.nominal_duration,
                window.rate,
            )
        })
        .collect::<Vec<_>>();
    let selection = crate::domain::selection::compute_selection(&windows, &account.reading, now);
    let mut fields = vec![
        format!("\"runway\":{}", json_string(selection.runway.as_str())),
        format!(
            "\"confidence\":{}",
            json_string(selection.confidence.as_str())
        ),
    ];
    if let Some(priority) = selection.spend_priority {
        fields.push(format!("\"spend_priority\":{priority}"));
    }
    if let Some(secs) = selection.usable_runway_secs {
        fields.push(format!("\"usable_runway_secs\":{secs}"));
    }
    format!("\"selection\":{{{}}}", fields.join(","))
}
