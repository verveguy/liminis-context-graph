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

  case "$(uname -s)" in
    Darwin) deps="$(otool -L "$bin" 2>/dev/null || true)" ;;
    # A fully static or non-dynamic binary makes ldd exit non-zero ("not a
    # dynamic executable"); that is a pass, not an error, so swallow it.
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
