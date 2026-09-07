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

`valid.html` is a sanitized capture of the live workspace page, fetched on
2026-09-07 with the operator's session cookie while the account was being
configured (`aub-s6e4`). The workspace id, the account email, the Stripe
customer, payment-method and subscription ids, the referral code and the
server-action ids were replaced with zeros or neutral words; nothing else was
changed, so the fixture carries the page's real hydration script, header,
navigation and the collapsed "Show details" blocks beside the weekly and
monthly items. It parses to 0 / 1.1 / 2.1 percent with resets `5 hours 0
minutes` / `6 days 8 hours` / `27 days 8 hours`. The reset text is
hour-granular on the real page while the hydration script carries the exact
`resetInSec`; the parser reads the markup, as the reference tool does.

`malformed-state.html` reproduces the shape of the reference tool's own
committed sample page (`usage/usage_test.go`'s fixture): three `usage-item`
blocks with figures 0 / 35.5 / 64.8 percent and resets `5 hours 0 minutes` /
`4 days 10 hours` / `7 days 5 hours`, with the weekly percent broken on
purpose. Before the live capture, `valid.html` was that same shape; the
reference fixture shape survives here so a page the tool itself parses still
has a case. No cookie, token, key, email, session identifier or account
identifier appears in any of them, and each parses clean against the shared
forbidden-pattern list (`docs/forbidden-patterns.txt`).

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

- `valid.html`: the sanitized live page, three usage windows in the rendered
  markup; parses to three `MeterWindow` rows at 0 / 1.1 / 2.1 percent.
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
