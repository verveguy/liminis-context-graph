#!/usr/bin/env bash
# stage-openssl-static.sh — force lbug's OpenSSL link to resolve statically.
#
# Usage:
#   scripts/stage-openssl-static.sh              # print the export line
#   eval "$(scripts/stage-openssl-static.sh)"    # apply it to the current shell
#
# In GitHub Actions ($GITHUB_ENV set) the export is appended to $GITHUB_ENV
# instead, so every later step in the job inherits it.
#
# Why this exists
# ---------------
# lbug 0.18.0 moved OpenSSL out of the prebuilt fat bundle
# (LadybugDB/ladybug#590, "Link against OpenSSL3"). Its build.rs now emits
#
#     cargo:rustc-link-lib=dylib=ssl
#     cargo:rustc-link-lib=dylib=crypto
#
# *unconditionally*, and derives the search path from exactly one source:
#
#     pkg-config --variable=libdir openssl
#
# As of 0.20.1, build.rs checks OPENSSL_DIR/OPENSSL_ROOT_DIR first and, if either
# is set, uses that directory directly without consulting pkg-config at all (see
# docs/adr/0398-openssl-linkage-for-release-artifacts.md's Amendment). This repo
# sets neither, so that branch is inert here and PKG_CONFIG_PATH remains the
# only lever this script needs.
#
# Left alone, the release binary picks up a dynamic libssl/libcrypto. On macOS
# it also inherits Homebrew's absolute install name
# (/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib), so the artifact fails to
# load for anyone without that exact path — the same failure upstream hit on
# their Node binding (LadybugDB/ladybug#682). That breaks the self-contained
# single-binary property the shell installer depends on.
#
# The fix: stage a directory containing *only* libssl.a and libcrypto.a, publish
# a synthesized openssl.pc whose libdir points at it, and put that directory
# first on PKG_CONFIG_PATH. lbug's build.rs then emits its rustc-link-search at
# the staging directory, and the linker — handed -lssl/-lcrypto with no .dylib
# or .so in sight — resolves both to the archives.
#
# See docs/adr/0398-openssl-linkage-for-release-artifacts.md. Deleting this step
# silently reintroduces the dynamic dependency; scripts/assert-static-openssl.sh
# is the guard that catches that.

set -euo pipefail

die() {
  echo "stage-openssl-static.sh: $*" >&2
  exit 1
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
staging_dir="${LCG_OPENSSL_STAGING_DIR:-$repo_root/target/openssl-static}"

# --- 1. Locate a real OpenSSL installation ----------------------------------
#
# LCG_OPENSSL_LIBDIR is an explicit override for environments where neither
# probe below applies. On macOS the Homebrew keg is authoritative: openssl@3 is
# keg-only, so pkg-config only resolves it when someone has symlinked the .pc
# into a searched directory, which is not true on a fresh runner. On Linux
# pkg-config is reliable because libssl-dev installs into the default path.
if [[ -n "${LCG_OPENSSL_LIBDIR:-}" ]]; then
  libdir="$LCG_OPENSSL_LIBDIR"
  source_desc="LCG_OPENSSL_LIBDIR"
elif [[ "$(uname -s)" == "Darwin" ]] && command -v brew >/dev/null 2>&1 \
  && brew --prefix openssl@3 >/dev/null 2>&1; then
  libdir="$(brew --prefix openssl@3)/lib"
  source_desc="brew --prefix openssl@3"
elif command -v pkg-config >/dev/null 2>&1 \
  && pkg-config --variable=libdir openssl >/dev/null 2>&1; then
  libdir="$(pkg-config --variable=libdir openssl)"
  source_desc="pkg-config --variable=libdir openssl"
else
  die "could not locate OpenSSL. Install it (brew install openssl@3, or
  apt-get install libssl-dev) or set LCG_OPENSSL_LIBDIR to a directory
  containing libssl.a and libcrypto.a."
fi

[[ -d "$libdir" ]] || die "OpenSSL libdir '$libdir' (from $source_desc) does not exist."

# --- 2. Require the static archives, loudly ---------------------------------
#
# Falling through to a dynamic link here would reproduce exactly the failure
# this script exists to prevent, deferred to link time or — worse — to a user's
# dyld/ld.so. Fail now, where the message is actionable.
for lib in libssl.a libcrypto.a; do
  [[ -f "$libdir/$lib" ]] || die "$lib not found in '$libdir' (from $source_desc).
  Static OpenSSL archives are required for a self-contained release artifact.
  On Debian/Ubuntu they ship in libssl-dev; on macOS in the openssl@3 keg."
done

# --- 3. Stage an archives-only lib directory --------------------------------
#
# Only the two .a files are linked in, so no .dylib/.so can be reached through
# the search path lbug emits.
rm -rf "$staging_dir"
mkdir -p "$staging_dir/pkgconfig"
ln -s "$libdir/libssl.a" "$staging_dir/libssl.a"
ln -s "$libdir/libcrypto.a" "$staging_dir/libcrypto.a"

# --- 4. Publish a pkg-config file pointing at the staging directory ---------
#
# lbug only reads `--variable=libdir`, but a well-formed .pc keeps this usable
# for anything else that consults it.
version="$(pkg-config --modversion openssl 2>/dev/null || echo "3")"
cat > "$staging_dir/pkgconfig/openssl.pc" <<EOF
prefix=$staging_dir
libdir=$staging_dir
includedir=$staging_dir/include

Name: OpenSSL
Description: Static OpenSSL archives staged by scripts/stage-openssl-static.sh
Version: $version
Libs: -L\${libdir} -lssl -lcrypto
Cflags: -I\${includedir}
EOF

new_path="$staging_dir/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

echo "stage-openssl-static.sh: staged libssl.a + libcrypto.a from $libdir ($source_desc)" >&2
echo "stage-openssl-static.sh: -> $staging_dir" >&2

if [[ -n "${GITHUB_ENV:-}" ]]; then
  # $GITHUB_ENV is parsed as literal KEY=VALUE, not by a shell, so it must NOT be
  # quoted or escaped — Actions would treat the quotes as part of the value.
  echo "PKG_CONFIG_PATH=$new_path" >> "$GITHUB_ENV"
  echo "stage-openssl-static.sh: appended PKG_CONFIG_PATH to \$GITHUB_ENV" >&2
else
  # This line is meant to be consumed by `eval`, so it must survive word splitting.
  # A checkout path containing a space (Fabrik worktrees are nested several levels
  # deep under a user-chosen directory) would otherwise be split mid-path and set
  # PKG_CONFIG_PATH to a truncated value, silently reintroducing a dynamic link.
  printf 'export PKG_CONFIG_PATH=%q\n' "$new_path"
fi
