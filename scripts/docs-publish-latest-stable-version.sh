#!/usr/bin/env bash
# docs-publish-latest-stable-version.sh — Print the latest published stable
# (non-prerelease) release version for this repo, or nothing if there is none.
#
# Usage: scripts/docs-publish-latest-stable-version.sh
#   Requires `gh` authenticated against this repo (or GH_TOKEN set).
#
# Output: the version string without its leading "v" (e.g. "0.13.3") on stdout,
#   or nothing (empty stdout, exit 0) if no stable version-scheme release exists.
#
# "Latest stable" is always recomputed fresh from the GitHub Releases API here —
# never cached or trusted from a triggering webhook event — so that two
# publish runs racing on close-together releases converge on the same answer
# regardless of which one happens to run first (see docs/adr/0477-*.md).
#
# A release only counts if:
#   - its tag matches the project's version-tag scheme (^v[0-9]+\.[0-9]+\.[0-9]+(-...)?$),
#     which excludes non-version releases like `eval-artifacts-2026-07` (FR-009)
#   - it is not marked a prerelease (FR-002)
#   - it is not a draft (drafts aren't published/visible to readers, so one
#     outranking the real latest stable by version number must not suppress
#     that promotion)

set -euo pipefail

REPO="${DOCS_PUBLISH_REPO:-verveguy/liminis-context-graph}"

TAG_PATTERN='^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$'

gh api "repos/${REPO}/releases" --paginate \
  --jq '.[] | select(.prerelease == false and .draft == false) | .tag_name' \
  | grep -E "${TAG_PATTERN}" \
  | sed 's/^v//' \
  | sort -t. -k1,1n -k2,2n -k3,3n \
  | tail -n1
