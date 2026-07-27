#!/usr/bin/env bash
# Scoring-only pass over three captured cassettes. ZERO extraction spend.
#
# Use this to resume a run whose judging phase died, or to re-score existing captures
# with different judge settings. Judge verdicts persist to the cache the moment they
# arrive (judge_cache.rs:157-162), so re-running after a failure re-serves everything
# already scored and only pays for what's left.
#
# WHY ALL THREE LEGS MAY BE REPLAYED HERE, when 04 insists `candidate` must be live:
#
#   The noise floor requires two INDEPENDENT SAMPLES of the reference model -- that is a
#   constraint on the *sampling events*, not on how the results reach the scorer. Once
#   04 has run the live candidate leg and captured it, that independent sample exists
#   permanently. Replaying baseline and candidate here still compares two separate live
#   runs, so the noise floor is intact.
#
#   What destroys the measurement is replaying the SAME cassette into both legs: the two
#   become byte-identical, judged F1 is 1.000 by construction, and the noise floor reads
#   as zero -- silently, in a clean-looking report. This script guards against that by
#   refusing to run if any two cassette paths are identical.
#
# So: 04 is how you MAKE an independent sample. 05 is how you SCORE one you already have.

set -euo pipefail

REPO="$HOME/dev/liminis-project/liminis-context-graph"
HAIKU="claude-haiku-4-5-20251001"
BASELINE_CASSETTE="anthropic-$HAIKU.jsonl"
CANDIDATE_CASSETTE="anthropic-$HAIKU.candidate.jsonl"
QWEN_CASSETTE="qwen3.6-27b.jsonl"

cd "$REPO"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ERROR: ANTHROPIC_API_KEY is not set. Extraction is free here, but the judge" >&2
  echo "       is not -- and without a key lcg-eval silently reports strict F1 only." >&2
  exit 1
fi

for f in "$BASELINE_CASSETTE" "$CANDIDATE_CASSETTE" "$QWEN_CASSETTE"; do
  if [ ! -s "$f" ]; then
    echo "ERROR: missing or empty cassette: $f" >&2
    echo "       run 04-full-run.sh to capture the live candidate leg first." >&2
    exit 1
  fi
done

# Guard the one mistake that fails silently rather than loudly (see header).
if [ "$(md5 -q "$BASELINE_CASSETTE")" = "$(md5 -q "$CANDIDATE_CASSETTE")" ]; then
  echo "ERROR: baseline and candidate cassettes are identical." >&2
  echo "       The noise floor would come back as 1.000 by construction." >&2
  exit 1
fi

BEFORE=$(wc -l < eval_judge_cache_248.jsonl 2>/dev/null | tr -d ' ' || echo 0)

python3 - "$BASELINE_CASSETTE" "$CANDIDATE_CASSETTE" "$QWEN_CASSETTE" <<'PY'
import json, sys
def keys(p):
    with open(p) as f:
        return {json.loads(l)["key"] for l in f if l.strip()}
b, c, q = (keys(p) for p in sys.argv[1:4])
print(f"==> COVERAGE  baseline {len(b)}  candidate {len(c)}  qwen {len(q)}")
print(f"    scoreable  vs candidate {len(b & c)}   vs qwen {len(b & q)}")
PY

echo "==> judge cache holds $BEFORE verdicts; already-scored chunks are free"
echo "==> starting $(date +%H:%M:%S) -- judge calls only, no extraction"
echo

./target/release/lcg-eval \
  --backend "baseline=cassette:path=$BASELINE_CASSETTE" \
  --backend "candidate=cassette:path=$CANDIDATE_CASSETTE" \
  --backend "qwen=cassette:path=$QWEN_CASSETTE" \
  --reference baseline \
  --all \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248.json

AFTER=$(wc -l < eval_judge_cache_248.jsonl | tr -d ' ')
echo
echo "==> done $(date +%H:%M:%S) -- $((AFTER - BEFORE)) new judge calls"
echo "    report: eval_report_248.json"
echo
echo "    Read qwen's judged F1 against the baseline-vs-candidate noise floor, never on"
echo "    its own. Two independent Haiku samples set the ceiling any model could reach."
echo
echo "    Replayed legs report the ORIGINAL recording's latency, not this run's."
