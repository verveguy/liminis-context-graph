#!/usr/bin/env bash
# Start mlx_lm.server for the #248 benchmark with THINKING MODE DISABLED.
#
# Why: qwen3.6 defaults to emitting <think> reasoning before its answer. Measured
# 1627 completion tokens for a one-sentence input, of which ~1400 were discarded
# reasoning. That is ~10x slower AND scores worse on enumeration tasks -- see
# docs/history/extraction-eval-2026-04.md:
#     qwen3.6-27b-only          10.9s p50
#     qwen3.6-27b-thinking-only 112.7s p50  ("operationally non-viable")
#
# Idempotent: kills any existing server on the port first.

set -euo pipefail

mkdir -p /tmp/eval248

VENV="$HOME/liminis-eval-venv"
MODEL="mlx-community/Qwen3.6-27B-4bit"
PORT=8765
LOG=/tmp/eval248/mlx-server.log

echo "==> stopping any existing mlx_lm server"
pkill -f "mlx_lm server" 2>/dev/null || true
sleep 2

echo "==> starting $MODEL on port $PORT (thinking disabled)"
nohup "$VENV/bin/python" -m mlx_lm server \
  --model "$MODEL" \
  --port "$PORT" \
  --chat-template-args '{"enable_thinking": false}' \
  > "$LOG" 2>&1 &

echo "==> waiting for server to accept requests"
for i in $(seq 1 60); do
  if curl -sf --max-time 3 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1; then
    echo "    ready after ${i}s"
    break
  fi
  if [ "$i" -eq 60 ]; then
    echo "    FAILED to become ready; last 20 log lines:" >&2
    tail -20 "$LOG" >&2
    exit 1
  fi
  sleep 1
done

echo "==> verifying thinking is actually OFF"
curl -s --max-time 300 "http://127.0.0.1:$PORT/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Extract entities as JSON from: A New Glenn rocket exploded, delaying NASA.\"}],\"max_tokens\":4096,\"temperature\":0.0}" \
  > /tmp/eval248/verify.json

python3 - <<'PY'
import json, sys
d = json.load(open('/tmp/eval248/verify.json'), strict=False)
msg = d.get('choices', [{}])[0].get('message', {})
reasoning = msg.get('reasoning') or ''
tokens = d.get('usage', {}).get('completion_tokens', 0)
print(f"    completion_tokens: {tokens}")
print(f"    reasoning chars  : {len(reasoning)}")
if reasoning:
    print("    RESULT: thinking is STILL ON -- do not proceed", file=sys.stderr)
    print(f"    reasoning starts: {reasoning[:80]!r}", file=sys.stderr)
    sys.exit(1)
print("    RESULT: thinking is OFF -- good")
PY

echo
echo "server log: $LOG"
echo "next: /tmp/eval248/02-timing-check.sh"
