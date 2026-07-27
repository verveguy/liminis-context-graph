#!/usr/bin/env bash
# The real #248 benchmark: noise floor + hosted-vs-qwen, one pass.
#
# COSTS REAL MONEY, but far less than it used to: exactly ONE live hosted leg plus
# judge calls. The other two legs replay cassettes captured earlier (#263), so this
# finishes in minutes rather than the ~2.3h a live qwen leg takes on the full corpus.
#
# Why these three backends:
#   baseline  = cassette  -> the reference. Replaying the recorded Haiku run costs
#                            nothing and is byte-stable across re-runs, which is
#                            exactly what you want from a reference.
#   candidate = anthropic -> LIVE, and it must stay live. Its disagreement with
#                            baseline IS the noise floor: two independent Haiku
#                            samples of the same corpus. Point this at a cassette and
#                            judged F1 becomes 1.000 by construction -- the
#                            measurement silently destroys itself. Do not "optimise"
#                            this leg into a replay.
#   qwen      = cassette  -> the candidate under test, replayed from 03's capture.
#
# A cassette miss is a per-chunk error (Error::CassetteMiss), not a crash, and shows up
# in the report's error_rate. The two cassettes do not cover identical chunk sets, so a
# handful of unscored chunks is expected -- see the COVERAGE line this prints.
#
# NOT VALID FOR ONTOLOGY RUNS. `opts.ontology` feeds both the entity and edge system
# prompts, and those are hashed into the cassette key (cassette.rs:69-88). Adding
# --ontology changes every key, so these cassettes miss on all of them. The ontology
# axis of #266 needs its own capture, including a fresh ~2.3h qwen pass.

set -euo pipefail

mkdir -p /tmp/eval248

REPO="$HOME/dev/liminis-project/liminis-context-graph"
HAIKU="claude-haiku-4-5-20251001"
BASELINE_CASSETTE="anthropic-$HAIKU.jsonl"
QWEN_CASSETTE="qwen3.6-27b.jsonl"
CANDIDATE_CASSETTE="anthropic-$HAIKU.candidate.jsonl"

cd "$REPO"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ERROR: ANTHROPIC_API_KEY is not set. The live candidate leg needs it, and" >&2
  echo "       without it judged F1 is silently skipped (main.rs:98-105)." >&2
  exit 1
fi

# The cassettes are INPUTS here. 03 moves its output aside before writing; this script
# must not, or it would destroy the thing it is about to read.
for f in "$BASELINE_CASSETTE" "$QWEN_CASSETTE"; do
  if [ ! -s "$f" ]; then
    echo "ERROR: missing or empty cassette: $f" >&2
    echo "       run 03-capture-qwen.sh for qwen, or a live recording pass for baseline." >&2
    exit 1
  fi
done

# The candidate's cassette is an OUTPUT, so the append-never-truncate rule applies.
if [ -s "$CANDIDATE_CASSETTE" ]; then
  STAMP=$(date +%Y%m%d-%H%M%S)
  mv "$CANDIDATE_CASSETTE" "$CANDIDATE_CASSETTE.$STAMP.bak"
  echo "==> moved existing candidate cassette aside: $CANDIDATE_CASSETTE.$STAMP.bak"
fi

python3 - "$BASELINE_CASSETTE" "$QWEN_CASSETTE" <<'PY'
import json, sys
def keys(p):
    with open(p) as f:
        return {json.loads(l)["key"] for l in f if l.strip()}
b, q = keys(sys.argv[1]), keys(sys.argv[2])
print(f"==> COVERAGE  baseline {len(b)}  qwen {len(q)}  scoreable overlap {len(b & q)}")
PY

echo
echo "==> starting run $(date +%H:%M:%S) -- one live Haiku leg + judge"
echo

./target/release/lcg-eval \
  --backend "baseline=cassette:path=$BASELINE_CASSETTE" \
  --backend "candidate=anthropic:model=$HAIKU" \
  --backend "qwen=cassette:path=$QWEN_CASSETTE" \
  --reference baseline \
  --all \
  --record-cassette "candidate=$CANDIDATE_CASSETTE" \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248.json

echo
echo "==> done $(date +%H:%M:%S)"
echo "    report: eval_report_248.json"
echo
echo "    Read latency as p50 with p95/p99 alongside. Over the full corpus qwen measured"
echo "    p50 39.8s / p95 212.6s / p99 377.9s against a 62.4s mean -- the mean overstates"
echo "    the typical chunk by more than half, so quoting it alone misleads."
echo
echo "    Replayed legs report the ORIGINAL recording's latency, not this run's."
