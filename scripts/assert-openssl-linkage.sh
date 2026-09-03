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
#   macOS  — an absolute install_name such as
#            /opt/homebrew/opt/openssl@3/lib/libssl.3.dylib, which fails to load
#            on Intel Homebrew, MacPorts, or a Mac with no package manager.
#            Correct form is @rpath/libssl.3.dylib, resolved against the roots
#            embedded by scripts/stage-openssl-rpath.sh.
#
#   Linux  — an absolute DT_NEEDED entry. Correct form is a bare SONAME such as
#            libssl.so.3, which ld.so resolves from the system search path.
#
# lbug's build.rs emits -lssl/-lcrypto unconditionally and falls through to
# hardcoded Homebrew probe paths, so on macOS the *only* thing producing the
# correct form is stage-openssl-rpath.sh having run first. If someone drops that
# step the build still succeeds and the failure surfaces on a user's machine at
# dyld time. This is the guard that turns that into a red CI job instead.

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
      # Every OpenSSL reference must go through @rpath.
      bad="$(otool -L "$bin" | grep -E 'libssl|libcrypto' | grep -v '@rpath/' || true)"
      if [[ -n "$bad" ]]; then
        echo "assert-openssl-linkage.sh: FAIL — '$bin' hardcodes an OpenSSL path:" >&2
        echo "$bad" >&2
        echo >&2
        echo "  Run scripts/stage-openssl-rpath.sh before building, and confirm the" >&2
        echo "  build picked up its OPENSSL_DIR/PKG_CONFIG_PATH/RUSTFLAGS. See ADR-0550." >&2
        status=1
        continue
      fi
      # An @rpath reference is useless without somewhere to resolve it.
      if otool -L "$bin" | grep -qE 'libssl|libcrypto'; then
        if ! otool -l "$bin" | grep -q LC_RPATH; then
          echo "assert-openssl-linkage.sh: FAIL — '$bin' references @rpath OpenSSL but has no LC_RPATH" >&2
          status=1
          continue
        fi
      fi
      echo "assert-openssl-linkage.sh: OK — '$bin' resolves OpenSSL via @rpath"
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
