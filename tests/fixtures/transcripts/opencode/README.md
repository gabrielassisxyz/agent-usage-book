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
