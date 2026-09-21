# opencode transcript fixture

opencode keeps every session in one SQLite database (`opencode.db`) rather
than the line-delimited transcript files the other sources write, so its
fixture is a seed file, not a transcript: `seed.json` holds invented session
and message rows in the shape the real `session` and `message` tables carry
(`message.data` with `role`, `modelID`, `providerID`, `mode`, `cost`,
`finish`, `time.created/completed` and
`tokens.input/output/reasoning/cache/total`).

No value here came from a real database. Tests build a scratch `opencode.db`
from this seed at runtime, in a temporary directory, and parse that; the real
database is never copied into the repository and never read by a test. The
seed covers one parent session with two assistant messages and one user
message, one child session (`parent_id` set) with one assistant message, and
one assistant message without `tokens`, which must be skipped with a count
rather than crash the parse.

`transcript_seed.json` is the sibling seed behind `aub export transcript`
(`aub-m76e`): one session with three messages (a user turn, an assistant turn
with a finished tool part, an assistant turn with a reasoning part) and the
recorded `part` table shape. Its part `type` values (`text`, `tool` with
`state.input`/`state.output`, `reasoning` with readable `text`, plus the
`step-start`/`step-finish` run markers) were read off the live database on
2026-09-21; every id, timestamp, prose string and tool payload in the file is
invented. `plain.md`, `tools.md` and `thinking.md` pin its rendering under
each flag combination, the same golden layout the codex and pi fixtures use.
