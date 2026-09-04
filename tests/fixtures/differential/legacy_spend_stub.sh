#!/usr/bin/env bash
# Deterministic stub legacy spend tool for differential harness testing (aub-lqe.16).
# Simulates legacy spend reporting across six controlled scenarios:
# 1. agreement: outputs matching totals for the deterministic corpus
# 2. classified_disagreement: outputs legacy totals exhibiting the five named discrepancy categories
# 3. unclassified_disagreement: introduces a one-unit unexplained residual
# 4. child_nonzero_exit: fails loudly with nonzero exit status
# 5. timeout: sleeps past the test timeout budget
# 6. malformed_output: prints invalid non-JSON output
set -euo pipefail

SCENARIO="${LEGACY_STUB_SCENARIO:-agreement}"
SINCE="2026-08-25"
DAYS=2

while [ "$#" -gt 0 ]; do
    case "$1" in
        --scenario)
            SCENARIO="$2"
            shift 2
            ;;
        --since)
            SINCE="$2"
            shift 2
            ;;
        --days)
            DAYS="$2"
            shift 2
            ;;
        --format)
            # Ignored; always outputs JSON when successful
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

case "$SCENARIO" in
    child_nonzero_exit)
        echo "legacy-spend-stub: fatal: database lock timeout or process crashed" >&2
        exit 1
        ;;
    timeout)
        sleep 2
        exit 0
        ;;
    malformed_output)
        echo "<<<MALFORMED_OUTPUT_NOT_JSON>>> line 1 column 1"
        exit 0
        ;;
    agreement)
        cat <<'JSON'
{
  "tool": "legacy-spend",
  "periods": [
    {
      "period": "2026-08-25",
      "input": 1200,
      "output": 600,
      "cache_read": 2000,
      "cache_write": 3000,
      "total": 6800
    },
    {
      "period": "2026-08-26",
      "input": 500,
      "output": 250,
      "cache_read": 1000,
      "cache_write": 0,
      "total": 1750
    }
  ]
}
JSON
        exit 0
        ;;
    classified_disagreement)
        cat <<'JSON'
{
  "tool": "legacy-spend",
  "periods": [
    {
      "period": "2026-08-25",
      "input": 1000,
      "output": 500,
      "cache_read": 2000,
      "cache_write": 0,
      "total": 3500
    },
    {
      "period": "2026-08-26",
      "input": 490,
      "output": 355,
      "cache_read": 1000,
      "cache_write": 0,
      "total": 1845
    }
  ]
}
JSON
        exit 0
        ;;
    unclassified_disagreement)
        cat <<'JSON'
{
  "tool": "legacy-spend",
  "periods": [
    {
      "period": "2026-08-25",
      "input": 1000,
      "output": 501,
      "cache_read": 2000,
      "cache_write": 0,
      "total": 3501
    },
    {
      "period": "2026-08-26",
      "input": 490,
      "output": 355,
      "cache_read": 1000,
      "cache_write": 0,
      "total": 1845
    }
  ]
}
JSON
        exit 0
        ;;
    *)
        echo "legacy-spend-stub: unknown scenario '$SCENARIO'" >&2
        exit 2
        ;;
esac
