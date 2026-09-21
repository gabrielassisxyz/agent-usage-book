# opencode · fixture-project · ses_m76e_fixture

*Started: 2026-09-01T00:01:40.000000000Z*

## User
Render the usage summary for review.

## Assistant
I will read the ledger first.
**Tool: read**
```json
{
  "limit": 40,
  "offset": 0,
  "path": "/work/fixture-project/summary.md"
}
```
**Tool result:**
# Usage summary

Week 38 opened with three providers reporting against the same window, and the ledger joined them into one number per model.

- claude-code: 120400 input, 34210 output across 41 sessions
- codex: 88210 input, 12980 output across 17 sessions
- pi: 41020 input, 8030 output across 9 sessions

The join key is the canonical event id, so replays collapse instead of doubling. Cache reads dominate the input side on every provider, which is why the summary leads with them.

Trailing line to prove the output survives whole: 0123456789abcdef.

## Assistant
The summary is ready for review.
