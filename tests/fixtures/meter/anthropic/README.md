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
- `limits-success.json`: The limits contract with required session and weekly-all constraints,
  one model-scoped weekly constraint, and matching named-block calibration values.
- `weekly-scoped-object.json`: The live limits shape whose model-scoped weekly constraint carries
  `scope.model` as an object (`display_name` plus a nullable `id`), sanitized from the retained
  response evidence of 2026-09-06.
- `weekly-scoped-model-unidentified.json`: The same live shape with the display name absent and
  the `id` null, the planted negative for the scoped model identity rule.
- `zero-percentage.json`: Unused quota (0.0% utilization).
- `multiple-windows.json`: Account-wide 5h/7d windows plus multiple model-specific windows (`seven_day_sonnet`, `seven_day_opus`).
- `model-specific.json`: Model-specific weekly window (`seven_day_sonnet`).
- `error-401-invalid.json`: 401 response with rejected/invalid credential error.
- `error-401-expired.json`: 401 response with provider-declared token expiry message.
- `error-403-ambiguous.json`: 403 Forbidden with generic permission error (classified as `HttpStatus(ClientError)`, not authentication).
- `error-429.json`: 429 Too Many Requests response with retry information.
- `malformed.json`: Invalid non-JSON payload.
- `missing-field.json`: Valid response missing the required top-level `five_hour` window.
- `unknown-fields.json`: Valid response containing forward-compatible unknown fields.
- `stale-timestamp.json`: Valid response with a past reset timestamp.
- `reset-changed-a.json` & `reset-changed-b.json`: Paired responses demonstrating a changed reset timestamp.
- `idle-five-hour.json`: Idle 5-hour window with null reset instant and populated 7-day window.
