# Classification patterns, sourced by action.yml and by test-patterns.sh.
#
# In their own file so the test exercises the patterns CI actually uses rather
# than a copy of them that can drift. Nothing here executes; it only assigns.

# Paths that mean "this PR touches code": the full suite must run.
# See docs/adr/0322-ci-docs-only-fast-path.md for the entry-by-entry rationale.
CODE_PATTERN='(\.rs$|Cargo\.(toml|lock)$|^\.cargo/|^\.github/workflows/|^\.github/actions/|^scripts/|^crates/eval/scripts/)'

# Paths that match CODE_PATTERN but feed only the documentation site, and are
# therefore removed from the candidate list before it is applied.
#
# `^scripts/` and `^\.github/workflows/` are deliberately broad and stay that
# way: a script or workflow added tomorrow must default to code-touching. These
# exceptions are named individually, never by prefix, so nothing new is exempted
# by accident. Each is a file the Rust build and test jobs demonstrably never
# invoke — ci.yml runs stage-openssl-rpath.sh, assert-openssl-linkage.sh and
# crates/eval/scripts/test-scripts.sh, and nothing else under scripts/;
# docs-drift.yml and docs-publish.yml are separate workflows that ci.yml neither
# calls nor shares a job with.
#
# The fail-safe direction of ADR-0322 is unchanged: this only ever moves a named
# path from "code" to "docs", never the reverse.
DOCS_ONLY_PATTERN='^(\.github/workflows/docs-(drift|publish)\.yml|scripts/(docs-publish-build|docs-publish-latest-stable-version|generate-docs-llms-full)\.sh)$'
