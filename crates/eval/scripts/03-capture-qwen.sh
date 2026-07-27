#!/usr/bin/env bash
# Capture the qwen cassette. NO hosted spend: the Anthropic key is unset, so
# lcg-eval skips LLM-as-judge scoring entirely (main.rs:98-105).
#
# With one backend, --reference defaults to qwen itself, so the printed F1 numbers
# are a meaningless 1.000 self-comparison. THE CASSETTE IS THE DELIVERABLE.
#
# Usage:
#   03-capture-qwen.sh          # full 228-chunk corpus
#   03-capture-qwen.sh 25       # first 25 chunks only (validation run)

set -euo pipefail

mkdir -p /tmp/eval248

REPO="$HOME/dev/liminis-project/liminis-context-graph"
MODEL="mlx-community/Qwen3.6-27B-4bit"
CASSETTE="$REPO/qwen3.6-27b.jsonl"
LIMIT="${1:-}"

cd "$REPO"

# The cassette writer opens with create+append and never truncates (there is a test
# pinning that: writer_appends_without_truncating_across_multiple_opens). Re-running
# without moving the old file would silently produce duplicate entries.
if [ -s "$CASSETTE" ]; then
  STAMP=$(date +%Y%m%d-%H%M%S)
  mv "$CASSETTE" "$CASSETTE.$STAMP.bak"
  echo "==> moved existing cassette aside: $(basename "$CASSETTE").$STAMP.bak"
fi

if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  echo "==> unsetting ANTHROPIC_API_KEY so no judge calls are billed"
  unset ANTHROPIC_API_KEY
fi

if ! curl -sf --max-time 5 http://127.0.0.1:8765/v1/models >/dev/null 2>&1; then
  echo "ERROR: qwen server not reachable on 8765. Run 01-start-server.sh first." >&2
  exit 1
fi

if [ -n "$LIMIT" ]; then
  SCOPE=(--limit "$LIMIT"); echo "==> capturing FIRST $LIMIT chunks"
else
  SCOPE=(--all);            echo "==> capturing FULL corpus (228 chunks)"
fi

echo "==> started $(date +%H:%M:%S); watch progress with:"
echo "    wc -l $CASSETTE"
echo

./target/release/lcg-eval \
  --backend "qwen=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=$MODEL" \
  "${SCOPE[@]}" \
  --record-cassette "qwen=$CASSETTE" \
  --output /tmp/eval248/qwen-capture-report.json

echo
echo "==> done $(date +%H:%M:%S)"
echo "    cassette records: $(wc -l < "$CASSETTE" | tr -d ' ')"
echo "    report: /tmp/eval248/qwen-capture-report.json"
