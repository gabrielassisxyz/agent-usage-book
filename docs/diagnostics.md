# Diagnostic event vocabulary

Diagnostics are JSON lines on stderr. Every line has `ts`, `level`, `event`, and `run`.
`run` also appears in each report envelope. Default level is `warn`; set `AUB_LOG_LEVEL` to
`error`, `warn`, `info`, `debug`, or `trace`, or repeat `-v` up to three times to raise it.

Only logical names and safe typed fields may enter diagnostics. Credentials and provider bodies
render as `[REDACTED]`. Quantities are JSON objects with `value` and `unit`.

| Event | Level | Fields |
|---|---|---|
| run_started | info | command |
| report_rendered | info | report_kind |
| request_attempted | info | command |
| ingest_batch_landed | info | batch, events, writer_slot, generation |
| meter_attempt_committed | info | attempt, busy_wait |
| meter_evidence_spooled | info | attempt |
| meter_spool_drained | info | applied, already_applied, quarantined |
| projection_read | debug | state |
| legacy_meter_imported | info | source_digest, verified_backup_id, records_read, imported, unchanged, quarantined |
| seed_archive_imported | info | source_digest, verified_backup_id, records_read, imported, unchanged, quarantined, terminal_outcome |
| meter_window_anomaly_detected | warn | anomaly, kind, account |
