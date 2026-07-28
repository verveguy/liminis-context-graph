#!/usr/bin/env bash
# Capture the qwen cassette. NO hosted spend: the Anthropic key is unset, so
# lcg-eval skips LLM-as-judge scoring entirely.
#
# With one backend, --reference defaults to qwen itself, so the printed F1 numbers
# are a meaningless 1.000 self-comparison. THE CASSETTE IS THE DELIVERABLE.
#
# Usage:
#   03-capture-qwen.sh          # full 228-chunk corpus
#   03-capture-qwen.sh 25       # first 25 chunks only (validation run)

set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/_common.sh"

CASSETTE="$REPO/qwen3.6-27b.jsonl"
REPORT="$WORK/qwen-capture-report.json"
LIMIT="${1:-}"
EXPECTED_CHUNKS="${LIMIT:-228}"

cd "$REPO"
require_release_binary

# Reachability alone is not enough: this re-verifies the model id and that thinking is
# off, so running against a server left over from a reboot cannot silently repeat the
# 15-hour thinking-mode capture these scripts exist to prevent.
require_server_healthy

backup_if_present "$CASSETTE"

if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  echo "==> unsetting ANTHROPIC_API_KEY so no judge calls are billed"
  unset ANTHROPIC_API_KEY
fi

if [ -n "$LIMIT" ]; then
  SCOPE=(--limit "$LIMIT"); echo "==> capturing FIRST $LIMIT chunks"
else
  SCOPE=(--all);            echo "==> capturing FULL corpus ($EXPECTED_CHUNKS chunks)"
fi

echo "==> started $(date +%H:%M:%S); watch progress with:"
echo "    wc -l $CASSETTE"
echo

"$BIN" \
  --backend "qwen=oai-http:url=$URL,model=$MODEL" \
  "${SCOPE[@]}" \
  --record-cassette "qwen=$CASSETTE" \
  --output "$REPORT"

echo
echo "==> lcg-eval exited $(date +%H:%M:%S); validating the capture"

# Exit status 0 is not evidence of a usable cassette. A wrong model id fails every call
# and still exits cleanly, leaving an empty file behind — check the artifact, not the
# exit code.
python3 - "$CASSETTE" "$REPORT" "$EXPECTED_CHUNKS" <<'PY'
import json, sys
cassette, report, expected = sys.argv[1], sys.argv[2], int(sys.argv[3])

records = [json.loads(l) for l in open(cassette) if l.strip()]
if not records:
    sys.exit("ERROR: cassette is empty — every call failed. Check the model id and server log.")

empty = sum(1 for r in records
            if not ((r.get("response") or {}).get("entities")
                    or (r.get("response") or {}).get("edges")))
print(f"    records: {len(records)}/{expected}")
print(f"    empty extractions: {empty}")

c = json.load(open(report))["candidates"][0]
rate = c["error_rate"]
print(f"    errors: {c['errors']} ({rate*100:.1f}%)  "
      f"malformed: {c['structured_output']['malformed']}")
print(f"    latency p50/p95/p99 ms: {c['latency']['p50_ms']}/"
      f"{c['latency']['p95_ms']}/{c['latency']['p99_ms']}")

# A handful of malformed structured outputs is normal and is accounted per-chunk; a large
# fraction means something systemic and the cassette must not be trusted downstream.
if rate > 0.10:
    sys.exit(f"ERROR: error rate {rate*100:.1f}% exceeds 10% — do not use this cassette.")
if len(records) < expected * 0.9:
    sys.exit(f"ERROR: only {len(records)} of {expected} chunks recorded (<90%).")
print("    capture looks usable.")
PY

echo
echo "    cassette: $CASSETTE"
echo "    report:   $REPORT"
echo
echo "    Quote latency as p50 with p95/p99. The full-corpus capture measured p50 39.8s /"
echo "    p95 212.6s / p99 377.9s against a 62.4s mean — the mean describes nothing."
