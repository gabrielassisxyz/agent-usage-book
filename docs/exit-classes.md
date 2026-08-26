# Exit classes

The process exit code is a scripting contract, not a convention. Automation
must not parse prose to learn that a remote source needed authentication, so
the classes are stable and documented here. The table is checked against the
`ExitClass` enum by a test: adding a variant without a row here fails the
build.

| Code | Class | Meaning |
|---:|---|---|
| 0 | Success | Command completed for its contract |
| 1 | Internal | Unexpected internal failure |
| 2 | Usage | Configuration, argument or environment invalid |
| 3 | AuthRequired | Requested live source requires authentication |
| 4 | RemoteUnavailable | Requested live or remote source unavailable |
| 5 | Store | Store or durable-state failure |
| 6 | InsufficientEvidence | Insufficient evidence for a requested quantitative result |
| 7 | ThresholdNotMet | Explicit threshold or advisory result not met |
| 8 | IngestIncomplete | Local ingest or report incomplete because required source material could not be normalized |

Class 4 is strictly about a live or remote source; class 8 is strictly about
local material. A single failure is exactly one class, never both.

## Special cases

These are part of the contract rather than exceptions to it.

* `status` returns 0 after a successful invocation even when it displays
  stale, auth-required or no-data conditions, and for a corrupt projection; it
  returns non-zero only for argument parsing failure. Status bars treat a
  non-zero exit as process failure and suppress the degraded output that was
  the point.
* Scheduled `sample --due` returns 0 when every attempt outcome was durably
  recorded, including remote auth and transport failures; it returns non-zero
  when evidence could not be durably preserved.
* `sample --require-success` records the same evidence and then reports remote
  failures through their ordinary live-source classes.
