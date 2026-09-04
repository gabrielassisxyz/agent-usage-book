#!/usr/bin/env sh
# Session-start hook: records which account this session is running under, so
# spend recorded during the session attributes to that account instead of
# falling into the unknown-account bucket (docs/PLAN.md section 19.2). Wire
# your agent CLI's session-start hook, or a compositor launcher keybinding, to
# call this. Neither inherits an interactive shell's PATH, hence the absolute
# path below.
#
# ACCOUNT must name an account already configured under [[accounts]] in
# aub.toml. SESSION_ID and RUN_ID come from the launcher; the exact
# environment variable or argument they arrive as depends on which launcher
# is calling this. --run-id is optional: pass it only when this machine also
# tracks a separate friction ledger that shares the same run identifier.
#
# --if-due skips the network request when the account was recently sampled
# and still records the marker: the marker is durable evidence, not a poll
# result, and it is written before the due decision is even evaluated.
ACCOUNT="work-primary"
SESSION_ID="$1"
RUN_ID="${2:-}"

if [ -n "$RUN_ID" ]; then
    /usr/local/bin/aub sample --account "$ACCOUNT" --if-due --session-id "$SESSION_ID" --run-id "$RUN_ID"
else
    /usr/local/bin/aub sample --account "$ACCOUNT" --if-due --session-id "$SESSION_ID"
fi
