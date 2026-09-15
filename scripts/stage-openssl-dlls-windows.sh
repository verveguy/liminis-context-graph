#!/usr/bin/env bash
# stage-openssl-dlls-windows.sh — bundle the OpenSSL DLLs lbug's win_amd64 extensions import (#581).
#
# Usage:
#   scripts/stage-openssl-dlls-windows.sh <openssl-root> <bundle-root>
#
#   <openssl-root>  a *dynamic* OpenSSL 3 install with bin/libssl-3-x64.dll and
#                   bin/libcrypto-3-x64.dll — e.g. vcpkg's installed/x64-windows (not the
#                   x64-windows-static-md triplet the main binary links, which has no DLLs).
#   <bundle-root>   the directory holding .lbdb/ — the same <dest-dir> given to
#                   scripts/stage-lbug-extensions.sh win_amd64, which must have run first.
#
# WHY THIS EXISTS
#
# lbug's prebuilt win_amd64 extension libraries (libvector/libfts.lbug_extension) are DLLs that
# import libssl-3-x64.dll and libcrypto-3-x64.dll. The main binary links OpenSSL statically
# (ADR-0581), but that does nothing for a separately loaded DLL: lbug loads each extension with a
# plain LoadLibraryW, and Windows resolves the extension's dependencies from the exe's directory,
# the system directories and PATH. On a machine with no OpenSSL DLLs on PATH, Db::open fails with
# "Failed to load library: ...libvector.lbug_extension ... The specified module could not be found."
#
# This copies both DLLs into <bundle-root>/.lbdb/extension/<LBUG_EXTENSION_VERSION>/win_amd64/,
# the directory Cargo.toml's `include = [".lbdb"]` already packages per target, and which
# lbug_extension_home::expose_extension_dependencies adds to the DLL search path (SetDllDirectoryW)
# before LOAD EXTENSION. Works identically for the release archive and an LCG_LBUG_HOME bundle.

set -euo pipefail

die() { echo "stage-openssl-dlls-windows.sh: $*" >&2; exit 1; }
note() { echo "stage-openssl-dlls-windows.sh: $*" >&2; }

[[ $# -eq 2 ]] || die "usage: $0 <openssl-root> <bundle-root>"
root="$1"
dest="$2"
if command -v cygpath >/dev/null 2>&1; then
  root="$(cygpath -u "$root")"
  dest="$(cygpath -u "$dest")"
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
version="$(cat "$script_dir/../LBUG_EXTENSION_VERSION")"
[[ -n "$version" ]] || die "LBUG_EXTENSION_VERSION is empty"

target_dir="$dest/.lbdb/extension/$version/win_amd64"
[[ -d "$target_dir" ]] || die "$target_dir not found — run scripts/stage-lbug-extensions.sh win_amd64 first"

for dll in libssl-3-x64.dll libcrypto-3-x64.dll; do
  [[ -f "$root/bin/$dll" ]] || die "$root/bin/$dll not found — <openssl-root> must be a dynamic OpenSSL 3 install (e.g. vcpkg installed/x64-windows)"
  cp "$root/bin/$dll" "$target_dir/$dll"
done

note "staged libssl-3-x64.dll and libcrypto-3-x64.dll from $root/bin -> $target_dir"
