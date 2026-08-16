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

  # A fully static or non-dynamic binary makes ldd exit non-zero ("not a dynamic
  # executable"); that is a pass, not an error, so swallow the exit code — the
  # tool ran, which is what the check above establishes.
  case "$tool" in
    otool) deps="$(otool -L "$bin" 2>/dev/null || true)" ;;
    *) deps="$(ldd "$bin" 2>/dev/null || true)" ;;
  esac

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
