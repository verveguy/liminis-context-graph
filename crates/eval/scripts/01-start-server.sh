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
# Idempotent: stops any existing server on this port first.

set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/_common.sh"

VENV="${LCG_EVAL_VENV:-$HOME/liminis-eval-venv}"
LOG="$WORK/mlx-server.log"

echo "==> stopping any existing server on port $PORT"
# Scoped to the port, not `pkill -f "mlx_lm server"`, which would also kill an unrelated
# MLX server serving a different model on a different port.
if PIDS=$(lsof -ti "tcp:$PORT" -sTCP:LISTEN 2>/dev/null) && [ -n "$PIDS" ]; then
  echo "    stopping: $PIDS"
  kill $PIDS 2>/dev/null || true
  sleep 2
  # Anything still holding the port after a polite kill
  if PIDS=$(lsof -ti "tcp:$PORT" -sTCP:LISTEN 2>/dev/null) && [ -n "$PIDS" ]; then
    kill -9 $PIDS 2>/dev/null || true
    sleep 1
  fi
else
  echo "    nothing listening on $PORT"
fi

[ -x "$VENV/bin/python" ] || die "no python at $VENV/bin/python (override with LCG_EVAL_VENV)"

echo "==> starting $MODEL on port $PORT (thinking disabled)"
nohup "$VENV/bin/python" -m mlx_lm server \
  --model "$MODEL" \
  --port "$PORT" \
  --chat-template-args '{"enable_thinking": false}' \
  > "$LOG" 2>&1 &

echo "==> waiting for server to accept requests"
for i in $(seq 1 60); do
  if curl -fsS --max-time 3 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1; then
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

echo "==> verifying model identity and that thinking is actually OFF"
require_server_healthy

echo
echo "server log: $LOG"
echo "next: $_SCRIPT_DIR/02-timing-check.sh"
