# Problem codes

Exit codes are a coarse channel: nine of them and far more distinguishable
conditions. A symbolic problem code carries the detail without expanding the
exit taxonomy, and automation reads a name rather than parsing prose. The
codes are stable across releases, derived in one place from the failure,
stale-reason and report-qualification classifications, and from the error
taxonomy for the generic failures whose only payload is a human message. Every
code maps to exactly one exit class. The table is checked against the
`ProblemCode` enum by a test: renaming or removing a code, or adding one
without a row here, fails the build.

| Code | Class | Meaning |
|---|---|---|
| DNS_FAILURE | RemoteUnavailable | Name resolution for the provider endpoint failed |
| CONNECT_TIMEOUT | RemoteUnavailable | The connection to the provider timed out |
| READ_TIMEOUT | RemoteUnavailable | The provider did not answer within the read timeout |
| TOTAL_BUDGET_EXPIRED | RemoteUnavailable | The command's total execution budget expired across retries |
| HTTP_CLIENT_ERROR | RemoteUnavailable | The provider answered with a client-error status |
| HTTP_SERVER_ERROR | RemoteUnavailable | The provider answered with a server-error status |
| RATE_LIMITED | RemoteUnavailable | The provider rate-limited the request |
| MALFORMED_BODY | RemoteUnavailable | The provider body could not be parsed |
| MISSING_REQUIRED_FIELD | RemoteUnavailable | The provider body parsed but lacked a required field |
| CREDENTIAL_EXPIRED | AuthRequired | The configured credential is expired |
| CREDENTIAL_REJECTED | AuthRequired | The provider rejected the configured credential |
| PROVIDER_DECLARED_EXPIRY | AuthRequired | The provider declared the credential's authentication expired |
| AGE_EXCEEDED | Success | The reading is stale because it is older than the freshness horizon |
| NO_SUCCESSFUL_OBSERVATION | InsufficientEvidence | No successful observation exists to report |
| MALFORMED_PROVIDER_RESPONSE | RemoteUnavailable | A provider response was malformed, leaving the reading stale |
| SAMPLING_GAP | IngestIncomplete | A gap in sampling left the series incomplete |
| CLOCK_ANOMALY | Internal | A provider timestamp fell outside the clock-skew envelope |
| COLLECTOR_INTERRUPTED | IngestIncomplete | The collector died before recording a terminal attempt result |
| CREDENTIAL_CHANGED_UNVERIFIED | AuthRequired | The credential changed and the new one is not yet verified |
| INGEST_PARTIAL | IngestIncomplete | The report is partial because required source material could not be normalized |
| INTERNAL_ERROR | Internal | An unexpected internal failure with no finer classification |
| INVALID_USAGE | Usage | A configuration, argument or environment value was invalid |
| AUTHENTICATION_REQUIRED | AuthRequired | A requested live source requires authentication, reason not further classified |
| REMOTE_SOURCE_UNAVAILABLE | RemoteUnavailable | A requested live or remote source was unavailable, reason not further classified |
| STORE_FAILURE | Store | A store or durable-state operation failed |
| INSUFFICIENT_EVIDENCE | InsufficientEvidence | The evidence on hand was insufficient for the requested quantitative result |
| THRESHOLD_NOT_MET | ThresholdNotMet | An explicit threshold or advisory result was not met |

A remote timeout (`CONNECT_TIMEOUT`, `READ_TIMEOUT`, `TOTAL_BUDGET_EXPIRED`)
and a collector interruption (`COLLECTOR_INTERRUPTED`) are distinct codes even
though they map to different coarse classes, so automation can tell them apart
without parsing a message.
