#!/usr/bin/env bash
# stage-lbug-extensions.sh — pre-place the lbug vector/fts extension binaries so Db::open never
# needs to reach extension.ladybugdb.com at startup (issue #559, ADR-0559).
#
# Usage:
#   scripts/stage-lbug-extensions.sh <platform> <dest-dir>
#
#   <platform>  one of lbug's own extension-directory platform strings: osx_arm64,
#               linux_amd64, linux_arm64 (see crates/core/src/lbug_extension_home.rs's
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

set -euo pipefail

die() { echo "stage-lbug-extensions.sh: $*" >&2; exit 1; }
note() { echo "stage-lbug-extensions.sh: $*" >&2; }

[[ $# -eq 2 ]] || die "usage: $0 <platform> <dest-dir>"
platform="$1"
dest="$2"

case "$platform" in
  osx_arm64|linux_amd64|linux_arm64) ;;
  *) die "unrecognized platform '$platform' — must be one of osx_arm64, linux_amd64, linux_arm64 (see crates/core/src/lbug_extension_home.rs's platform_string())" ;;
esac

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
version_file="$repo_root/LBUG_EXTENSION_VERSION"
[[ -f "$version_file" ]] || die "$version_file not found"
version="$(cat "$version_file")"
[[ -n "$version" ]] || die "$version_file is empty"

target_dir="$dest/.lbdb/extension/$version/$platform"
mkdir -p "$target_dir"

for name in vector fts; do
  ext_dir="$target_dir/$name"
  mkdir -p "$ext_dir"
  file="lib${name}.lbug_extension"
  url="https://extension.ladybugdb.com/v${version}/${platform}/${name}/${file}"
  note "fetching $url"
  # --fail so a 404/5xx is a non-zero exit rather than an HTML error body staged as the binary;
  # no silent partial staging.
  curl --fail --silent --show-error --location --output "$ext_dir/$file" "$url" ||
    die "failed to download $url"
  [[ -s "$ext_dir/$file" ]] || die "$ext_dir/$file was downloaded but is empty"
done

note "staged lbug $version extensions for $platform -> $target_dir"
