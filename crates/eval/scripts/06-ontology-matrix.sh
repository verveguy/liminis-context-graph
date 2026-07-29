#!/usr/bin/env bash
# The ontology-mode arms of the #248/#266 comparison: Open and Strict, each with a noise
# floor, over the same corpus and backends as the freeform run.
#
# COSTS REAL MONEY AND TAKES HOURS. Per mode: two live hosted extraction legs (~$4 each),
# one local qwen capture (free, ~2.3h), and judging (~$38 with --judge-mode both). Two
# modes is therefore roughly $92 and an overnight, dominated by the qwen captures.
#
# WHY THREE BACKENDS, when run_mode_matrix.sh uses two.
#
#   That script pairs baseline against the local candidate and stops. Without a second
#   independent hosted sample there is no noise floor, and an F1 with no ceiling is not
#   interpretable: on the freeform corpus Haiku agreed with ITSELF only 0.869 on entities
#   and 0.422 on edges, so qwen's 0.225 edge score is 53% of achievable rather than the
#   catastrophe it looks like in isolation. The ontology arms need their own ceiling for
#   the same reason — and the ceiling is expected to MOVE, since constraining relation
#   names should make Haiku more self-consistent. That shift is one of the more
#   interesting things this matrix can measure, and two backends cannot see it at all.
#
# WHY FREEFORM IS NOT RE-RUN HERE.
#
#   We already own freeform cassettes for all three legs from the #248 run. Re-capturing
#   them would cost two hosted legs and 2.3h of local compute to reproduce data sitting on
#   disk. Set MODES="freeform open strict" if you genuinely need a fresh freeform arm.
#
# RESUMABLE BY DESIGN. Extraction is the part you must never pay for twice. If a mode's
# three cassettes already exist, this replays them instead of re-extracting, so a run that
# dies during judging costs only the judge calls it had not yet made. Judge verdicts
# persist to the cache as they arrive, so those are not re-paid either.

set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/_common.sh"

HAIKU="${LCG_EVAL_HAIKU:-claude-haiku-4-5-20251001}"
ONTOLOGY="${LCG_EVAL_ONTOLOGY:-$REPO/crates/core/tests/fixtures/real_corpus_wal/ontology.yaml}"
JUDGE_MODE="${LCG_EVAL_JUDGE_MODE:-both}"
JUDGE_CACHE="${LCG_EVAL_JUDGE_CACHE:-eval_judge_cache_266.jsonl}"
# `${MODES-...}` not `${MODES:-...}`: the colon form treats an EMPTY value as unset, so
# `MODES="" 06-ontology-matrix.sh` silently ran the full default matrix and began live
# hosted extraction. An explicit empty MODES must mean "do nothing", which is the only
# safe reading for a script that spends money on startup.
MODES="${MODES-open strict}"

cd "$REPO"
require_release_binary

[ -n "${ANTHROPIC_API_KEY:-}" ] || die "ANTHROPIC_API_KEY is not set. Both hosted legs and
       the judge need it, and without it lcg-eval silently reports strict F1 only."
[ -s "$ONTOLOGY" ] || die "ontology fixture not found: $ONTOLOGY"

if [ -z "${MODES// /}" ]; then
  echo "==> MODES is empty — nothing to do. Set MODES to some of: freeform open strict" >&2
  exit 0
fi

echo "==> ontology matrix: modes [$MODES], judge-mode $JUDGE_MODE"
echo "    fixture: $ONTOLOGY"
echo "    THIS SPENDS MONEY AND RUNS FOR HOURS. Set DRY_RUN=1 to print the plan only."
echo

# A dry run is the only way to inspect what this will do without it starting. Added
# because the first structural test of this script began a live capture within seconds.
if [ -n "${DRY_RUN:-}" ]; then
  for mode in $MODES; do
    echo "    mode '$mode': would use ontology=${ONTOLOGY##*/} mode=$mode,"
    echo "      cassettes anthropic-$mode-{baseline,candidate}-$HAIKU.jsonl, qwen3.6-27b-$mode.jsonl"
    echo "      -> eval_report_266_$mode.json"
  done
  echo
  echo "==> DRY_RUN: nothing executed."
  exit 0
fi


for mode in $MODES; do
  BASE_CAS="anthropic-$mode-baseline-$HAIKU.jsonl"
  CAND_CAS="anthropic-$mode-candidate-$HAIKU.jsonl"
  QWEN_CAS="qwen3.6-27b-$mode.jsonl"
  REPORT="eval_report_266_$mode.json"

  ONT_ARGS=()
  if [ "$mode" != "freeform" ]; then
    ONT_ARGS=(--ontology "$ONTOLOGY" --ontology-mode "$mode")
  fi

  # A mode is resumable only if ALL THREE cassettes are present. A partial set means the
  # capture died midway; replaying two legs against a third that must run live would
  # compare a recording to a fresh sample, which is fine for qwen but silently destroys
  # the noise floor if it happens to be baseline vs candidate.
  if [ -s "$BASE_CAS" ] && [ -s "$CAND_CAS" ] && [ -s "$QWEN_CAS" ]; then
    echo "=== mode '$mode': all three cassettes present — REPLAYING (no extraction spend) ==="
    if [ "$(md5 -q "$BASE_CAS")" = "$(md5 -q "$CAND_CAS")" ]; then
      die "$BASE_CAS and $CAND_CAS are byte-identical — the noise floor would be 1.000 by
       construction. One was copied over the other; delete both and re-capture."
    fi
    BACKENDS=(
      --backend "baseline=cassette:path=$BASE_CAS"
      --backend "candidate=cassette:path=$CAND_CAS"
      --backend "qwen=cassette:path=$QWEN_CAS"
    )
    RECORD=()
  else
    echo "=== mode '$mode': capturing live (2 hosted legs + ~2.3h local qwen) ==="
    for f in "$BASE_CAS" "$CAND_CAS" "$QWEN_CAS"; do backup_if_present "$f"; done
    # Reachability is not enough: this re-verifies the served model id and that thinking is
    # off, so a server left over from a reboot cannot silently produce a thinking-mode
    # capture — the 15-hour mistake these scripts exist to prevent.
    require_server_healthy
    BACKENDS=(
      --backend "baseline=anthropic:model=$HAIKU"
      --backend "candidate=anthropic:model=$HAIKU"
      --backend "qwen=oai-http:url=$URL,model=$MODEL"
    )
    RECORD=(
      --record-cassette "baseline=$BASE_CAS"
      --record-cassette "candidate=$CAND_CAS"
      --record-cassette "qwen=$QWEN_CAS"
    )
  fi

  echo "    started $(date '+%H:%M:%S')"
  "$BIN" \
    "${BACKENDS[@]}" \
    "${RECORD[@]}" \
    --reference baseline \
    --all \
    "${ONT_ARGS[@]}" \
    --judge-mode "$JUDGE_MODE" \
    --judge-cache "$JUDGE_CACHE" \
    --judge-model claude-sonnet-4-6 \
    --output "$REPORT"
  echo "    finished $(date '+%H:%M:%S') — report: $REPORT"
  echo
done

echo "==> done. Reports:"
for mode in $MODES; do echo "    eval_report_266_$mode.json"; done
echo
echo "    Compare each mode against ITS OWN baseline-vs-candidate noise floor, not against"
echo "    the other modes' raw F1. The ceiling is expected to differ per mode — that shift"
echo "    is a finding, not an artifact to normalise away."
echo
echo "    Prior prediction, recorded before the run: the fixture's aliases and keywords"
echo "    exist to collapse relation-name divergence (752 distinct names over 2289 edges),"
echo "    while entity typing already converges freeform. So edge F1 should move a lot and"
echo "    entity F1 barely. Entity F1 moving sharply instead points at the fixture, not at"
echo "    something interesting about ontologies."
