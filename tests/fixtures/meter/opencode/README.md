# OpenCode Go Meter Fixtures

Synthetic page fixtures for the OpenCode Go workspace-page meter adapter
(`aub-8hu3`). The OpenCode Go usage meter has no public endpoint: the
authoritative surface is the workspace page `https://opencode.ai/workspace/<id>/go`
as seen in a signed-in browser, and the usage meters live in the page's
embedded initial state script (the reference,
`https://ai.rud.is/posts/2026-06-06-opencode-go-usage/`, documents the field
names: per window `percent` (integer 0..100) and `reset_in_sec`, for
`rolling`, `weekly` and `monthly`, plus `plan` and `fetched_at`).

## Provenance

Every fixture here is **reconstructed, not captured**: the session cookie was
not available on the machine that wrote them, so no live page could be
fetched. Each file states its own synthetic status in its visible text. No
cookie, token, key, email, session identifier or account identifier appears
in any of them, and each parses clean against the shared forbidden-pattern
list (`docs/forbidden-patterns.txt`). A sanitized live capture joins them as
its own fixture when the reviewer's cookie exists.

## Parser contract

The adapter keys the page-state script by the literal marker string
`reset_in_sec`: a `<script>` element whose text contains it carries the store
state, and the parser extracts the brace-balanced JSON object from that
element's text. The marker is one of the exact field names the reference
documents for the store state, and the only one distinctive enough not to
appear elsewhere in the component tree's markup.

## Catalog of Fixtures

- `valid.html`: the three usage windows in the embedded state script, plus
  `plan` and `fetched_at`; parses to three `MeterWindow` rows.
- `login-redirect.html`: the sign-in body the workspace request is redirected
  to when the session cookie is invalid or expired; the adapter classifies
  the redirect response itself, so this body pairs with a 302 status in the
  synthetic server.
- `no-state-marker.html`: the page shape without any state script; parses to
  `FailureClass::SchemaDrift`, never a silent zero.
- `malformed-state.html`: the state script present but its JSON truncated;
  parses to `FailureClass::MalformedBody`.