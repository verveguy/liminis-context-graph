#!/usr/bin/env bash
# Measure generation speed and project the full-corpus runtime BEFORE committing
# hours to a capture. Run this after 01-start-server.sh.
#
# Exits NONZERO when the projection is out of line with the April baseline or thinking
# mode is on, so it can gate a scripted workflow rather than merely printing advice.
#
# Reference points from docs/history/extraction-eval-2026-04.md (same M3 Ultra):
#   qwen3.6-27b extract p50: 5.7s (demo) / 10.9s (production)
#   raw generation:          30-60 tok/s

set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/_common.sh"

# Per-chunk seconds above which we refuse to green-light a full capture. The April
# production baseline is ~11s/chunk; 60 is a deliberately loose ceiling that still catches
# the 10x thinking-mode regression and a wrong-model fallback. Override for a machine
# under known load.
THRESHOLD_S="${LCG_EVAL_MAX_CHUNK_S:-60}"

require_server_healthy
echo

echo "==> checking the machine is quiet"
# MLX inference on Apple Silicon is bound by memory bandwidth x active params per
# token (see docs/history/extraction-eval-2026-04.md). A concurrent compile/test job
# contends for exactly that bandwidth, so timings taken under load read pessimistic.
# Fabrik workers running cargo test have been measured at ~460% CPU during a capture.
LOAD=$(uptime | sed 's/.*averages*: //' | awk '{print $1}' | tr -d ',')
# No `| head` here: with `set -o pipefail`, head closing the pipe early can abort the
# script mid-check. awk truncates its own output instead.
BUSY=$(ps -Ao pcpu,comm -r | awk 'NR>1 && $1>100 && n<6 {printf "%s(%s%%) ", $2, $1; n++}')
echo "    load average: $LOAD"
if [ -n "$BUSY" ]; then
  echo "    BUSY PROCESSES: $BUSY"
  echo "    WARNING: timings below will read pessimistic. For a benchmark-quality"
  echo "             number, quiesce Fabrik and other heavy jobs first."
else
  echo "    no heavy competing processes"
fi
echo

echo "==> raw generation speed (500 tokens, small prompt)"
python3 - "$MODEL" > "$WORK/req1.json" <<'PY'
import json, sys
print(json.dumps({
    "model": sys.argv[1],
    "messages": [{"role": "user", "content": "Write a detailed 400-word description of the solar system."}],
    "max_tokens": 500,
    "temperature": 0.0,
}))
PY
S=$(python3 -c 'import time;print(time.time())')
api_post "$WORK/req1.json" "$WORK/t1.json" 600
E=$(python3 -c 'import time;print(time.time())')
python3 -c "
import json
d=json.load(open('$WORK/t1.json'),strict=False); u=d.get('usage',{})
ct=u.get('completion_tokens',0); el=$E-$S
print(f'    {el:.1f}s for {ct} tokens -> {ct/el:.1f} tok/s' if el else '    no timing')
print('    (expect 30-60 tok/s)')
"

echo
echo "==> realistic extraction call (large system prompt + json_object)"
# $MODEL is passed via argv: this heredoc is quoted, so a literal id written inside would
# never track an override and would silently keep hitting the old model after a bump --
# reintroducing exactly the wrong-model-id trap these scripts document.
python3 - "$MODEL" > "$WORK/req2.json" <<'PY'
import json, sys
system = "You are an entity extraction system. " + "Follow these detailed extraction rules carefully. " * 120
user = ("Extract entities and relationships as JSON from: On May 28 2026 a New Glenn rocket "
        "blew up during testing. The explosion destroyed the rocket and the launch pad, causing "
        "a major delay for NASA and Blue Origin. Jeff Bezos, founder of Blue Origin, responded.")
print(json.dumps({
    "model": sys.argv[1],
    "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
    "max_tokens": 8192,
    "temperature": 0.0,
    "response_format": {"type": "json_object"},
}))
PY
S=$(python3 -c 'import time;print(time.time())')
api_post "$WORK/req2.json" "$WORK/t2.json" 900
E=$(python3 -c 'import time;print(time.time())')

python3 -c "
import json, sys
d=json.load(open('$WORK/t2.json'),strict=False)
u=d.get('usage',{}); msg=d.get('choices',[{}])[0].get('message',{})
ct=u.get('completion_tokens',0); pt=u.get('prompt_tokens',0); el=$E-$S
reasoning=msg.get('reasoning') or ''
print(f'    {el:.1f}s  prompt={pt}  completion={ct}  -> {ct/el:.1f} tok/s')
print(f'    reasoning chars: {len(reasoning)}  (MUST be 0)')
print()
per_chunk = el*2
print(f'    a chunk is 2 such calls (entity + edge) -> ~{per_chunk:.0f}s per chunk')
print(f'    228 chunks -> {228*per_chunk/3600:.1f} hours')
print()
if len(reasoning) > 0:
    sys.exit('    STOP: thinking is still on. Re-run 01-start-server.sh.')
if per_chunk > $THRESHOLD_S:
    sys.exit(f'    STOP: ~{per_chunk:.0f}s/chunk exceeds the {$THRESHOLD_S}s ceiling '
             f'(April baseline ~11s). Investigate before spending hours on a capture; '
             f'raise LCG_EVAL_MAX_CHUNK_S only if you know why it is slow.')
print('    In line with the April baseline. Safe to proceed to 03-capture-qwen.sh.')
"
