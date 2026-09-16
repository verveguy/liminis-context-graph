#!/usr/bin/env bash
# stage-lbug-extensions.sh — pre-place the lbug vector/fts extension binaries so Db::open never
# needs to reach extension.ladybugdb.com at startup (issue #559, ADR-0559).
#
# Usage:
#   scripts/stage-lbug-extensions.sh <platform> <dest-dir>
#
#   <platform>  one of lbug's own extension-directory platform strings: osx_arm64,
#               linux_amd64, linux_arm64, win_amd64 (see crates/core/src/lbug_extension_home.rs's
#               platform_string(), which must stay in lockstep with this script).
#   <dest-dir>  directory to stage into. This script creates
#               <dest-dir>/.lbdb/extension/<LBUG_EXTENSION_VERSION>/<platform>/{vector,fts}/ —
#               the exact layout Db::open's resolve_extension_files() (crates/core) looks for,
#               and the layout cargo-dist's `include` packages into each release archive.
#
# WHY THIS EXISTS
#
# Without a pre-staged bundle, Db::open runs INSTALL vector / INSTALL fts, which lbug resolves
# by checking <home_directory>/.lbdb/extension/<version>/<platform>/<name>/lib<name>.lbug_extension
# and downloading from the CDN if that file is absent. This script pre-stages the file at a
# location Db::open then finds (LCG_LBUG_HOME, or a directory derived from the running binary's
# own path) and loads directly via LOAD EXTENSION '<absolute path>', bypassing INSTALL (and
# home_directory) entirely — an earlier version of this mechanism instead redirected
# home_directory before letting INSTALL/LOAD EXTENSION resolve locally, but that was abandoned
# after it was found to cause silent row loss in an unrelated query path (see ADR-0559). Either
# way, this script is the one place that performs the download — at release-build or CI-build
# time, not at a user's startup — shared by .github/build-setup.yml (per-target release
# packaging) and ci.yml's build-release job (a single linux_amd64 fetch, cached and reused by
# every other CI job via LCG_LBUG_HOME).
#
# Reads the version from the repo-root LBUG_EXTENSION_VERSION file — the same file
# crates/core/src/lbug_extension_home.rs reads via include_str!() — so the version segment can
# never drift between what this script stages and what Db::open looks for.
#
# PINNED BYTES, NOT JUST "NON-EMPTY" (issue #593, ADR-0593)
#
# extension.ladybugdb.com publishes at a per-minor path and overwrites it on patch releases, so
# the bytes served here depend on *when* this script runs, not on anything in this repo. Before
# staging a downloaded file, this script hashes it and compares against the checked-in
# repo-root LBUG_EXTENSION_HASHES pin file — a mismatch (or a missing pin entry) fails the build
# before any file is staged. On a mismatch, the script also reports the highest embedded
# MAJOR.MINOR.PATCH version string found in the *received* bytes, as a diagnostic only: it tells
# a human why the hash moved, but version equality is deliberately not treated as an ABI
# compatibility signal (the currently-pinned bytes are themselves a different-version extension
# than the core they're paired with, and that pairing works). Re-pinning after a legitimate
# upstream republish is a manual, human-verified action — this script never updates its own
# pins.
#
# THIS SCRIPT IS THE ONLY LEGITIMATE WRITER of the <dest>/.lbdb/extension/<version>/<platform>/
# tree (issue #561). Db::open's resolution (lbug_extension_home.rs) is directory-existence-based
# and cannot verify that staged bytes actually correspond to the version their directory is named
# after — it only checks the directory exists and its files are non-empty. This script's own
# `$version` variable feeds both the download URL and the destination directory name below, so a
# mismatch between a directory's name and its contents is impossible as long as this script is
# the only thing that populates the tree. Never hand-copy or hand-rename files into this
# directory structure — doing so can produce a directory that looks validly staged (and loads
# without error) while actually shipping the wrong extension version, silently defeating #559's
# CDN-avoidance guarantee with nothing to catch it.

set -euo pipefail

die() { echo "stage-lbug-extensions.sh: $*" >&2; exit 1; }
note() { echo "stage-lbug-extensions.sh: $*" >&2; }

[[ $# -eq 2 ]] || die "usage: $0 <platform> <dest-dir>"
platform="$1"
dest="$2"

case "$platform" in
  osx_arm64|linux_amd64|linux_arm64|win_amd64) ;;
  *) die "unrecognized platform '$platform' — must be one of osx_arm64, linux_amd64, linux_arm64, win_amd64 (see crates/core/src/lbug_extension_home.rs's platform_string())" ;;
esac

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
version_file="$repo_root/LBUG_EXTENSION_VERSION"
[[ -f "$version_file" ]] || die "$version_file not found"
version="$(cat "$version_file")"
[[ -n "$version" ]] || die "$version_file is empty"

hashes_file="$repo_root/LBUG_EXTENSION_HASHES"
[[ -f "$hashes_file" ]] || die "$hashes_file not found"

target_dir="$dest/.lbdb/extension/$version/$platform"
mkdir -p "$target_dir"

for name in vector fts; do
  ext_dir="$target_dir/$name"
  mkdir -p "$ext_dir"
  file="lib${name}.lbug_extension"
  url="https://extension.ladybugdb.com/v${version}/${platform}/${name}/${file}"

  pinned="$(awk -v p="$platform" -v n="$name" '$1 == p && $2 == n { print $3 }' "$hashes_file")"
  [[ -n "$pinned" ]] ||
    die "no pinned sha256 for platform '$platform' extension '$name' in $hashes_file — add one after verifying the downloaded bytes by hand"
  pinned="$(printf '%s' "$pinned" | tr '[:upper:]' '[:lower:]')"

  note "fetching $url"
  tmp="$(mktemp "$ext_dir/.${file}.XXXXXX")"
  # --fail so a 404/5xx is a non-zero exit rather than an HTML error body staged as the binary;
  # no silent partial staging.
  curl --fail --silent --show-error --location --output "$tmp" "$url" || {
    rm -f "$tmp"
    die "failed to download $url"
  }

  actual="$(shasum -a 256 "$tmp" | awk '{ print $1 }' | tr '[:upper:]' '[:lower:]')"
  if [[ "$actual" != "$pinned" ]]; then
    # Not anchored to a leading "0." — a future major-version bump must still match.
    version_hint="$(grep -a -o -E '\b[0-9]+\.[0-9]+\.[0-9]+\b' "$tmp" | sort -uV | tail -1 || true)"
    rm -f "$tmp"
    if [[ -n "$version_hint" ]]; then
      die "sha256 mismatch for $platform/$name: expected $pinned, got $actual (downloaded from $url). Highest embedded version string in the received bytes: $version_hint — informational only, NOT a claim of ABI compatibility or incompatibility with the pinned lbug core. If this is a legitimate upstream republish, verify the new bytes by hand and update $hashes_file; do not simply re-run this script."
    else
      die "sha256 mismatch for $platform/$name: expected $pinned, got $actual (downloaded from $url). No embedded version string found (binary appears stripped). If this is a legitimate upstream republish, verify the new bytes by hand and update $hashes_file; do not simply re-run this script."
    fi
  fi

  mv "$tmp" "$ext_dir/$file"
  [[ -s "$ext_dir/$file" ]] || die "$ext_dir/$file was downloaded but is empty"
done

note "staged lbug $version extensions for $platform -> $target_dir"
