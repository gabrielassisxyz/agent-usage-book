# Transcript fixture corpus

This directory is the parser's contract with reality. Transcript formats change
without notice, so each fixture pins the shape the code was written for, and a
separate check against real files proves the fixture is still true. Keeping both,
and keeping them separate, is the rule: a test against a live file is not
reproducible, and a fixture alone eventually describes a format nobody produces.

The corpus is audited as a whole by `tests/fixture_corpus_audit.rs`: every
supported format must cover every catalog shape (either with a fixture or with a
machine-readable not-applicable rationale), every fixture must state the
input-format version it represents, and the whole directory must pass the
sanitization scan. The audit fails when any of those stops holding.

## Layout

- `native/` holds the parser fixtures for the three native-usage formats
  (claude-code, codex, pi). `native/MANIFEST.json` declares, per format, the
  input-format version and which fixture (or rationale) covers each catalog
  shape. `native/expected/` holds one golden expected-output file per applicable
  fixture.
- `nested-only/` holds the recursive-discovery regression fixture: every
  transcript file lives in a subdirectory, so a flat glob finds zero. It is part
  of the audited corpus; moving it out fails the audit.

## Capturing a new fixture

1. Copy a real transcript file from the source CLI's transcript directory into
   `native/`, named `<format>-<shape>.jsonl` (for example
   `claude-truncated.jsonl`). Keep the file as short as the shape allows: one or
   two records usually suffice.
2. Sanitize it. The fixture is committed, so it is publishable text:
   - Remove every text content payload. Keep at most a short string when the
     shape needs one (a nested subagent path, a model name).
   - Remove fields the parser does not read, unless the shape is about how the
     parser treats an unknown field.
   - Replace any value that could identify a person, a machine or an account
     (real session ids, paths under a real home directory, account names) with a
     short placeholder.
   - Remove any credential-shaped content. The shared forbidden-pattern list
     lives in `crates/test-support/src/sanitization.rs`; the audit scan reads it
     rather than restating it, and a fixture that matches any pattern fails the
     audit.
3. Verify the fixture parses with the intended outcome. Run the parser over it
   and record the normalized events and quarantine count in
   `native/expected/<fixture>.json` (one entry per event, in order, plus the
   quarantine count).
4. Declare the fixture in `native/MANIFEST.json` under its format and shape, and
   make sure the format's `input_format_version` matches what the parser
   declares.
5. Run the audit: `cargo test --test fixture_corpus_audit`. A fixture that fails
   the scan, a shape left uncovered, or a golden that does not match all fail
   loudly, naming the fixture and the shape.

A synthetic capture exercising these steps is part of the audit suite, so the
procedure cannot drift from what the tests enforce.
