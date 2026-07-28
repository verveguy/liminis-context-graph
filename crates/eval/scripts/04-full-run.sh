#!/usr/bin/env bash
# The real #248 benchmark: noise floor + hosted-vs-qwen, one pass.
#
# COSTS REAL MONEY, but far less than it used to: exactly ONE live hosted leg plus
# judge calls. The other two legs replay cassettes captured earlier (#263), so this
# finishes far quicker than the ~2.3h a live qwen leg takes on the full corpus.
#
# Why these three backends:
#   baseline  = cassette  -> the reference. Replaying the recorded Haiku run costs
#                            nothing and is byte-stable across re-runs, which is
#                            exactly what you want from a reference.
#   candidate = anthropic -> LIVE, and it must stay live IN THIS SCRIPT. Its
#                            disagreement with baseline IS the noise floor: two
#                            independent Haiku samples of the same corpus. Replaying
#                            the baseline cassette here makes the two legs identical,
#                            judged F1 comes back 1.000 by construction, and the noise
#                            floor silently reads as zero. Do not "optimise" this leg.
#
#                            The constraint is on the *sampling events* being
#                            independent, not on how results reach the scorer. Once
#                            this script has captured its own candidate cassette, that
#                            independent sample exists permanently -- and
#                            05-score-only.sh may then replay all three legs to
#                            re-score at zero extraction cost. 04 makes the sample;
#                            05 scores one that already exists.
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
source "$(dirname -- "${BASH_SOURCE[0]}")/_common.sh"

HAIKU="${LCG_EVAL_HAIKU:-claude-haiku-4-5-20251001}"
BASELINE_CASSETTE="anthropic-$HAIKU.jsonl"
QWEN_CASSETTE="qwen3.6-27b.jsonl"
CANDIDATE_CASSETTE="anthropic-$HAIKU.candidate.jsonl"
JUDGE_MODE="${LCG_EVAL_JUDGE_MODE:-reference}"

cd "$REPO"
require_release_binary

[ -n "${ANTHROPIC_API_KEY:-}" ] || die "ANTHROPIC_API_KEY is not set. The live candidate
       leg needs it, and without it judged F1 is silently skipped."

# The cassettes are INPUTS here. 03 moves its output aside before writing; this script
# must not, or it would destroy the thing it is about to read.
for f in "$BASELINE_CASSETTE" "$QWEN_CASSETTE"; do
  [ -s "$f" ] || die "missing or empty cassette: $f
       Run 03-capture-qwen.sh for qwen, or a live recording pass for baseline."
done

# The candidate's cassette is an OUTPUT, so the append-never-truncate rule applies.
backup_if_present "$CANDIDATE_CASSETTE"

python3 - "$BASELINE_CASSETTE" "$QWEN_CASSETTE" <<'PY'
import hashlib, json, sys, collections

def digest(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for block in iter(lambda: f.read(65536), b""):
            h.update(block)
    return h.hexdigest()

def keys(p):
    ks = [json.loads(l)["key"] for l in open(p) if l.strip()]
    dupes = [k for k, n in collections.Counter(ks).items() if n > 1]
    if dupes:
        sys.exit(f"ERROR: {p} contains {len(dupes)} duplicate keys — it was appended to "
                 "without being moved aside. Replay would serve stale verdicts.")
    return set(ks)

# This script spends real money on the live candidate leg before any scoring happens, so
# the checks that would invalidate the whole comparison must run first and fail closed.
if digest(sys.argv[1]) == digest(sys.argv[2]):
    sys.exit("ERROR: the baseline and qwen cassettes are byte-identical. Every F1 between "
             "them would be 1.000 by construction — one was almost certainly copied over "
             "the other.")

b, q = keys(sys.argv[1]), keys(sys.argv[2])
print(f"==> COVERAGE  baseline {len(b)}  qwen {len(q)}  scoreable overlap {len(b & q)}")
if not (b & q):
    sys.exit("ERROR: the cassettes share no chunks — nothing could be scored, and the live "
             "leg would be paid for nothing.")
# A partial mismatch is a warning, not a failure: the real captures legitimately differ
# (226/223 with a 221 overlap) because each side lost different chunks to malformed
# structured output, and unscoreable chunks are accounted in error_rate rather than hidden.
if len(b & q) < min(len(b), len(q)):
    print(f"    note: {min(len(b), len(q)) - len(b & q)} chunk(s) present on only one side")
PY

echo
echo "==> starting run $(date +%H:%M:%S) -- one live Haiku leg + judge (mode: $JUDGE_MODE)"
echo

"$BIN" \
  --backend "baseline=cassette:path=$BASELINE_CASSETTE" \
  --backend "candidate=anthropic:model=$HAIKU" \
  --backend "qwen=cassette:path=$QWEN_CASSETTE" \
  --reference baseline \
  --all \
  --judge-mode "$JUDGE_MODE" \
  --record-cassette "candidate=$CANDIDATE_CASSETTE" \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248.json

echo
echo "==> done $(date +%H:%M:%S)"
echo "    report: eval_report_248.json"
echo
echo "    Read qwen's judged F1 against the baseline-vs-candidate noise floor, never on"
echo "    its own. Two independent Haiku samples set the ceiling any model could reach."
echo
echo "    Quote latency as p50 with p95/p99 alongside; replayed legs report the ORIGINAL"
echo "    recording's latency, not this run's."
