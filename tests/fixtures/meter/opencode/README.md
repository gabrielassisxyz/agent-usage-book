# OpenCode Go Meter Fixtures

Synthetic fixtures for the OpenCode Go meter adapter (`aub-8hu3`, migrated by
`aub-id41`). The adapter reads the console's own status endpoint,
`GET https://opencode.ai/console/api/go/status`, which answers JSON for a
signed-in browser session. The real contract comes from the reference tool
(`git.sr.ht/~hrbrmstr/opencode-go-usage`, `usage/usage.go`), whose whole
history is one commit titled `feat: migrate to console JSON API from HTML
scraping`.

`login-redirect.html` is the one fixture left from the revision this adapter
replaced, when the authoritative surface was the rendered workspace page. It
survives because the redirect case still uses it; the three page-parsing
fixtures beside it were removed with the parser they fed.

## Provenance

`valid.json` is the live response captured on 2026-09-20 with the operator's
session cookies, with the subscriber id and the payment-method id replaced by
neutral fixture words. Every quota figure is the response's own: a $12.00
five-hour ceiling, $30.00 weekly and $60.00 monthly, against 0, 137731547 and
695777296 micro-cents spent. It carries the real response's own mixture of
reset instants, which is the reason it is worth keeping verbatim: `week`
states one, `fiveHour` states `null`, and `month` omits the key altogether.

`no-state-marker.json` and `malformed-state.json` are hand-written around that
shape rather than captured. No cookie, token, key, email, session identifier
or account identifier appears in any fixture, and each parses clean against
the shared forbidden-pattern list (`docs/forbidden-patterns.txt`).

## Parser contract

The adapter keys the response on `access.meters`: one object per usage window,
under the keys `fiveHour`, `week` and `month`, mapped to the semantic keys
`rolling`, `weekly` and `monthly`. Each carries `limitMicroCents` and
`usedMicroCents` as decimal strings, and `resetsAt` as an RFC 3339 instant,
as `null`, or not at all.

The quota fraction is the exact integer ratio of used to limit, rounded half
away from zero to parts per million; a window whose limit is zero states no
ceiling and refuses rather than reading as zero usage. A window with no reset
instant is the not-started state `aub-eun.15` decided on, and no reset is
inferred from `access.endsAt`. A body that is not JSON, one whose content type
is not JSON, and one carrying no `access.meters` object are all schema drift,
never a silent zero.

The projection the evidence capsule retains is narrower than the response: the
three windows and nothing else. The response also carries a subscriber id and
a payment-method id, and neither belongs in an evidence store that exists to
prove a quota number.

## Catalog of Fixtures

- `valid.json`: the sanitized live response, three usage windows with the
  provider's own micro-cent counts; parses to three `MeterWindow` rows at
  0 / 45911 / 115963 ppm, with only the weekly window stating a reset.
- `no-state-marker.json`: a well-formed body with no `access.meters` object;
  parses to `FailureClass::SchemaDrift`, never a silent zero.
- `malformed-state.json`: three meters present, but the five-hour meter's
  `usedMicroCents` is the text `not-a-number`; parses to
  `FailureClass::MalformedBody`, with the sanitized projection still retained
  since the response itself read structurally fine.
- `login-redirect.html`: the sign-in body the request is redirected to when
  the session is invalid or expired; the adapter classifies the redirect
  response itself, so this body pairs with a 302 status in the synthetic
  server and its content is never parsed.
