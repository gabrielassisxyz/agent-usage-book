# OpenCode Go Meter Fixtures

Synthetic page fixtures for the OpenCode Go workspace-page meter adapter
(`aub-8hu3`). The OpenCode Go usage meter has no public endpoint: the
authoritative surface is the workspace page
`https://opencode.ai/workspace/<id>/go` as seen in a signed-in browser, and
the usage meters live in the page's **rendered markup**, not in an embedded
JSON blob. The real contract comes from the reference tool
(`git.sr.ht/~hrbrmstr/opencode-go-usage`, branch `batman`, `usage/usage.go`
and `usage/usage_test.go`), which parses the same HTML a browser renders.

## Provenance

`valid.html` and `malformed-state.html` reproduce the shape of the reference
tool's own committed sample page (`usage/usage_test.go`'s fixture): three
`usage-item` blocks with figures 0 / 35.5 / 64.8 percent and resets `5 hours
0 minutes` / `4 days 10 hours` / `7 days 5 hours`. This is the tool's own
test fixture, not a captured live page. No sanitized live capture is present
here: the session cookie was not available on the machine that wrote these
fixtures, so no live workspace page could be fetched. No cookie, token, key,
email, session identifier or account identifier appears in any of them, and
each parses clean against the shared forbidden-pattern list
(`docs/forbidden-patterns.txt`).

## Parser contract

The adapter keys the page on the literal marker string
`data-slot="usage-item"`: one `<div>` carrying that attribute per usage
window. Inside it: `<span data-slot="usage-label">` names the window
(`5-hour Usage`, `Weekly Usage`, `Monthly Usage`, matched by substring to
`rolling`, `weekly`, `monthly`); a `role="progressbar"` element's
`aria-valuenow` attribute carries the percent as a decimal with one place;
and `<span data-slot="reset-time">` carries `Resets in <N days> <N hours>
<N minutes> <N seconds>` (any subset of those units), with React comment
markers (`<!--$-->`, `<!--/-->`) stripped before parsing. A page with no
`data-slot="usage-item"` element anywhere is schema drift.

## Catalog of Fixtures

- `valid.html`: the three usage windows in the rendered markup; parses to
  three `MeterWindow` rows at 0 / 35.5 / 64.8 percent.
- `login-redirect.html`: the sign-in body the workspace request is redirected
  to when the session cookie is invalid or expired; the adapter classifies
  the redirect response itself, so this body pairs with a 302 status in the
  synthetic server and its content is never parsed.
- `no-state-marker.html`: the page shape with no `usage-item` element at
  all; parses to `FailureClass::SchemaDrift`, never a silent zero.
- `malformed-state.html`: three items present, but the weekly item's
  `aria-valuenow` is the text `not-a-number`; parses to
  `FailureClass::MalformedBody`, with the sanitized raw-percent/reset-text
  evidence still retained since the markup itself read structurally fine.
