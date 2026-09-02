# Rate book fixture

`rates.toml` is the initial dated rate book this project imports (PLAN.md
section 32): the API price table the pre-existing hardcoded estimator carried,
with its date comments become structured metadata — effective intervals and
publication references.

- Anthropic rows were read 2026-06-24 from the claude-api reference.
- OpenAI rows were read 2026-08-14 from `bunx tokscale pricing` (LiteLLM).
- The claude-sonnet-5 introductory rows expire 2026-08-31; their `review_due`
  is that expiry date.

The file is import source, not a runtime witness: `aub rate-card import`
persists it as immutable records, and valuation reads the database records,
never this file. A vendor price change is a new dated edition of this file,
imported alongside the old one, never an edit in place — an edit in place would
silently revalue historical traffic.