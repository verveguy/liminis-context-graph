#!/usr/bin/env bash
# Shared preamble for the #248 benchmark scripts. Sourced, not executed.
#
# Everything here exists because the same mistake was otherwise repeated in five scripts:
# resolving the repo, proving the local server is the one we think it is, checking that a
# curl actually succeeded, and not clobbering a capture that cost hours.

# Repo root, resolved from this file's location so the scripts run from any checkout.
# (They previously hardcoded one developer's absolute path.)
#
# LCG_EVAL_REPO overrides it, which matters more than it looks: cassettes and the release
# binary live in whichever checkout produced them, so running a *worktree's* copy of these
# scripts would otherwise target the worktree — no cassettes, no binary. That is exactly
# the situation when iterating on the scripts while a captured run waits in the main
# checkout.
_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [ -n "${LCG_EVAL_REPO:-}" ]; then
  REPO="$(cd -- "$LCG_EVAL_REPO" && pwd)" || exit 1
else
  REPO="$(cd -- "$_SCRIPT_DIR/../../.." && pwd)"
fi

MODEL="${LCG_EVAL_MODEL:-mlx-community/Qwen3.6-27B-4bit}"
PORT="${LCG_EVAL_PORT:-8765}"
URL="http://127.0.0.1:$PORT/v1/chat/completions"

# A stable, private work directory. Deliberately NOT mktemp: a capture runs for hours and
# has already been killed and resumed once, so its log and report must be findable at a
# known path afterwards. Ownership + 0700 is the part that actually matters on a shared
# machine; an unpredictable name would only obscure it.
WORK="${LCG_EVAL_WORK:-/tmp/eval248}"
mkdir -p "$WORK"
# Ownership is checked BEFORE chmod, not after. Against a directory owned by someone else
# `chmod` itself fails with permission denied and `set -e` kills the script first, so the
# guard never fired on the one case it exists for — the operator got a bare chmod error
# instead of the message telling them to set LCG_EVAL_WORK.
if [ ! -O "$WORK" ]; then
  echo "ERROR: $WORK exists but is not owned by you — refusing to use it." >&2
  echo "       Set LCG_EVAL_WORK to a directory you own." >&2
  exit 1
fi
chmod 700 "$WORK"

BIN="$REPO/target/release/lcg-eval"

# --- helpers ----------------------------------------------------------------------

die() { echo "ERROR: $*" >&2; exit 1; }

require_release_binary() {
  [ -x "$BIN" ] || die "$BIN not found. Build it first:
       cargo build --release -p lcg-eval"
}

# Move a file aside rather than letting the cassette writer append to it. The writer opens
# create+append and never truncates (pinned by
# writer_appends_without_truncating_across_multiple_opens), so re-running over an existing
# cassette silently doubles it. $$ is in the suffix because a bare one-second timestamp can
# collide between two rapid runs and overwrite the previous backup.
backup_if_present() {
  local f="$1"
  if [ -s "$f" ]; then
    local stamp dest
    stamp="$(date +%Y%m%d-%H%M%S)"
    dest="$f.$stamp.$$.bak"
    mv "$f" "$dest"
    echo "==> moved existing file aside: $(basename "$dest")"
  fi
}

# POST to the local server and fail loudly on transport OR HTTP OR API-level errors.
#
# `curl -s` alone exits 0 on a 4xx/5xx, so an error body with no `usage` field used to be
# reported as "0.0s for 0 tokens -> 0.0 tok/s" — a plausible-looking zero from the one
# script whose entire job is catching problems before a multi-hour capture.
api_post() {
  local body_file="$1" out_file="$2" timeout="${3:-900}"
  curl -fsS --max-time "$timeout" "$URL" \
    -H 'content-type: application/json' \
    -d @"$body_file" > "$out_file" \
    || die "request to $URL failed (transport or HTTP status). Is the server up?
       Check the log: $WORK/mlx-server.log"
  python3 - "$out_file" <<'PY' || exit 1
import json, sys
try:
    d = json.load(open(sys.argv[1]), strict=False)
except Exception as e:
    sys.exit(f"ERROR: response was not JSON: {e}")
if isinstance(d, dict) and d.get("error"):
    sys.exit(f"ERROR: server returned an API error: {d['error']}")
if not (d.get("choices") and d.get("usage")):
    sys.exit(f"ERROR: response missing choices/usage — cannot trust any timing from it:\n{json.dumps(d)[:400]}")
PY
}

# Prove the server on $PORT is the model we intend AND has thinking disabled.
#
# Reachability alone is not enough. An operator running 03 or 04 directly against a server
# left over from a reboot or an earlier session — one never started by 01 — would otherwise
# repeat the 15-hour thinking-mode capture these scripts exist to prevent. That check
# belongs at the point of use, not only at the point of starting the server.
require_server_healthy() {
  curl -fsS --max-time 5 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 \
    || die "no server reachable on port $PORT. Run 01-start-server.sh first."

  local probe="$WORK/health-probe.json"
  cat > "$WORK/health-req.json" <<EOF
{"model": "$MODEL",
 "messages": [{"role": "user", "content": "Extract entities as JSON from: A rocket exploded, delaying NASA."}],
 "max_tokens": 2048, "temperature": 0.0}
EOF
  api_post "$WORK/health-req.json" "$probe" 300

  python3 - "$probe" "$MODEL" <<'PY' || exit 1
import json, sys
d = json.load(open(sys.argv[1]), strict=False)
want = sys.argv[2]
msg = (d.get("choices") or [{}])[0].get("message", {})
reasoning = msg.get("reasoning") or ""
served = d.get("model", "")
if reasoning:
    sys.exit(
        f"ERROR: thinking mode is ON ({len(reasoning)} reasoning chars).\n"
        "       A full capture in this state projects to 15+ hours and records the\n"
        "       known-bad configuration. Re-run 01-start-server.sh."
    )
# mlx_lm.server treats an unrecognised id as a HuggingFace repo to download, so a typo'd
# model does not error at startup — it fails every single call with Repository Not Found
# and leaves an empty cassette behind.
if served and want.split("/")[-1].lower() not in served.lower():
    sys.exit(f"ERROR: server is serving '{served}', not '{want}'.")
print(f"    server healthy: {served or want}, thinking off")
PY
}
