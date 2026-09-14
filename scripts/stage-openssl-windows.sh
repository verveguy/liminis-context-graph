#!/usr/bin/env bash
# stage-openssl-windows.sh — let MSVC link lbug's OpenSSL dependency (issue #581).
#
# Usage (Git Bash on Windows):
#   eval "$(scripts/stage-openssl-windows.sh <openssl-root>)"   # locally: apply the exports
#   bash scripts/stage-openssl-windows.sh <openssl-root>        # in CI: appends to $GITHUB_ENV
#
#   <openssl-root>  an OpenSSL 3 install with lib/libssl.lib and lib/libcrypto.lib, e.g.
#                   vcpkg's installed/x64-windows. Defaults to $OPENSSL_DIR.
#
# WHY THIS EXISTS
#
# lbug links OpenSSL 3 externally since 0.18.0 (LadybugDB/ladybug#590), and its build.rs emits
# `cargo:rustc-link-lib=dylib=ssl` and `=crypto` unconditionally, with a search path only from
# pkg-config (absent on Windows). On MSVC those directives mean the import libraries `ssl.lib`
# and `crypto.lib` — but every OpenSSL 3 distribution for Windows (vcpkg, the Shining Light
# installers, a source build) names them `libssl.lib` and `libcrypto.lib`. The link fails with
# `LNK1181: cannot open input file 'ssl.lib'` even when OpenSSL is installed and OPENSSL_DIR is
# set (OPENSSL_DIR is read by openssl-sys, not by lbug).
#
# HOW
#
# Copies the two import libraries under the names lbug asks for into a staging directory and
# prepends that directory to LIB, the MSVC linker's library search path. LIB rather than
# `RUSTFLAGS=-L`: changing RUSTFLAGS invalidates cargo's whole build cache and collides with
# cargo-dist's own flags, while LIB is read by link.exe alone. The staged copies are link-time
# fixtures; the runtime DLLs (libssl-3-x64.dll, libcrypto-3-x64.dll, in <openssl-root>/bin) must
# still be on PATH or beside the executable.
#
# Mirrors scripts/stage-openssl-rpath.sh (the macOS equivalent, ADR-0550). A no-op elsewhere.

set -euo pipefail

die() { echo "stage-openssl-windows.sh: $*" >&2; exit 1; }
note() { echo "stage-openssl-windows.sh: $*" >&2; }

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ;;
  *) note "not Windows — nothing to do"; exit 0 ;;
esac

root="${1:-${OPENSSL_DIR:-}}"
[[ -n "$root" ]] || die "usage: $0 <openssl-root> (or set OPENSSL_DIR)"
root="$(cygpath -u "$root")"
for lib in libssl.lib libcrypto.lib; do
  [[ -f "$root/lib/$lib" ]] || die "$root/lib/$lib not found — is <openssl-root> an OpenSSL 3 install?"
done

stage="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/lcg-openssl-link"
mkdir -p "$stage"
cp "$root/lib/libssl.lib" "$stage/ssl.lib"
cp "$root/lib/libcrypto.lib" "$stage/crypto.lib"
note "staged ssl.lib and crypto.lib from $root/lib -> $stage"

stage_win="$(cygpath -w "$stage")"
bin_win="$(cygpath -w "$root/bin")"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "LIB=${stage_win};${LIB:-}" >> "$GITHUB_ENV"
  echo "$bin_win" >> "$GITHUB_PATH"
  note "appended LIB and PATH to \$GITHUB_ENV/\$GITHUB_PATH"
else
  printf 'export LIB=%q\n' "${stage_win};${LIB:-}"
  printf 'export PATH=%q\n' "$root/bin:$PATH"
fi
