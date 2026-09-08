# Status-line payload fixtures

One fixture per recorded-shape case the status-line tee's tests exercise.
Each file is a Claude Code `statusLine` payload as the status line command
receives it on stdin: a JSON object with `session_id` (or `sessionId`), `cwd`
(or `workspace.current_dir`), and a `rate_limits` object whose windows carry
`used_percentage` and an epoch-second `resets_at` string. Real payloads also
carry context fields the tee must never keep (`cost`, model ids, transcript
paths); `payload-five-seven.json` keeps a `cost` object so the tests can grep
the recorded line for its value and prove the tee kept nothing of it.

Capture procedure: the payloads here are hand-built to the documented shape,
not captured from a live session, so no machine, account or path from this
one is in them. The session id is a throwaway UUID, the cwd is under /tmp,
and the whole directory passes the shared sanitization scan
(`test_support::sanitization::matched_patterns`).

Every fixture carries `rate_limits` in the form the status line actually
sends (`resets_at` as an epoch-seconds string), because a fixture in a
theoretical form tests a payload Claude Code never sends.