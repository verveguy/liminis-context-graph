#!/usr/bin/env bash
# Runs the freeform / Open / Strict ontology-mode matrix (#266) over the same corpus and
# backends, producing three structurally comparable lcg-eval JSON reports.
#
# This does NOT run itself in CI or as part of any automated pipeline — it requires a live,
# spend-authorized ANTHROPIC_API_KEY and a reachable local OpenAI-compatible model server,
# neither of which is available inside Fabrik's sandbox (see #248/#217 precedent). It exists so
# a maintainer can reproduce the exact three commands without hand-retyping them — see
# docs/eval-full-corpus-runbook.md's "Running the ontology mode matrix" section for the
# line-by-line walkthrough this script mirrors.
#
# Usage: from the repo root (paths below are resolved relative to this script, matching
#   default_corpus_path()'s CARGO_MANIFEST_DIR-relative resolution, not the shell's cwd):
#
#   export ANTHROPIC_API_KEY=sk-ant-...
#   LOCAL_BACKEND_SPEC='oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=mlx-community/Qwen3.6-27B-4bit' \
#     bash crates/eval/scripts/run_mode_matrix.sh
#
# Override any of these env vars to change defaults:
#   ANTHROPIC_MODEL     claude-haiku-4-5-20251001
#   LOCAL_BACKEND_SPEC   (required) full --backend SPEC for the local candidate, e.g.
#                        'oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=mlx-community/Qwen3.6-27B-4bit'
#   ONTOLOGY_FIXTURE    the committed FR-005 fixture (crates/core/tests/fixtures/real_corpus_wal/ontology.yaml)
#   JUDGE_MODEL         claude-sonnet-4-6
#   JUDGE_CACHE_PREFIX  eval_judge_cache_266 (one shared cache file across all three modes —
#                       the cache key already includes rendered prompts, so ontology-mode
#                       differences never collide in it; see FR-007's mode-aware cassette key)
#   REPORT_PREFIX       eval_report_266
#   EVAL_ARGS           extra args appended verbatim to every invocation, e.g. '--limit 20'
#                       for a cheap smoke pass instead of the full corpus (--all is the default)
#
# Each mode records baseline=anthropic to its own cassette file
# (anthropic-<mode>-claude-haiku-4-5-20251001.jsonl) — ontology participates in the cassette
# key via the rendered system prompts (crates/core/src/cassette.rs), so a cassette recorded
# under one mode is never reusable under another. If a later run needs to resume only the local
# leg for a given mode (e.g. the local server dropped mid-run), replay that mode's own cassette
# instead of re-paying for the hosted leg — see the runbook's "Resuming a partial run" section
# for the worked example this generalizes.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

: "${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY must be set — see docs/eval-full-corpus-runbook.md}"
: "${LOCAL_BACKEND_SPEC:?LOCAL_BACKEND_SPEC must be set, e.g. LOCAL_BACKEND_SPEC='oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=mlx-community/Qwen3.6-27B-4bit'}"

ANTHROPIC_MODEL="${ANTHROPIC_MODEL:-claude-haiku-4-5-20251001}"
ONTOLOGY_FIXTURE="${ONTOLOGY_FIXTURE:-$REPO_ROOT/crates/core/tests/fixtures/real_corpus_wal/ontology.yaml}"
JUDGE_MODEL="${JUDGE_MODEL:-claude-sonnet-4-6}"
JUDGE_CACHE_PREFIX="${JUDGE_CACHE_PREFIX:-eval_judge_cache_266}"
REPORT_PREFIX="${REPORT_PREFIX:-eval_report_266}"
EVAL_ARGS="${EVAL_ARGS:---all}"

run_mode() {
    local mode="$1"
    shift
    echo "=== lcg-eval: ontology mode '$mode' ===" >&2
    # shellcheck disable=SC2086
    cargo run --release -p lcg-eval -- \
        --backend "baseline=anthropic:model=$ANTHROPIC_MODEL" \
        --backend "local=$LOCAL_BACKEND_SPEC" \
        --reference baseline \
        --record-cassette "baseline=anthropic-${mode}-${ANTHROPIC_MODEL}.jsonl" \
        --judge-cache "${JUDGE_CACHE_PREFIX}.jsonl" \
        --judge-model "$JUDGE_MODEL" \
        --output "${REPORT_PREFIX}_${mode}.json" \
        "$@" \
        $EVAL_ARGS
}

# 1. Freeform — no --ontology flag; ExtractOptions.ontology is None (FR-001's unchanged path).
run_mode freeform

# 2. Open — declared types are preferred, the model may still invent others.
run_mode open --ontology "$ONTOLOGY_FIXTURE" --ontology-mode open

# 3. Strict — only declared types are ever accepted; FR-007's vocabulary-compliance metric
#    is only populated in this mode.
run_mode strict --ontology "$ONTOLOGY_FIXTURE" --ontology-mode strict

echo "Wrote ${REPORT_PREFIX}_freeform.json, ${REPORT_PREFIX}_open.json, ${REPORT_PREFIX}_strict.json" >&2
