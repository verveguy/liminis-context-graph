#!/usr/bin/env bash
# stage-openssl-rpath.sh — make macOS binaries resolve OpenSSL through @rpath.
#
# Usage:
#   eval "$(scripts/stage-openssl-rpath.sh)"   # locally: apply the exports
#   bash scripts/stage-openssl-rpath.sh        # in CI: appends to $GITHUB_ENV
#
# WHY THIS EXISTS
#
# lbug links OpenSSL 3 externally since 0.18.0 (LadybugDB/ladybug#590) and its
# build.rs emits -lssl/-lcrypto as *dylib*. On macOS a Mach-O records the
# absolute install_name of whatever dylib it linked against, so a stock build
# bakes in the build machine's Homebrew path:
#
#   /opt/homebrew/opt/openssl@3/lib/libssl.3.dylib
#
# That binary then fails to load on any Mac without Homebrew at that exact
# prefix — Intel Homebrew, MacPorts, or no package manager at all. Upstream hit
# the identical bug in their Node addon (LadybugDB/ladybug#681) and fixed it the
# way this script does (LadybugDB/ladybug#682): resolve through @rpath, with
# several known OpenSSL roots embedded as search paths.
#
# HOW
#
# Copies the keg's dylibs into a staging directory and rewrites their *own*
# install_name to @rpath/... with install_name_tool. Linking against those
# copies makes the linker record `@rpath/libssl.3.dylib` in our binary rather
# than an absolute path. The staged copies are a link-time fixture only — they
# are never shipped; at runtime the loader resolves @rpath against the roots
# embedded below, which point at the user's own maintained OpenSSL.
#
# Doing this at link time (rather than install_name_tool on the finished binary)
# matters for the release: `dist build` builds *and packages* in one step, so a
# post-build rewrite would leave the tarball holding the un-rewritten binary.
#
# NOT STATIC LINKING — and deliberately so. See ADR-0550.
#
# Linux needs none of this: ELF records a SONAME (libssl.so.3), which ld.so
# resolves from the system search path at load time, so a Linux binary is
# already relocatable. This script is a no-op there.

set -euo pipefail

die() { echo "stage-openssl-rpath.sh: $*" >&2; exit 1; }
note() { echo "stage-openssl-rpath.sh: $*" >&2; }

# The roots @rpath is resolved against at runtime, in order. Homebrew ARM,
# Homebrew Intel, MacPorts — the same three upstream settled on.
RPATH_ROOTS=(
  /opt/homebrew/opt/openssl@3/lib
  /usr/local/opt/openssl@3/lib
  /opt/local/lib
)

if [[ "$(uname -s)" != "Darwin" ]]; then
  note "not macOS — nothing to do (ELF SONAME linkage is already relocatable)"
  exit 0
fi

command -v install_name_tool >/dev/null 2>&1 ||
  die "install_name_tool not found (install Xcode command line tools)"

keg="$(brew --prefix openssl@3 2>/dev/null || true)"
[[ -n "$keg" && -d "$keg/lib" ]] ||
  die "could not locate an openssl@3 keg via 'brew --prefix openssl@3'.
  Install it (brew install openssl@3) — it is needed at build time even though
  the shipped binary resolves OpenSSL from the user's own installation."
libdir="$keg/lib"

staging_dir="$(pwd)/target/openssl-rpath"
rm -rf "$staging_dir"
mkdir -p "$staging_dir/pkgconfig"

for lib in libssl.3.dylib libcrypto.3.dylib; do
  [[ -f "$libdir/$lib" ]] || die "$lib not found in '$libdir'"
  cp "$libdir/$lib" "$staging_dir/$lib"
  chmod u+w "$staging_dir/$lib"
  # The install_name the *linker copies into our binary*. This is the whole point.
  install_name_tool -id "@rpath/$lib" "$staging_dir/$lib"
done

# -lssl / -lcrypto resolve through the unversioned symlinks.
ln -sf libssl.3.dylib "$staging_dir/libssl.dylib"
ln -sf libcrypto.3.dylib "$staging_dir/libcrypto.dylib"

cat > "$staging_dir/pkgconfig/openssl.pc" <<EOF
prefix=$staging_dir
libdir=$staging_dir
includedir=$keg/include

Name: OpenSSL
Description: OpenSSL dylibs staged with @rpath install names by scripts/stage-openssl-rpath.sh
Version: $(pkg-config --modversion openssl 2>/dev/null || echo "3")
Libs: -L\${libdir} -lssl -lcrypto
Cflags: -I\${includedir}
EOF

# Which lever matters depends on the pinned lbug (see ADR-0550's table):
# 0.18.1's build.rs reads only pkg-config and has no Homebrew fallback, so
# PKG_CONFIG_PATH alone suffices and OPENSSL_DIR is inert. 0.20.1 checks
# OPENSSL_DIR first and *returns*, while its pkg-config branch falls through to
# hardcoded Homebrew paths — putting the real keg, absolute install_names and
# all, back on the search path. Set both, so this is correct either way.
pkg_path="$staging_dir/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

rpath_flags=""
for root in "${RPATH_ROOTS[@]}"; do
  rpath_flags="$rpath_flags -C link-arg=-Wl,-rpath,$root"
done
rustflags="${RUSTFLAGS:+$RUSTFLAGS}$rpath_flags"

note "staged @rpath dylibs from $libdir -> $staging_dir"
note "rpath roots: ${RPATH_ROOTS[*]}"

if [[ -n "${GITHUB_ENV:-}" ]]; then
  # $GITHUB_ENV is literal KEY=VALUE — no quoting or escaping.
  echo "PKG_CONFIG_PATH=$pkg_path" >> "$GITHUB_ENV"
  echo "OPENSSL_DIR=$staging_dir" >> "$GITHUB_ENV"
  echo "RUSTFLAGS=$rustflags" >> "$GITHUB_ENV"
  note "appended PKG_CONFIG_PATH, OPENSSL_DIR and RUSTFLAGS to \$GITHUB_ENV"
else
  # Consumed by `eval`, so it must survive word splitting on paths with spaces.
  printf 'export PKG_CONFIG_PATH=%q\n' "$pkg_path"
  printf 'export OPENSSL_DIR=%q\n' "$staging_dir"
  printf 'export RUSTFLAGS=%q\n' "$rustflags"
fi
