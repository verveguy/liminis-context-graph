#!/usr/bin/env bash
# Measure generation speed and project the full-corpus runtime BEFORE committing
# hours to a capture. Run this after 01-start-server.sh.
#
# Reference points from docs/history/extraction-eval-2026-04.md (same M3 Ultra):
#   qwen3.6-27b extract p50: 5.7s (demo) / 10.9s (production)
#   raw generation:          30-60 tok/s

set -euo pipefail

mkdir -p /tmp/eval248

MODEL="mlx-community/Qwen3.6-27B-4bit"
URL="http://127.0.0.1:8765/v1/chat/completions"

echo "==> raw generation speed (500 tokens, small prompt)"
S=$(python3 -c 'import time;print(time.time())')
curl -s --max-time 600 "$URL" -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Write a detailed 400-word description of the solar system.\"}],\"max_tokens\":500,\"temperature\":0.0}" \
  > /tmp/eval248/t1.json
E=$(python3 -c 'import time;print(time.time())')
python3 -c "
import json
d=json.load(open('/tmp/eval248/t1.json'),strict=False); u=d.get('usage',{})
ct=u.get('completion_tokens',0); el=$E-$S
print(f'    {el:.1f}s for {ct} tokens -> {ct/el:.1f} tok/s' if el else '    no timing')
print('    (expect 30-60 tok/s)')
"

echo
echo "==> realistic extraction call (large system prompt + json_object)"
python3 - <<'PY' > /tmp/eval248/req2.json
import json
system = "You are an entity extraction system. " + "Follow these detailed extraction rules carefully. " * 120
user = ("Extract entities and relationships as JSON from: On May 28 2026 a New Glenn rocket "
        "blew up during testing. The explosion destroyed the rocket and the launch pad, causing "
        "a major delay for NASA and Blue Origin. Jeff Bezos, founder of Blue Origin, responded.")
print(json.dumps({
    "model": "mlx-community/Qwen3.6-27B-4bit",
    "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
    "max_tokens": 8192,
    "temperature": 0.0,
    "response_format": {"type": "json_object"},
}))
PY
S=$(python3 -c 'import time;print(time.time())')
curl -s --max-time 900 "$URL" -H 'content-type: application/json' -d @/tmp/eval248/req2.json \
  > /tmp/eval248/t2.json
E=$(python3 -c 'import time;print(time.time())')

python3 -c "
import json
d=json.load(open('/tmp/eval248/t2.json'),strict=False)
u=d.get('usage',{}); msg=d.get('choices',[{}])[0].get('message',{})
ct=u.get('completion_tokens',0); pt=u.get('prompt_tokens',0); el=$E-$S
reasoning=msg.get('reasoning') or ''
print(f'    {el:.1f}s  prompt={pt}  completion={ct}  -> {ct/el:.1f} tok/s' if el else '')
print(f'    reasoning chars: {len(reasoning)}  (MUST be 0)')
print()
per_chunk = el*2
print(f'    a chunk is 2 such calls (entity + edge) -> ~{per_chunk:.0f}s per chunk')
print(f'    228 chunks -> {228*per_chunk/3600:.1f} hours')
print()
if len(reasoning)>0:
    print('    STOP: thinking is still on. Re-run 01-start-server.sh.')
elif per_chunk > 60:
    print('    Slower than the April baseline (~11s/chunk). Investigate before a full capture.')
else:
    print('    In line with the April baseline. Safe to proceed to 03-capture-qwen.sh.')
"
