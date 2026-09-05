#!/usr/bin/env bash
# assert-openssl-linkage.sh — fail if a built binary hardcodes an OpenSSL path.
#
# Usage: scripts/assert-openssl-linkage.sh <path-to-binary> [...]
#
# lcg links OpenSSL 3 *dynamically*, supplied by the user's package manager, so
# that OpenSSL security updates reach our users without us cutting a release for
# every CVE (ADR-0550; upstream states the same position in
# LadybugDB/ladybug#681). What must never ship is a binary that names a specific
# machine's OpenSSL:
#
#   macOS  — @rpath/libssl.3.dylib is the ideal: relocatable across Homebrew ARM,
#            Homebrew Intel and MacPorts. A package manager's *stable* prefix
#            (/opt/homebrew/opt/openssl@3/lib, /usr/local/opt/openssl@3/lib,
#            /opt/local/lib) is also accepted — lcg documents Homebrew OpenSSL 3
#            as a macOS prerequisite, so such a binary loads on any Mac that met
#            it. What is rejected is a path no user can be expected to have: a
#            version-pinned Cellar path like
#            /opt/homebrew/Cellar/openssl@3/3.6.3/lib, which breaks on the next
#            openssl patch release even on the machine that built it.
#
#   Linux  — an absolute DT_NEEDED entry. Correct form is a bare SONAME such as
#            libssl.so.3, which ld.so resolves from the system search path.
#
# lbug's build.rs emits -lssl/-lcrypto unconditionally and falls through to
# hardcoded Homebrew probe paths. scripts/stage-openssl-rpath.sh gets the
# @rpath form when it takes effect; under `dist build` on a GitHub runner it
# does not, and the link resolves through liblbug.a's own LC_LINKER_OPTION
# records to the Homebrew prefix instead (#550, still open). Either outcome
# ships a binary that works on a Mac with Homebrew OpenSSL 3; neither may ship a
# Cellar path. This guard draws that line.

set -euo pipefail

[[ $# -ge 1 ]] || { echo "usage: $0 <path-to-binary> [...]" >&2; exit 2; }

status=0
os="$(uname -s)"

for bin in "$@"; do
  if [[ ! -f "$bin" ]]; then
    echo "assert-openssl-linkage.sh: '$bin' does not exist" >&2
    status=1
    continue
  fi

  case "$os" in
    Darwin)
      refs="$(otool -L "$bin" | grep -E 'libssl|libcrypto' || true)"
      # @rpath, or a package manager's stable prefix. Anything else — a Cellar
      # path above all — names a directory the user has no reason to have.
      bad="$(echo "$refs" | grep -vE '^\s*(@rpath/|/opt/homebrew/opt/openssl@3/lib/|/usr/local/opt/openssl@3/lib/|/opt/local/lib/)' | grep -E 'libssl|libcrypto' || true)"
      if [[ -n "$bad" ]]; then
        echo "assert-openssl-linkage.sh: FAIL — '$bin' names an OpenSSL path no user can rely on:" >&2
        echo "$bad" >&2
        echo >&2
        echo "  Accepted: @rpath/..., or a stable prefix (/opt/homebrew/opt/openssl@3/lib," >&2
        echo "  /usr/local/opt/openssl@3/lib, /opt/local/lib). A versioned Cellar path" >&2
        echo "  breaks on the next openssl patch release. See ADR-0550." >&2
        status=1
        continue
      fi
      # An @rpath reference is useless without somewhere to resolve it.
      if echo "$refs" | grep -q '@rpath/'; then
        if ! otool -l "$bin" | grep -q LC_RPATH; then
          echo "assert-openssl-linkage.sh: FAIL — '$bin' references @rpath OpenSSL but has no LC_RPATH" >&2
          status=1
          continue
        fi
        echo "assert-openssl-linkage.sh: OK — '$bin' resolves OpenSSL via @rpath"
      elif [[ -n "$refs" ]]; then
        echo "assert-openssl-linkage.sh: OK — '$bin' resolves OpenSSL via a package-manager prefix:"
        echo "$refs"
      else
        echo "assert-openssl-linkage.sh: OK — '$bin' references no OpenSSL"
      fi
      ;;
    Linux)
      # readelf is in binutils; objdump is the fallback.
      if command -v readelf >/dev/null 2>&1; then
        needed="$(readelf -d "$bin" 2>/dev/null | grep NEEDED || true)"
      else
        needed="$(objdump -p "$bin" 2>/dev/null | grep NEEDED || true)"
      fi
      bad="$(echo "$needed" | grep -E 'libssl|libcrypto' | grep '/' || true)"
      if [[ -n "$bad" ]]; then
        echo "assert-openssl-linkage.sh: FAIL — '$bin' hardcodes an OpenSSL path:" >&2
        echo "$bad" >&2
        status=1
        continue
      fi
      echo "assert-openssl-linkage.sh: OK — '$bin' resolves OpenSSL via SONAME"
      ;;
    *)
      echo "assert-openssl-linkage.sh: unsupported OS '$os' — skipping '$bin'" >&2
      ;;
  esac
done

exit "$status"
