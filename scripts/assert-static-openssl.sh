#!/usr/bin/env bash
# assert-static-openssl.sh — fail if a built binary carries a dynamic OpenSSL.
#
# Usage: scripts/assert-static-openssl.sh <path-to-binary> [...]
#
# The release artifact must be self-contained: no libssl/libcrypto in its
# dynamic-link list. lbug's build.rs emits -lssl/-lcrypto unconditionally
# (LadybugDB/ladybug#590), so the *only* thing keeping the link static is
# scripts/stage-openssl-static.sh having run first. If someone removes that
# step, the build still succeeds and the failure surfaces later — on a user's
# machine, at dyld/ld.so time. This is the guard that turns that into a red CI
# job instead.
#
# See docs/adr/0398-openssl-linkage-for-release-artifacts.md.

set -euo pipefail

[[ $# -ge 1 ]] || {
  echo "usage: $0 <path-to-binary> [...]" >&2
  exit 2
}

status=0

for bin in "$@"; do
  if [[ ! -f "$bin" ]]; then
    echo "assert-static-openssl.sh: '$bin' does not exist" >&2
    status=1
    continue
  fi

  # A missing tool must NOT read as "no OpenSSL found" — an empty `deps` greps
  # clean and would report OK on a binary nobody actually inspected, which is the
  # one failure mode this guard cannot afford.
  case "$(uname -s)" in
    Darwin) tool=otool ;;
    *) tool=ldd ;;
  esac
  command -v "$tool" >/dev/null 2>&1 || {
    echo "assert-static-openssl.sh: FAIL — '$tool' not found; cannot inspect '$bin'." >&2
    echo "  Refusing to report a pass on an uninspected binary. Install $tool" >&2
    echo "  (macOS: Xcode command line tools; Linux: libc-bin) and re-run." >&2
    status=1
    continue
  }

  # Capture the tool's output AND its real exit status. Discarding both
  # (`2>/dev/null || true`) collapses "the tool could not inspect this file" into
  # an empty `deps`, which greps clean and reports OK on a binary nobody looked
  # at — the same silent pass the `command -v` check above exists to prevent,
  # just reached with the tool present and failing rather than missing. Verified
  # reachable on all three of: a chmod-000 artifact (otool exits 1), a truncated
  # Mach-O, and a file that is not an object file (otool exits 0 but emits a
  # diagnostic instead of a dependency listing).
  set +e
  case "$tool" in
    otool) deps="$(otool -L "$bin" 2>&1)" ;;
    *) deps="$(ldd "$bin" 2>&1)" ;;
  esac
  tool_status=$?
  set -e

  inspect_error=""
  case "$tool" in
    otool)
      # otool exits 0 for a non-Mach-O or damaged file, so its status alone is
      # not sufficient — require a real dependency listing and reject the
      # diagnostics it prints in place of one.
      if [ "$tool_status" -ne 0 ]; then
        inspect_error="otool exited $tool_status"
      elif echo "$deps" | grep -qE 'is not an object file|extends past end|truncated|malformed|can.t open file'; then
        inspect_error="otool could not parse the file"
      fi
      ;;
    *)
      # A fully static binary makes ldd exit non-zero with "not a dynamic
      # executable" — that is a genuine pass. Any other non-zero exit means the
      # file was not inspected.
      if [ "$tool_status" -ne 0 ] && ! echo "$deps" | grep -qi 'not a dynamic executable'; then
        inspect_error="ldd exited $tool_status"
      fi
      ;;
  esac

  if [ -n "$inspect_error" ]; then
    echo "assert-static-openssl.sh: FAIL — could not inspect '$bin' ($inspect_error)." >&2
    echo "  Refusing to report a pass on an uninspected binary. $tool said:" >&2
    echo "$deps" | sed 's/^/    /' >&2
    status=1
    continue
  fi

  if echo "$deps" | grep -qiE 'libssl|libcrypto'; then
    echo "assert-static-openssl.sh: FAIL — '$bin' has a dynamic OpenSSL dependency:" >&2
    echo "$deps" | grep -iE 'libssl|libcrypto' >&2
    echo "" >&2
    echo "  Run scripts/stage-openssl-static.sh before building, and confirm the" >&2
    echo "  build picked up its PKG_CONFIG_PATH. See ADR-0398." >&2
    status=1
  else
    echo "assert-static-openssl.sh: OK — '$bin' has no dynamic OpenSSL dependency"
  fi
done

exit $status
