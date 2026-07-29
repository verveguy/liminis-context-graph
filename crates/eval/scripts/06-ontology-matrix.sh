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
# Overridable like every other tunable here. It was hardcoded, which read as a deliberate
# pin for cross-mode reproducibility but was simply an omission — and an inconsistency a
# future reader would have to test to resolve. Changing it mid-matrix WOULD invalidate the
# comparison, since the judge model is part of the cache key and of what each F1 means, so
# the warning is where the knob is.
JUDGE_MODEL="${LCG_EVAL_JUDGE_MODEL:-claude-sonnet-4-6}"
# Shared with run_mode_matrix.sh on purpose, unlike the report name. Judge cache keys are
# derived from rendered prompts and operand content, not from backend names, so verdicts
# computed by either script are reusable by the other — sharing is free reuse, not a
# collision. It is the *reports* that mean different things and must not share a name.
JUDGE_CACHE="${LCG_EVAL_JUDGE_CACHE:-eval_judge_cache_266.jsonl}"
# `${MODES-...}` not `${MODES:-...}`: the colon form treats an EMPTY value as unset, so
# `MODES="" 06-ontology-matrix.sh` silently ran the full default matrix and began live
# hosted extraction. An explicit empty MODES must mean "do nothing", which is the only
# safe reading for a script that spends money on startup.
MODES="${MODES-open strict}"

cd "$REPO"

if [ -z "${MODES// /}" ]; then
  echo "==> MODES is empty — nothing to do. Set MODES to some of: freeform open strict" >&2
  exit 0
fi

# Validated here rather than inside DRY_RUN, so a typo is rejected identically by both
# paths. Without this, MODES="Open strict" sailed through cassette_names_for_mode into the
# else branch, printed a full plan against fabricated cassette names, and the real run then
# aborted immediately at lcg-eval's --ontology-mode validation. That is the same failure the
# comments in this file warn about twice: a preview that omits a stopping condition.
for _m in $MODES; do
  case "$_m" in
    freeform|open|strict) ;;
    *) die "unknown mode '$_m'. Valid modes: freeform, open, strict (case-sensitive —
       lcg-eval's --ontology-mode rejects anything else)." ;;
  esac
done

echo "==> ontology matrix: modes [$MODES], judge-mode $JUDGE_MODE, judge $JUDGE_MODEL"
echo "    fixture: $ONTOLOGY"
echo "    THIS SPENDS MONEY AND RUNS FOR HOURS. Set DRY_RUN=1 to print the plan only."
echo

# Sets BASE_CAS/CAND_CAS/QWEN_CAS for a mode. Defined once and called from BOTH the
# DRY_RUN branch and the real loop: a dry run that prints different filenames from the ones
# the run would actually use is worse than no dry run at all, because it invites precisely
# the mistake it exists to reveal.
cassette_names_for_mode() {
  local mode="$1"
  # Freeform reuses the #248 cassettes, which predate this script and carry no mode segment
  # in their names. Without this mapping the freeform arm looks for files that do not exist
  # and re-captures live — two hosted legs and 2.3h to reproduce data already on disk,
  # which is the exact waste this script's header claims to avoid.
  if [ "$mode" = "freeform" ]; then
    BASE_CAS="anthropic-$HAIKU.jsonl"
    CAND_CAS="anthropic-$HAIKU.candidate.jsonl"
    QWEN_CAS="qwen3.6-27b.jsonl"
  else
    BASE_CAS="anthropic-$mode-baseline-$HAIKU.jsonl"
    CAND_CAS="anthropic-$mode-candidate-$HAIKU.jsonl"
    QWEN_CAS="qwen3.6-27b-$mode.jsonl"
  fi
}

# One rule for the report filename, shared by DRY_RUN and the real loop — re-deriving it
# separately is how the summary came to advertise a name the run never wrote.
report_name_for_mode() {
  local prefix="${REPORT_PREFIX-eval_report_266_noisefloor}"
  if [ -n "$prefix" ]; then echo "${prefix}_$1.json"; else echo "$1.json"; fi
}

# A dry run is the only way to inspect what this will do without it starting. Added
# because the first structural test of this script began a live capture within seconds.
if [ -n "${DRY_RUN:-}" ]; then
  for mode in $MODES; do
    cassette_names_for_mode "$mode"
    echo "    mode '$mode' (ontology=${ONTOLOGY##*/}) -> $(report_name_for_mode "$mode")"
    for pair in "baseline:$BASE_CAS" "candidate:$CAND_CAS" "qwen:$QWEN_CAS"; do
      leg="${pair%%:*}"
      cas="${pair#*:}"
      if cassette_complete "$cas"; then
        echo "      $leg: REPLAY $cas ($(cassette_records "$cas") records)"
      elif [ -s "$cas" ]; then
        echo "      $leg: LIVE — $cas is incomplete ($(cassette_records "$cas") records)"
      else
        echo "      $leg: LIVE — no cassette at $cas"
      fi
    done

    # The aborts a real run would hit are previewed too, not just the replay/live plan.
    # Two clean REPLAY lines followed by an immediate abort on the real invocation is the
    # same failure as printing the wrong filenames: DRY_RUN exists to show what will
    # happen, and a preview that omits the stopping conditions is not one.
    for cas in "$BASE_CAS" "$CAND_CAS" "$QWEN_CAS"; do
      if cassette_complete "$cas"; then
        keyerr="$(cassette_key_check "$cas" 2>&1)" || case $? in
          1) echo "      WOULD ABORT: $cas has duplicate keys ($keyerr)" ;;
          2) echo "      WOULD ABORT: $cas is corrupt or truncated ($keyerr)" ;;
        esac
      fi
    done
    if cassette_complete "$BASE_CAS" && cassette_complete "$CAND_CAS" \
       && [ "$(sha256_of "$BASE_CAS")" = "$(sha256_of "$CAND_CAS")" ]; then
      echo "      WOULD ABORT: baseline and candidate cassettes are byte-identical —"
      echo "                   the noise floor would be 1.000 by construction"
    fi
  done
  echo
  echo "==> DRY_RUN: nothing executed."
  exit 0
fi

# Checked only once a real run is committed to. They used to run first, which meant
# DRY_RUN — whose entire purpose is inspecting the plan before anything is in place —
# died on a missing binary or an unset key. The plan-printing path must work before the
# prerequisites do.
require_release_binary
[ -n "${ANTHROPIC_API_KEY:-}" ] || die "ANTHROPIC_API_KEY is not set. Both hosted legs and
       the judge need it, and without it lcg-eval silently reports strict F1 only."
# Only required if some requested mode actually uses it. `MODES=freeform` passes no
# --ontology flag at all (see the empty ONT_ARGS branch), so dying on a missing fixture
# there would block a run that never needed it.
for _m in $MODES; do
  if [ "$_m" != "freeform" ]; then
    [ -s "$ONTOLOGY" ] || die "ontology fixture not found: $ONTOLOGY
       (needed for mode '$_m'; a freeform-only run does not require it)"
    break
  fi
done


REPORTS=()
for mode in $MODES; do
  cassette_names_for_mode "$mode"
  # Deliberately NOT run_mode_matrix.sh's `eval_report_266_$mode.json`. That script is
  # two-backend with no noise floor; this one is three-backend with one. The two reports
  # are structurally different and a reader cannot tell them apart from the filename, so
  # sharing a name means a later run silently overwrites the other's result and the
  # survivor gets read as whatever the reader assumes. Nothing backs reports up the way
  # backup_if_present covers cassettes, so the loss would be total and unannounced.
  # `${REPORT_PREFIX-...}` not `${REPORT_PREFIX:-...}`, matching the MODES contract this
  # file explains at the top: an explicitly-empty value must mean something different from
  # unset. REPORT_PREFIX="" now yields an unprefixed `<mode>.json` rather than silently
  # reverting to the default. Nothing is at stake but a filename — the point is that the
  # empty-vs-unset rule holds everywhere in this script, not just where money is involved.
  REPORT="$(report_name_for_mode "$mode")"

  ONT_ARGS=()
  if [ "$mode" != "freeform" ]; then
    ONT_ARGS=(--ontology "$ONTOLOGY" --ontology-mode "$mode")
  fi

  # Resume is decided PER LEG, not all-or-nothing.
  #
  # Backends run sequentially and fully, in the order baseline, candidate, qwen — so the
  # likeliest failure is a death during the ~2.3h local leg, leaving both hosted cassettes
  # complete and qwen's truncated. An all-or-nothing check discards those two and re-pays
  # ~$8 to reproduce data already on disk. Per-leg resume replays what completed and
  # re-captures only what did not.
  #
  # Mixing a replayed leg with a live one is safe for the NOISE FLOOR, which is what the
  # earlier all-or-nothing rule was protecting: baseline and candidate only have to be
  # independent *sampling events*, and a recording of one live run against a fresh run of
  # the other still satisfies that. What must never happen is the same cassette feeding
  # both, which the identity check below catches.
  #
  # Completeness, not mere existence: a truncated cassette is non-empty and would otherwise
  # replay as if whole, showing up only as an elevated error_rate with nothing saying why.
  BACKENDS=() RECORD=() PLAN=()
  NEED_SERVER=0
  for leg in baseline candidate qwen; do
    case "$leg" in
      baseline) cas="$BASE_CAS"; live="anthropic:model=$HAIKU" ;;
      candidate) cas="$CAND_CAS"; live="anthropic:model=$HAIKU" ;;
      qwen) cas="$QWEN_CAS"; live="oai-http:url=$URL,model=$MODEL" ;;
    esac
    if cassette_complete "$cas"; then
      # Complete by count is not the same as trustworthy. A cassette appended to outside
      # backup_if_present — a hand-resumed capture, a stray copy — holds duplicate keys,
      # and replay serves duplicates FIFO, so a chunk would be scored against a stale
      # verdict rather than failing. 04 and 05 already reject this before trusting an input.
      require_replayable_cassette "$cas"
      BACKENDS+=(--backend "$leg=cassette:path=$cas")
      PLAN+=("$leg=replay($(cassette_records "$cas") records)")
    else
      if [ -s "$cas" ]; then
        echo "    $leg: cassette present but incomplete ($(cassette_records "$cas") records) — re-capturing"
      fi
      backup_if_present "$cas"
      BACKENDS+=(--backend "$leg=$live")
      RECORD+=(--record-cassette "$leg=$cas")
      PLAN+=("$leg=live")
      [ "$leg" = "qwen" ] && NEED_SERVER=1
    fi
  done

  echo "=== mode '$mode': ${PLAN[*]} ==="

  # Only verify the local server if a live qwen leg is actually planned. Reachability alone
  # is not enough: this re-checks the served model id and that thinking is off, so a server
  # left over from a reboot cannot silently produce a thinking-mode capture.
  [ "$NEED_SERVER" = "1" ] && require_server_healthy

  # The noise floor dies silently if baseline and candidate are the same data, so check it
  # whenever both are replayed. When either is live they are independent by construction,
  # and the post-run check below covers the freshly recorded pair.
  if cassette_complete "$BASE_CAS" && cassette_complete "$CAND_CAS"; then
    if [ "$(sha256_of "$BASE_CAS")" = "$(sha256_of "$CAND_CAS")" ]; then
      die "$BASE_CAS and $CAND_CAS are byte-identical — the noise floor would be 1.000 by
       construction. One was copied over the other; delete both and re-capture."
    fi
  fi

  REPORTS+=("$REPORT")
  # Belt and braces: even with a distinct name, never lose a previous result silently.
  backup_if_present "$REPORT"

  echo "    started $(date '+%H:%M:%S')"
  "$BIN" \
    "${BACKENDS[@]}" \
    "${RECORD[@]}" \
    --reference baseline \
    --all \
    "${ONT_ARGS[@]}" \
    --judge-mode "$JUDGE_MODE" \
    --judge-cache "$JUDGE_CACHE" \
    --judge-model "$JUDGE_MODEL" \
    --output "$REPORT"
  echo "    finished $(date '+%H:%M:%S') — report: $REPORT"

  # Everything below runs AFTER $BIN, because capture and judging share one invocation and
  # a freshly recorded cassette cannot be inspected before it is judged. Asserting here at
  # least stops a partial or degenerate report being read as a real result.
  #
  # cassette_complete gates which legs REPLAY; without this it never gated what a live
  # capture produced, so a short capture could leave a plausible-looking report behind.
  for cas in "$BASE_CAS" "$CAND_CAS" "$QWEN_CAS"; do
    cassette_complete "$cas" || die "$REPORT is INVALID: $cas holds only
       $(cassette_records "$cas") records, below the 90% completeness threshold. That
       report came from a partial capture — delete it and re-run this mode."
  done

  # Capture and judging happen in one invocation, so a freshly recorded pair cannot be
  # compared before it is judged. Asserting afterwards at least stops a degenerate report
  # being read as a real one.
  if [ -s "$BASE_CAS" ] && [ -s "$CAND_CAS" ] \
     && [ "$(sha256_of "$BASE_CAS")" = "$(sha256_of "$CAND_CAS")" ]; then
    die "$REPORT is INVALID: the two hosted cassettes came out byte-identical, so the
       noise floor in it is 1.000 by construction rather than measured. Delete both
       cassettes and the report, and re-capture."
  fi
  echo
done

echo "==> done. Reports:"
# Printed from what was actually written, not re-derived. Re-deriving is how this line came
# to advertise `eval_report_266_$mode.json` — the very name the REPORT_PREFIX rename exists
# to avoid, belonging to run_mode_matrix.sh's output — while the run wrote something else,
# and it ignored a custom REPORT_PREFIX entirely.
for r in "${REPORTS[@]}"; do echo "    $r"; done
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
