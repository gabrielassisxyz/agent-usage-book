# Anthropic Meter Fixture Corpus

Sanitized real and synthetic-shape response captures for the Anthropic OAuth usage provider adapter (`aub-eun.4`).

## Sanitization Procedure

All fixtures in this directory are vetted to ensure:
1. No credential material, session tokens, or API identifiers are present.
2. No personal identifiers (email addresses, real account names) are present.
3. No internal tracing headers or request identifiers are present.
4. All fixtures pass the shared scan in `test_support::sanitization::matched_patterns`.

## Catalog of Fixtures

- `valid-success.json`: Normal subscription usage response with 5h and 7d windows.
- `zero-percentage.json`: Unused quota (0.0% utilization).
- `multiple-windows.json`: Account-wide 5h/7d windows plus multiple model-specific windows (`seven_day_sonnet`, `seven_day_opus`).
- `model-specific.json`: Model-specific weekly window (`seven_day_sonnet`).
- `error-401-invalid.json`: 401 response with rejected/invalid credential error.
- `error-401-expired.json`: 401 response with provider-declared token expiry message.
- `error-403-ambiguous.json`: 403 Forbidden with generic permission error (classified as `HttpStatus(ClientError)`, not authentication).
- `error-429.json`: 429 Too Many Requests response with retry information.
- `malformed.json`: Invalid non-JSON payload.
- `missing-field.json`: Valid JSON missing a required quota field (`utilization`).
- `unknown-fields.json`: Valid response containing forward-compatible unknown fields.
- `stale-timestamp.json`: Valid response with a past reset timestamp.
- `reset-changed-a.json` & `reset-changed-b.json`: Paired responses demonstrating a changed reset timestamp.
