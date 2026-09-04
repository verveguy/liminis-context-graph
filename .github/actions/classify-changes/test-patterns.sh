#!/usr/bin/env bash
# Tests for the classify-changes patterns.
#
# This logic has been wrong before in a way nothing caught: a `set -e`
# interaction meant every genuinely docs-only PR failed closed and ran the full
# suite anyway (see the note in action.yml). It is also the thing standing
# between a code change and its test suite, so a mistake in the other direction
# is worse. Both directions are asserted here.
#
# Run: .github/actions/classify-changes/test-patterns.sh

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=patterns.sh
source ./patterns.sh

failures=0

# Mirrors the classification in action.yml: drop the docs-only exceptions, then
# apply the deny-list to what remains.
classify() {
  local files="$1" candidates
  candidates=$(echo "$files" | grep -Ev "$DOCS_ONLY_PATTERN") || true
  if echo "$candidates" | grep -Eq "$CODE_PATTERN"; then echo code; else echo docs; fi
}

expect() {
  local want="$1" files="$2" why="$3" got
  got=$(classify "$files")
  if [ "$got" = "$want" ]; then
    printf '  ok    %-6s %s\n' "$want" "$why"
  else
    printf '  FAIL  wanted %s, got %s: %s\n' "$want" "$got" "$why"
    failures=$((failures + 1))
  fi
}

echo "docs-only changes must skip the suite:"
expect docs 'docs/configuration.md' 'a documentation page'
expect docs 'site/astro.config.mjs' 'the documentation site'
expect docs 'site/src/content/docs/index.mdx
docs/index.md' 'site and docs together'
expect docs 'scripts/generate-docs-llms-full.sh' 'the llms bundle generator'
expect docs 'scripts/docs-publish-build.sh' 'the docs publish build'
expect docs 'scripts/docs-publish-latest-stable-version.sh' 'the docs publish helper'
expect docs '.github/workflows/docs-drift.yml' 'the docs drift workflow'
expect docs '.github/workflows/docs-publish.yml' 'the docs publish workflow'
expect docs 'README.md
CHANGELOG.md
docs/adr/0322-ci-docs-only-fast-path.md' 'prose across the repo'

echo
echo "code changes must run the suite:"
expect code 'crates/core/src/db.rs' 'Rust source'
expect code 'Cargo.toml' 'the workspace manifest'
expect code 'crates/service/Cargo.toml' 'a crate manifest'
expect code '.cargo/config.toml' 'the cargo config'
expect code '.github/workflows/ci.yml' 'the CI workflow itself'
expect code '.github/actions/classify-changes/patterns.sh' 'these patterns'
expect code '.github/actions/classify-changes/action.yml' 'the classifier itself'
expect code 'scripts/stage-openssl-rpath.sh' 'a script the build runs'
expect code 'scripts/assert-openssl-linkage.sh' 'the OpenSSL linkage guard'
expect code 'crates/eval/scripts/test-scripts.sh' 'a script the test job runs'
expect code 'docs/configuration.md
crates/core/src/db.rs' 'docs alongside code'
expect code 'scripts/generate-docs-llms-full.sh
crates/core/src/db.rs' 'a docs script alongside code'

echo
echo "new paths default to code, never to docs:"
expect code 'scripts/some-new-build-step.sh' 'an unrecognised script'
expect code '.github/workflows/some-new-workflow.yml' 'an unrecognised workflow'

echo
if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)"
  exit 1
fi
echo "all pattern classifications correct"
