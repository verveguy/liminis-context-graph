#!/usr/bin/env bash
# Scoring-only pass over three captured cassettes. ZERO extraction spend.
#
# Use this to resume a run whose judging phase died, or to re-score existing captures
# with different judge settings. Judge verdicts persist to the cache the moment they
# arrive (judge_cache.rs), so re-running after a failure re-serves everything already
# scored and only pays for what is left.
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
#   as zero -- silently, in a clean-looking report. lcg-eval itself refuses to run if any
#   two cassette backends are identical, or corrupt, or duplicate-keyed (#279).
#
# So: 04 is how you MAKE an independent sample. 05 is how you SCORE one you already have.
#
# Usage:
#   05-score-only.sh                 # reference-mode judging (default)
#   LCG_EVAL_JUDGE_MODE=both 05-score-only.sh
#       adds blind pairwise judging (#269) over the same cassettes: win rates plus the
#       reference-vs-candidate calibration control. Roughly doubles judge spend, because
#       every pair is judged in both slot orders to measure position bias.

set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/_common.sh"

HAIKU="${LCG_EVAL_HAIKU:-claude-haiku-4-5-20251001}"
BASELINE_CASSETTE="anthropic-$HAIKU.jsonl"
CANDIDATE_CASSETTE="anthropic-$HAIKU.candidate.jsonl"
QWEN_CASSETTE="qwen3.6-27b.jsonl"
JUDGE_CACHE="eval_judge_cache_248.jsonl"
JUDGE_MODE="${LCG_EVAL_JUDGE_MODE:-reference}"

cd "$REPO"
require_release_binary

for f in "$BASELINE_CASSETTE" "$CANDIDATE_CASSETTE" "$QWEN_CASSETTE"; do
  [ -s "$f" ] || die "missing or empty cassette: $f
       Run 04-full-run.sh to capture the live candidate leg first."
done

# The one mistake that fails silently rather than loudly (see header) — any two of these
# three cassettes being byte-identical, or any one being corrupt/duplicate-keyed — is now
# guarded by lcg-eval itself (#279 FR-002..FR-004), before it makes any judge call.

BEFORE=$(wc -l < "$JUDGE_CACHE" 2>/dev/null | tr -d ' ' || echo 0)

# A fully cached re-score makes no judge calls at all, so a key is not strictly required
# for that case. It is still demanded by default, because without one lcg-eval quietly
# reports strict-string F1 only -- a silent downgrade that looks like a completed run.
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  if [ "${LCG_EVAL_ALLOW_NO_KEY:-}" = "1" ]; then
    echo "==> no ANTHROPIC_API_KEY; proceeding because LCG_EVAL_ALLOW_NO_KEY=1"
    echo "    judged F1 will be reported only for verdicts already in the cache"
  else
    die "ANTHROPIC_API_KEY is not set. Extraction is free here, but judging is not,
       and without a key lcg-eval silently reports strict F1 only.
       If every verdict you need is already cached, re-run with LCG_EVAL_ALLOW_NO_KEY=1."
  fi
fi

echo "==> judge cache holds $BEFORE verdicts; already-scored chunks are free"
echo "==> starting $(date +%H:%M:%S) -- judge calls only, no extraction (mode: $JUDGE_MODE)"
echo

"$BIN" \
  --backend "baseline=cassette:path=$BASELINE_CASSETTE" \
  --backend "candidate=cassette:path=$CANDIDATE_CASSETTE" \
  --backend "qwen=cassette:path=$QWEN_CASSETTE" \
  --reference baseline \
  --all \
  --judge-mode "$JUDGE_MODE" \
  --judge-cache "$JUDGE_CACHE" \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248.json

AFTER=$(wc -l < "$JUDGE_CACHE" 2>/dev/null | tr -d ' ' || echo 0)
echo
echo "==> done $(date +%H:%M:%S) -- $((AFTER - BEFORE)) new judge calls"
echo "    report: eval_report_248.json"
echo
echo "    Read qwen's judged F1 against the baseline-vs-candidate noise floor, never on"
echo "    its own. Two independent Haiku samples set the ceiling any model could reach."
echo
echo "    Replayed legs report the ORIGINAL recording's latency, not this run's."
