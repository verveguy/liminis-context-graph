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
  printf '#!/bin/sh\nexit 0\n' > "$root/target/release/lcg-eval"
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

echo "== _common.sh: cassette_key_check diagnosis (1 = duplicates, 2 = anything else) =="
K="$WORKROOT/keys"; mkdir -p "$K"
printf '{"key":"a"}\n{"key":"b"}\n'        > "$K/ok.jsonl"
printf '{"key":"a"}\n{"key":"a"}\n'        > "$K/dupes.jsonl"
printf '{"key":"a"}\nNOT JSON\n'           > "$K/badjson.jsonl"
printf '{"key":"a"}\n{"other":1}\n'        > "$K/nokey.jsonl"
printf '{"key":"a"}\n5\n'                  > "$K/notobject.jsonl"
printf '{"key":"a"}\n[1,2]\n'              > "$K/listrec.jsonl"
printf '{"key":"a"}\n{"key":{"x":1}}\n'    > "$K/unhashable.jsonl"
: > "$K/empty.jsonl"
printf '{"key":"a"}\n'                     > "$K/unreadable.jsonl"; chmod 000 "$K/unreadable.jsonl"
cassette_key_check "$K/ok.jsonl" >/dev/null 2>&1;         assert_rc 0 $? "clean cassette"
cassette_key_check "$K/dupes.jsonl" >/dev/null 2>&1;      assert_rc 1 $? "duplicate keys -> 1"
cassette_key_check "$K/badjson.jsonl" >/dev/null 2>&1;    assert_rc 2 $? "invalid JSON -> 2, not 1"
cassette_key_check "$K/nokey.jsonl" >/dev/null 2>&1;      assert_rc 2 $? "missing key field -> 2"
cassette_key_check "$K/notobject.jsonl" >/dev/null 2>&1;  assert_rc 2 $? "scalar record -> 2"
cassette_key_check "$K/listrec.jsonl" >/dev/null 2>&1;    assert_rc 2 $? "list record -> 2"
cassette_key_check "$K/unhashable.jsonl" >/dev/null 2>&1; assert_rc 2 $? "non-string key -> 2"
cassette_key_check "$K/empty.jsonl" >/dev/null 2>&1;      assert_rc 0 $? "empty cassette is not an error"
if [ "$(id -u)" != "0" ]; then
  cassette_key_check "$K/unreadable.jsonl" >/dev/null 2>&1; assert_rc 2 $? "unreadable file -> 2"
else
  ok "unreadable file -> 2 (skipped: running as root)"
fi

echo "== _common.sh: sha256_of and backup_if_present =="
printf 'same\n' > "$K/x.jsonl"; printf 'same\n' > "$K/y.jsonl"; printf 'diff\n' > "$K/z.jsonl"
assert_eq "$(sha256_of "$K/x.jsonl")" "$(sha256_of "$K/y.jsonl")" "identical files hash equal"
if [ "$(sha256_of "$K/x.jsonl")" != "$(sha256_of "$K/z.jsonl")" ]; then
  ok "different files hash differently"
else
  bad "different files hash differently"
fi
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

echo "== 06: abort conditions are previewed, not just hit =="
REPO4="$WORKROOT/repo4"; make_fake_repo "$REPO4"
make_cassette "$REPO4/anthropic-open-baseline-$H.jsonl" 228 same
cp "$REPO4/anthropic-open-baseline-$H.jsonl" "$REPO4/anthropic-open-candidate-$H.jsonl"
make_cassette "$REPO4/qwen3.6-27b-open.jsonl" 228 q
out="$(run06 "$REPO4" MODES="open" DRY_RUN=1)"
assert_contains "WOULD ABORT" "$out" "identical hosted cassettes are flagged in DRY_RUN"
assert_contains "byte-identical" "$out" "the identity abort names its reason"
# A dummy key is required: the real path checks the key before the cassette guards, so
# without one this would die early and the guard would go untested.
out="$(run06 "$REPO4" MODES="open" ANTHROPIC_API_KEY=dummy)"
assert_contains "byte-identical" "$out" "a real run aborts on identical hosted cassettes"

REPO5="$WORKROOT/repo5"; make_fake_repo "$REPO5"
make_cassette "$REPO5/anthropic-open-baseline-$H.jsonl" 228 b
make_cassette "$REPO5/anthropic-open-candidate-$H.jsonl" 228 c
python3 -c "
import sys
with open(sys.argv[1], 'w') as f:
    for i in range(228):
        f.write('{\"key\": \"dup%d\"}\n' % (i % 50))
" "$REPO5/qwen3.6-27b-open.jsonl"
out="$(run06 "$REPO5" MODES="open" DRY_RUN=1)"
assert_contains "duplicate keys" "$out" "duplicate-keyed cassette flagged in DRY_RUN"
out="$(run06 "$REPO5" MODES="open" ANTHROPIC_API_KEY=dummy)"
assert_contains "duplicate keys" "$out" "a real run aborts on duplicate keys"

REPO6="$WORKROOT/repo6"; make_fake_repo "$REPO6"
make_cassette "$REPO6/anthropic-open-baseline-$H.jsonl" 228 b
make_cassette "$REPO6/anthropic-open-candidate-$H.jsonl" 228 c
printf '%s\n' "$(python3 -c "print('\n'.join('{\"key\": \"q%d\"}' % i for i in range(227)))")" > "$REPO6/qwen3.6-27b-open.jsonl"
printf 'NOT JSON\n' >> "$REPO6/qwen3.6-27b-open.jsonl"
out="$(run06 "$REPO6" MODES="open" DRY_RUN=1)"
assert_contains "corrupt or truncated" "$out" "corrupt cassette diagnosed as corrupt in DRY_RUN"
assert_not_contains "has duplicate keys ($REPO6/qwen" "$out" "corrupt is not misreported as duplicated"

echo "== 06: ontology fixture required only when a mode needs it =="
REPO7="$WORKROOT/repo7"; make_fake_repo "$REPO7"
rm -f "$REPO7/crates/core/tests/fixtures/real_corpus_wal/ontology.yaml"
out="$(run06 "$REPO7" MODES="open" ANTHROPIC_API_KEY=dummy)"
assert_contains "ontology fixture not found" "$out" "a non-freeform mode requires the fixture"
out="$(run06 "$REPO7" MODES="freeform" ANTHROPIC_API_KEY=dummy)"
assert_not_contains "ontology fixture not found" "$out" "freeform does not require the fixture"

echo
if [ "$FAIL" -eq 0 ]; then
  printf '\033[32mall %d checks passed\033[0m\n' "$PASS"
  exit 0
fi
printf '\033[31m%d of %d checks FAILED\033[0m\n' "$FAIL" "$((PASS + FAIL))"
exit 1
