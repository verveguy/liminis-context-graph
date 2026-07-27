#!/usr/bin/env bash
# The real #248 benchmark: noise floor + hosted-vs-qwen, both cassettes, one pass.
#
# COSTS REAL MONEY. Two hosted Haiku legs over 228 chunks plus judge calls.
# The 705-entry judge cache in the repo makes previously-scored comparisons free.
#
# Requires ANTHROPIC_API_KEY to be exported -- build_extractor errors at startup if
# it is missing, so a bad key fails immediately rather than partway through.
#
# Why three backends:
#   baseline  = anthropic  -> the reference, and the Haiku cassette
#   candidate = anthropic  -> a SECOND INDEPENDENT hosted run. Its disagreement with
#                             baseline IS the noise floor. Do not replace this with a
#                             cassette replay: identical inputs make judged F1 1.000
#                             by construction and the measurement becomes meaningless.
#   qwen      = local      -> the actual candidate under test

set -euo pipefail

mkdir -p /tmp/eval248

REPO="$HOME/dev/liminis-project/liminis-context-graph"
HAIKU="claude-haiku-4-5-20251001"
QWEN="mlx-community/Qwen3.6-27B-4bit"

cd "$REPO"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ERROR: ANTHROPIC_API_KEY is not set. Export it first." >&2
  exit 1
fi

if ! curl -sf --max-time 5 http://127.0.0.1:8765/v1/models >/dev/null 2>&1; then
  echo "ERROR: qwen server not reachable on 8765. Run 01-start-server.sh first." >&2
  exit 1
fi

for f in "anthropic-$HAIKU.jsonl" qwen3.6-27b.jsonl; do
  if [ -s "$f" ]; then
    STAMP=$(date +%Y%m%d-%H%M%S)
    mv "$f" "$f.$STAMP.bak"
    echo "==> moved existing cassette aside: $f.$STAMP.bak"
  fi
done

echo "==> starting full run $(date +%H:%M:%S)"
echo "    this runs for hours; use tmux/ccs so a disconnect does not kill it"
echo

./target/release/lcg-eval \
  --backend "baseline=anthropic:model=$HAIKU" \
  --backend "candidate=anthropic:model=$HAIKU" \
  --backend "qwen=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=$QWEN" \
  --reference baseline \
  --all \
  --record-cassette "baseline=anthropic-$HAIKU.jsonl" \
  --record-cassette "qwen=qwen3.6-27b.jsonl" \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248.json

echo
echo "==> done $(date +%H:%M:%S)"
echo "    report:    eval_report_248.json"
echo "    cassettes: anthropic-$HAIKU.jsonl  qwen3.6-27b.jsonl"
