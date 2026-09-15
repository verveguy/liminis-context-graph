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
# Copies the two libraries under the names lbug asks for into a staging directory and prepends
# that directory to LIB, the MSVC linker's library search path. lbug has asked for *different* names
# across releases: 0.18.1's build.rs emits `dylib=ssl`/`dylib=crypto` (ssl.lib/crypto.lib), while
# 0.20.4's emits `dylib=libssl`/`dylib=libcrypto` (libssl.lib/libcrypto.lib — found when #572's
# 0.20.4 pin failed to link on Windows with LNK1181 'libssl.lib'). Both name forms are staged, so
# the script works across the pin without tracking which lbug release is current. LIB rather than `RUSTFLAGS=-L`:
# changing RUSTFLAGS invalidates cargo's whole build cache and collides with cargo-dist's own
# flags, while LIB is read by link.exe alone.
#
# Two kinds of <openssl-root>, told apart by whether <root>/bin holds the OpenSSL DLLs:
#
#   static  (vcpkg x64-windows-static-md — what release builds use, ADR-0581): libssl.lib and
#           libcrypto.lib are the code itself, so the binary needs no OpenSSL DLL at runtime.
#           Static OpenSSL depends on Windows system libraries lbug's build.rs never names, so
#           this also exports LINK (link.exe's extra-inputs variable) with them.
#   dynamic (vcpkg x64-windows, Shining Light): the .lib files are import libraries and the
#           runtime DLLs (libssl-3-x64.dll, libcrypto-3-x64.dll) in <root>/bin must be on PATH or
#           beside the executable, so <root>/bin is added to PATH.
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
for base in ssl crypto; do
  cp "$root/lib/lib$base.lib" "$stage/$base.lib"      # lbug 0.18.x: dylib=ssl / dylib=crypto
  cp "$root/lib/lib$base.lib" "$stage/lib$base.lib"   # lbug 0.20.x: dylib=libssl / dylib=libcrypto
done
note "staged ssl.lib/libssl.lib and crypto.lib/libcrypto.lib from $root/lib -> $stage"

# Static OpenSSL's Windows system-library dependencies (sockets, certificate store, user32 for
# its console UI hooks, advapi32 for the registry/crypto provider).
system_libs="ws2_32.lib crypt32.lib user32.lib advapi32.lib"

if compgen -G "$root/bin/libssl-3*.dll" > /dev/null; then
  kind=dynamic
else
  kind=static
fi
note "OpenSSL at $root is $kind"

stage_win="$(cygpath -w "$stage")"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "LIB=${stage_win};${LIB:-}" >> "$GITHUB_ENV"
  if [[ "$kind" == static ]]; then
    echo "LINK=${system_libs}${LINK:+ $LINK}" >> "$GITHUB_ENV"
    echo "OPENSSL_STATIC=1" >> "$GITHUB_ENV"
  else
    cygpath -w "$root/bin" >> "$GITHUB_PATH"
  fi
  note "appended to \$GITHUB_ENV/\$GITHUB_PATH"
else
  printf 'export LIB=%q\n' "${stage_win};${LIB:-}"
  if [[ "$kind" == static ]]; then
    printf 'export LINK=%q\n' "${system_libs}${LINK:+ $LINK}"
    printf 'export OPENSSL_STATIC=1\n'
  else
    printf 'export PATH=%q\n' "$root/bin:$PATH"
  fi
fi
