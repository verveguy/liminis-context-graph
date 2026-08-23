#!/usr/bin/env bash
# docs-publish-build.sh — Build one release's docs/ tree (versioned, and root
# when it's the latest stable) and copy the result into a persistent gh-pages
# accumulator working directory.
#
# Usage: scripts/docs-publish-build.sh <docs-source-dir> <version> <is-latest-stable> <gh-pages-dir>
#   docs-source-dir    Path to a checkout containing a docs/ tree to build,
#                      already at the target ref (this script does not check
#                      anything out itself).
#   version            Version string without a leading "v" (e.g. "0.13.3").
#   is-latest-stable   "true" or "false" -- whether this version should also
#                      be promoted to the site root.
#   gh-pages-dir       Path to a checkout of the gh-pages accumulator branch.
#
# Only <gh-pages-dir>/v<version>/ (and root, when is-latest-stable) are ever
# touched -- every other entry in <gh-pages-dir> is left exactly as-is. That is
# what makes "publish a new version" and "never disturb an old one" (FR-003)
# true by construction rather than something this script has to be careful
# about.
#
# Requires the site's Node dependencies already installed
# (<docs-source-dir>/site, e.g. via `pnpm install --frozen-lockfile`).

set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <docs-source-dir> <version> <is-latest-stable> <gh-pages-dir>" >&2
  exit 1
fi

DOCS_SRC="$(cd "$1" && pwd)"
VERSION="$2"
IS_LATEST_STABLE="$3"
GH_PAGES_DIR="$(cd "$4" && pwd)"

if [[ ! "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$ ]]; then
  echo "error: version '${VERSION}' does not look like X.Y.Z" >&2
  exit 1
fi
if [[ "${IS_LATEST_STABLE}" != "true" && "${IS_LATEST_STABLE}" != "false" ]]; then
  echo "error: is-latest-stable must be 'true' or 'false', got '${IS_LATEST_STABLE}'" >&2
  exit 1
fi

SITE_DIR="${DOCS_SRC}/site"
if [[ ! -f "${SITE_DIR}/package.json" ]]; then
  echo "error: ${SITE_DIR}/package.json not found -- this ref predates the Astro docs site." >&2
  echo "Tags from before that migration were built with Jekyll and cannot be rebuilt under" >&2
  echo "this scheme; their published copies under gh-pages/v*/ are left untouched." >&2
  exit 1
fi

# DOCS_VERSION labels the build with the version it documents, rather than
# whatever Cargo.toml at this ref happens to say. A no-op when building straight
# from the matching tag (the values already agree), and the mechanism that makes
# the FR-006 republish procedure work when building from a corrected main/branch
# commit instead. The Jekyll build did this by sed-patching a checked-in
# _config.yml; an environment variable does the same job without editing a file
# in the working tree.
build_site() {
  local baseurl="$1" dest="$2"
  ( cd "${SITE_DIR}" \
      && DOCS_BASE="${baseurl}" DOCS_VERSION="${VERSION}" pnpm build --outDir "${dest}" )
}

VERSIONED_DEST="$(mktemp -d)"
build_site "/liminis-context-graph/v${VERSION}" "${VERSIONED_DEST}"

rm -rf "${GH_PAGES_DIR:?}/v${VERSION}"
mkdir -p "${GH_PAGES_DIR}/v${VERSION}"
cp -R "${VERSIONED_DEST}/." "${GH_PAGES_DIR}/v${VERSION}/"
rm -rf "${VERSIONED_DEST}"

if [[ "${IS_LATEST_STABLE}" == "true" ]]; then
  ROOT_DEST="$(mktemp -d)"
  build_site "/liminis-context-graph" "${ROOT_DEST}"

  # Replace everything at gh-pages root except the accumulated v*/ version
  # directories -- this preserves FR-003 even for the root copy. Also spare
  # .git: this directory is a git worktree, and .git here is the file linking
  # it back to the main checkout's .git/worktrees/ -- deleting it breaks every
  # subsequent git command in this worktree, including the commit/push step.
  find "${GH_PAGES_DIR}" -mindepth 1 -maxdepth 1 ! -name 'v*' ! -name '.git' -exec rm -rf {} +
  cp -R "${ROOT_DEST}/." "${GH_PAGES_DIR}/"
  rm -rf "${ROOT_DEST}"
fi

# Pages would otherwise re-run Jekyll over this already-built static output.
# Still required: the accumulator is served with build_type: legacy, so Pages
# treats the branch as a Jekyll source unless told not to.
touch "${GH_PAGES_DIR}/.nojekyll"

# Regenerate the version manifest from what's actually on disk -- scanned, not
# appended-to, so a run building an older version (FR-006 republish) can never
# leave a newer version out of the list, and the end state is correct
# regardless of the order two close-together publishes actually run in.
versions_json="$(
  for d in "${GH_PAGES_DIR}"/v*/; do
    [[ -d "$d" ]] || continue
    basename "$d" | sed 's/^v//'
  done | sort -rV | jq -R . | jq -s '{versions: .}'
)"
printf '%s\n' "${versions_json}" > "${GH_PAGES_DIR}/versions.json"
