# Ollama Cloud Meter Fixture Corpus

Synthetic-shape response captures for the Ollama Cloud usage provider adapter (`aub-ud17`).

None of these fixtures come from a real account: the shape is documented by
`bin/ollama-quota` in llm-workflow (`API_BASE=https://ollama.com`, `GET /api/usage`) and its
own test suite (`scripts/ollama-quota-test.sh`), which fixes `usage` as a fraction in `[0, 1]`
of the window consumed (for example `0.163` renders as `16.3%`), not an integer percent. No
fixture here carries a credential, a session id, or an account identifier of any kind.

## Catalog of Fixtures

- `valid.json`: a healthy response with both required windows (`limits.session`,
  `limits.weekly`), reusing the same usage values `ollama-quota`'s own test suite verifies
  render as `16.3%` and `40.6%`.
- `zero-usage.json`: unused quota on both windows (`usage: 0`).
- `missing-weekly.json`: `limits.session` present, `limits.weekly` absent, the planted
  negative for the required-window check.
- `error-401.json`: a rejected-credential response body, used with an HTTP 401 status.
- `error-429.json`: a rate-limit response body carrying the provider's own
  `error.type` classification and message, used with an HTTP 429 status
  (aub-rfot).
- `malformed.json`: invalid, truncated JSON.
