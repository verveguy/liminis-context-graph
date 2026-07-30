#!/usr/bin/env bash
# Tests for the benchmark scripts' decision logic. Runs in seconds, needs no network, no
# API key, no local model server, and no built binary.
#
# WHY THIS EXISTS
#
#   These scripts gate a multi-hour, ~$92 run, and their guards had accumulated real logic
#   — completeness thresholds, per-leg replay decisions, cassette diagnosis, filename
#   contracts — with no automated coverage at all. Every check was verified by hand and the
#   scaffolding thrown away, so reviewers ended up functioning as the test suite: #278 took
#   seven review rounds, and roughly half the findings were introduced by the previous
#   round's fix. Rounding down a threshold, conflating "corrupt" with "duplicated", two
#   separate cases of DRY_RUN previewing something the run would not do, and an unvalidated
#   mode name are all things a test would have caught in seconds.
#
#   So the rule this file encodes: any guard that decides whether to spend money, or that
#   claims to preview what a run will do, gets a test here.
#
# Usage: crates/eval/scripts/test-scripts.sh

set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; [ -n "${2:-}" ] && printf '       %s\n' "$2"; }

assert_eq() {
  local want="$1" got="$2" what="$3"
  if [ "$want" = "$got" ]; then ok "$what"; else bad "$what" "wanted [$want], got [$got]"; fi
}

assert_rc() {
  local want="$1" got="$2" what="$3"
  if [ "$want" = "$got" ]; then ok "$what"; else bad "$what" "wanted exit $want, got $got"; fi
}

assert_contains() {
  local needle="$1" hay="$2" what="$3"
  case "$hay" in
    *"$needle"*) ok "$what" ;;
    *) bad "$what" "expected to contain [$needle]" ;;
  esac
}

assert_not_contains() {
  local needle="$1" hay="$2" what="$3"
  case "$hay" in
    *"$needle"*) bad "$what" "should NOT contain [$needle]" ;;
    *) ok "$what" ;;
  esac
}

# --- fixtures ---------------------------------------------------------------------

WORKROOT="$(mktemp -d)"
trap 'chmod -R u+rwX "$WORKROOT" 2>/dev/null; rm -rf "$WORKROOT"' EXIT

# A cassette with n records, keys prefixed so two files can be made to differ or match.
make_cassette() {
  local path="$1" n="$2" prefix="${3:-k}"
  python3 -c "
import sys
p, n, pre = sys.argv[1], int(sys.argv[2]), sys.argv[3]
with open(p, 'w') as f:
    for i in range(n):
        f.write('{\"key\": \"%s%d\"}\n' % (pre, i))
" "$path" "$n" "$prefix"
}

# A repo-shaped directory: stub binary (so require_release_binary passes), ontology fixture.
make_fake_repo() {
  local root="$1"
  mkdir -p "$root/target/release" "$root/crates/core/tests/fixtures/real_corpus_wal"
  # Records its argv so tests can assert the binary was actually REACHED, and with which
  # flags. A stub that merely exits 0 lets an "this error is absent" assertion pass even when
  # the script crashed before invoking it — which is how a bash 3.2 empty-array crash on the
  # freeform and all-replay paths shipped past a green suite.
  cat > "$root/target/release/lcg-eval" <<'STUB'
#!/bin/sh
echo "STUB-INVOKED $*" >> "$(dirname "$0")/../../stub-argv.log"
exit 0
STUB
  chmod +x "$root/target/release/lcg-eval"
  echo "mode: strict" > "$root/crates/core/tests/fixtures/real_corpus_wal/ontology.yaml"
}

# Run 06 with a scratch WORK dir so the shared preamble's ownership check is satisfied and
# nothing leaks into /tmp/eval248.
run06() {
  local repo="$1"; shift
  env LCG_EVAL_REPO="$repo" LCG_EVAL_WORK="$WORKROOT/work" "$@" \
    "$HERE/06-ontology-matrix.sh" 2>&1
}

# shellcheck source=/dev/null
source "$HERE/_common.sh" >/dev/null 2>&1 || true

echo "== _common.sh: cassette_complete threshold =="
D="$WORKROOT/complete"; mkdir -p "$D"
make_cassette "$D/full.jsonl" 228
make_cassette "$D/at.jsonl" 206      # 206/228 = 90.4% -> pass
make_cassette "$D/below.jsonl" 205   # 205/228 = 89.9% -> must FAIL (this rounded through once)
make_cassette "$D/tiny.jsonl" 12
: > "$D/empty.jsonl"
cassette_complete "$D/full.jsonl";  assert_rc 0 $? "228/228 is complete"
cassette_complete "$D/at.jsonl";    assert_rc 0 $? "206/228 (90.4%) is complete"
cassette_complete "$D/below.jsonl"; assert_rc 1 $? "205/228 (89.9%) is NOT complete — no rounding down"
cassette_complete "$D/tiny.jsonl";  assert_rc 1 $? "12/228 is not complete"
cassette_complete "$D/empty.jsonl"; assert_rc 1 $? "empty file is not complete"
cassette_complete "$D/absent.jsonl"; assert_rc 1 $? "missing file is not complete"
assert_eq 228 "$(cassette_records "$D/full.jsonl")" "cassette_records counts records"
assert_eq 0 "$(cassette_records "$D/absent.jsonl")" "cassette_records on a missing file is 0"

# The corrupt-vs-duplicated cassette diagnosis formerly tested here via _common.sh's
# cassette_key_check now lives in lcg-eval itself (crates/core/src/cassette.rs's
# load_records, #279 FR-002/FR-003) — see its own unit tests (load_records_*) and
# crates/eval/src/plan.rs's tests for the guard-violation-level coverage.

echo "== _common.sh: backup_if_present =="
# sha256_of and the byte-identical-cassette-pair check it backed (both the pre-run guard
# formerly in this file's identity-check tests and 06's post-run "hosted cassettes came out
# identical" check) now live in lcg-eval itself (crates/eval/src/plan.rs's FR-004 guard for
# pre-existing cassette: backends, crates/eval/src/report.rs's
# validate_recorded_cassettes_distinct for freshly --record-cassette'd ones, #279) — see
# those modules' own unit tests for the guard-violation-level coverage.
B="$WORKROOT/bk"; mkdir -p "$B"; printf 'orig\n' > "$B/c.jsonl"
( cd "$B" && backup_if_present "c.jsonl" >/dev/null )
[ ! -e "$B/c.jsonl" ] && ok "backup_if_present moves the original aside" \
  || bad "backup_if_present moves the original aside"
assert_eq 1 "$(find "$B" -name 'c.jsonl.*.bak' | wc -l | tr -d ' ')" "backup lands under a .bak name"

echo "== 06: MODES contract =="
REPO1="$WORKROOT/repo1"; make_fake_repo "$REPO1"
out="$(run06 "$REPO1" MODES="" )"
assert_contains "MODES is empty" "$out" "explicit empty MODES is a no-op, not the default matrix"
assert_not_contains "capturing live" "$out" "empty MODES starts nothing"
out="$(run06 "$REPO1" MODES="Open strict" DRY_RUN=1)"
assert_contains "unknown mode 'Open'" "$out" "a typo'd mode is rejected"
assert_not_contains "REPLAY" "$out" "a typo'd mode prints no plan"
out="$(run06 "$REPO1" MODES="open" DRY_RUN=1)"
assert_contains "DRY_RUN: nothing executed" "$out" "a valid mode reaches the DRY_RUN summary"

echo "== 06: DRY_RUN needs no binary and no key =="
BARE="$WORKROOT/bare"; mkdir -p "$BARE"
out="$(run06 "$BARE" MODES="open" DRY_RUN=1)"
assert_contains "DRY_RUN: nothing executed" "$out" "DRY_RUN works with no release binary"
assert_not_contains "not found. Build it first" "$out" "DRY_RUN does not demand the binary"
out="$(run06 "$BARE" MODES="open")"
assert_contains "not found. Build it first" "$out" "a real run DOES demand the binary"

echo "== 06: report naming, incl. the empty-vs-unset contract =="
out="$(run06 "$REPO1" MODES="open" DRY_RUN=1)"
assert_contains "eval_report_266_noisefloor_open.json" "$out" "default report prefix"
assert_not_contains "eval_report_266_open.json " "$out" "does not use run_mode_matrix.sh's colliding name"
out="$(run06 "$REPO1" MODES="open" REPORT_PREFIX=myrun DRY_RUN=1)"
assert_contains "myrun_open.json" "$out" "custom REPORT_PREFIX is honoured"
out="$(run06 "$REPO1" MODES="open" REPORT_PREFIX= DRY_RUN=1)"
assert_contains "-> open.json" "$out" "explicitly-empty REPORT_PREFIX means unprefixed"

echo "== 06: per-leg resume decisions =="
REPO2="$WORKROOT/repo2"; make_fake_repo "$REPO2"
H="claude-haiku-4-5-20251001"
make_cassette "$REPO2/anthropic-open-baseline-$H.jsonl" 228 b
make_cassette "$REPO2/anthropic-open-candidate-$H.jsonl" 228 c
make_cassette "$REPO2/qwen3.6-27b-open.jsonl" 12 q      # truncated: the likeliest failure
out="$(run06 "$REPO2" MODES="open" DRY_RUN=1)"
assert_contains "baseline: REPLAY" "$out" "complete hosted leg replays"
assert_contains "candidate: REPLAY" "$out" "complete second hosted leg replays"
assert_contains "qwen: LIVE" "$out" "truncated leg is re-captured, not replayed as whole"
assert_not_contains "baseline: LIVE" "$out" "a truncated qwen leg does not discard the hosted legs"

echo "== 06: freeform reuses the #248 cassette names =="
REPO3="$WORKROOT/repo3"; make_fake_repo "$REPO3"
make_cassette "$REPO3/anthropic-$H.jsonl" 226 b
make_cassette "$REPO3/anthropic-$H.candidate.jsonl" 228 c
make_cassette "$REPO3/qwen3.6-27b.jsonl" 223 q
out="$(run06 "$REPO3" MODES="freeform" DRY_RUN=1)"
assert_eq 3 "$(printf '%s\n' "$out" | grep -c 'REPLAY')" "freeform replays all three legs"
assert_not_contains "anthropic-freeform-baseline" "$out" "freeform does not invent mode-segmented names"

# The identity/duplicate/corrupt-cassette abort-preview cases formerly tested here
# (identical hosted cassettes, a duplicate-keyed cassette, a corrupt cassette — each both
# in DRY_RUN and on a real invocation) moved to lcg-eval itself (#279 FR-002..FR-004) and
# are no longer previewable or enforceable by this script's stub binary; see
# crates/eval/src/plan.rs's test module for the Rust-side equivalents.

echo "== 06: ontology fixture required only when a mode needs it =="
REPO7="$WORKROOT/repo7"; make_fake_repo "$REPO7"
rm -f "$REPO7/crates/core/tests/fixtures/real_corpus_wal/ontology.yaml"
out="$(run06 "$REPO7" MODES="open" ANTHROPIC_API_KEY=dummy)"
assert_contains "ontology fixture not found" "$out" "a non-freeform mode requires the fixture"
out="$(run06 "$REPO7" MODES="freeform" ANTHROPIC_API_KEY=dummy)"
assert_not_contains "ontology fixture not found" "$out" "freeform does not require the fixture"

echo "== 06: LIMIT smoke path =="
REPO8="$WORKROOT/repo8"; make_fake_repo "$REPO8"
out="$(run06 "$REPO8" MODES="open" LIMIT=3 DRY_RUN=1)"
assert_contains "SMOKE RUN: first 3 chunks" "$out" "LIMIT announces itself as a smoke run"
assert_contains ".limit3" "$out" "smoke artifacts are suffixed so they cannot pass as full captures"
out="$(run06 "$REPO8" MODES="open" LIMIT=abc DRY_RUN=1)"
assert_contains "LIMIT must be a positive integer" "$out" "non-numeric LIMIT is rejected"
out="$(run06 "$REPO8" MODES="open" LIMIT=0 DRY_RUN=1)"
assert_contains "greater than 0" "$out" "LIMIT=0 is rejected"

# A smoke cassette must satisfy completeness against the LIMIT, not against 228 — and must
# never be read by a full run.
make_cassette "$REPO8/anthropic-open-baseline-$H.limit3.jsonl" 3 b
make_cassette "$REPO8/anthropic-open-candidate-$H.limit3.jsonl" 3 c
make_cassette "$REPO8/qwen3.6-27b-open.limit3.jsonl" 3 q
out="$(run06 "$REPO8" MODES="open" LIMIT=3 DRY_RUN=1)"
assert_eq 3 "$(printf '%s\n' "$out" | grep -c 'REPLAY')" "3-record cassettes are complete for LIMIT=3"
out="$(run06 "$REPO8" MODES="open" DRY_RUN=1)"
assert_not_contains "REPLAY" "$out" "a full run ignores smoke cassettes entirely"

# Freeform must not touch the #248 cassettes under LIMIT.
REPO9="$WORKROOT/repo9"; make_fake_repo "$REPO9"
make_cassette "$REPO9/anthropic-$H.jsonl" 226 b
make_cassette "$REPO9/anthropic-$H.candidate.jsonl" 228 c
make_cassette "$REPO9/qwen3.6-27b.jsonl" 223 q
out="$(run06 "$REPO9" MODES="freeform" LIMIT=3 DRY_RUN=1)"
assert_not_contains "anthropic-$H.jsonl " "$out" "a freeform smoke run does not read the #248 cassettes"
assert_contains ".limit3" "$out" "a freeform smoke run uses suffixed names"

echo "== 06: judge-mode cost is stated, not implied =="
out="$(run06 "$REPO1" MODES="open" DRY_RUN=1)"
assert_contains "04/05 default to 'reference'" "$out" "the differing judge-mode default is called out"
out="$(run06 "$REPO1" MODES="open" LCG_EVAL_JUDGE_MODE=reference DRY_RUN=1)"
assert_not_contains "includes pairwise" "$out" "no pairwise cost warning when reference is chosen"

echo "== 06: the binary is actually reached (bash 3.2 empty-array regression) =="
# macOS ships bash 3.2, where expanding an EMPTY array as "${arr[@]}" is an unbound-variable
# error under set -u (fixed in 4.4) — so it fails where the script is run and never on CI.
# ONT_ARGS is empty for freeform; RECORD is empty when every leg replays. Both crashed before
# invoking the binary, and nothing noticed because every reachability assertion here was
# "this error message is absent", which passes when the script dies of something else.
#
# These drive the REPLAY path deliberately: it needs no model server, so they are hermetic
# and behave identically on CI. It is also the only way to reach an empty RECORD at all (a
# live leg always records), while freeform gives an empty ONT_ARGS either way.
REPO_R1="$WORKROOT/reach1"; make_fake_repo "$REPO_R1"
make_cassette "$REPO_R1/anthropic-$H.jsonl" 228 b
make_cassette "$REPO_R1/anthropic-$H.candidate.jsonl" 228 c
make_cassette "$REPO_R1/qwen3.6-27b.jsonl" 228 q
rm -f "$REPO_R1/stub-argv.log"
out="$(run06 "$REPO_R1" MODES="freeform" ANTHROPIC_API_KEY=dummy)"
argv="$(cat "$REPO_R1/stub-argv.log" 2>/dev/null)"
assert_contains "STUB-INVOKED" "$argv" "freeform all-replay REACHES the binary (empty ONT_ARGS and RECORD)"
assert_not_contains "unbound variable" "$out" "no unbound-variable crash on freeform"
assert_not_contains "--ontology" "$argv" "freeform passes no --ontology flag"
assert_contains "--all" "$argv" "freeform passes the full-corpus scope"
assert_not_contains "--record-cassette" "$argv" "an all-replay run records nothing (empty RECORD)"

REPO_R2="$WORKROOT/reach2"; make_fake_repo "$REPO_R2"
make_cassette "$REPO_R2/anthropic-strict-baseline-$H.jsonl" 228 b
make_cassette "$REPO_R2/anthropic-strict-candidate-$H.jsonl" 228 c
make_cassette "$REPO_R2/qwen3.6-27b-strict.jsonl" 228 q
rm -f "$REPO_R2/stub-argv.log"
out="$(run06 "$REPO_R2" MODES="strict" ANTHROPIC_API_KEY=dummy)"
argv="$(cat "$REPO_R2/stub-argv.log" 2>/dev/null)"
assert_contains "--ontology-mode strict" "$argv" "strict passes its ontology mode"
assert_not_contains "unbound variable" "$out" "no unbound-variable crash on strict"

# The LIVE path additionally needs a reachable model server, which CI does not have. Run it
# when one is present so the dev machine still covers it; skip visibly rather than silently.
if curl -fsS --max-time 3 "http://127.0.0.1:${LCG_EVAL_PORT:-8765}/v1/models" >/dev/null 2>&1; then
  REPO_R3="$WORKROOT/reach3"; make_fake_repo "$REPO_R3"
  rm -f "$REPO_R3/stub-argv.log"
  out="$(run06 "$REPO_R3" MODES="freeform" ANTHROPIC_API_KEY=dummy)"
  argv="$(cat "$REPO_R3/stub-argv.log" 2>/dev/null)"
  assert_contains "STUB-INVOKED" "$argv" "freeform LIVE run reaches the binary"
  assert_contains "--record-cassette" "$argv" "a live run records its cassettes"
else
  ok "freeform LIVE run reaches the binary (skipped: no model server on this host)"
fi

# The identity/duplicate-key-under-LIMIT guard cases formerly tested here, and the
# validate_report.py report-accounting cases, both moved to lcg-eval itself (#279
# FR-002..FR-006) — see crates/eval/src/plan.rs's and crates/eval/src/report.rs's test
# modules for the Rust-side equivalents (including the exact LIMIT=3/33%-error-rate case
# that used to falsely fail under the old proportion-based heuristic).

echo
if [ "$FAIL" -eq 0 ]; then
  printf '\033[32mall %d checks passed\033[0m\n' "$PASS"
  exit 0
fi
printf '\033[31m%d of %d checks FAILED\033[0m\n' "$FAIL" "$((PASS + FAIL))"
exit 1
