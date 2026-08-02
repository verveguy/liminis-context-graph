#!/usr/bin/env bash
# generate-docs-llms-full.sh — Generate docs/llms-full.txt from canonical documentation pages.
#
# Usage: scripts/generate-docs-llms-full.sh
#   Run from the repo root or any directory — uses paths relative to this script.
#
# Output: docs/llms-full.txt — committed concatenated bundle checked by CI
#
# Also verifies docs/_config.yml's `version:` matches Cargo.toml's
# [workspace.package] version (FR-010) — a mismatch fails the script, not just a
# content diff, since llms-full.txt wouldn't change on a pure version bump.
#
# Workflow:
#   1. Run this script after modifying any canonical doc page listed in ORDERED below,
#      or after bumping Cargo.toml's [workspace.package] version
#   2. Commit docs/llms-full.txt (and docs/_config.yml if the version changed) alongside
#      your doc changes
#   3. CI (docs-drift.yml) verifies the committed file matches what this script produces

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DOCS_DIR="${REPO_ROOT}/docs"
OUT="${DOCS_DIR}/llms-full.txt"
CONFIG="${DOCS_DIR}/_config.yml"
CARGO_TOML="${REPO_ROOT}/Cargo.toml"

# ── Version-sync check (FR-010) ─────────────────────────────────────────────

docs_version=$(awk -F'"' '/^version:/{print $2; exit}' "${CONFIG}")
cargo_version=$(awk -F'"' '/^\[workspace.package\]/{found=1} found && /^version = /{print $2; exit}' "${CARGO_TOML}")
site_repository=$(awk '/^repository:/{print $2; exit}' "${CONFIG}")

if [[ -z "${docs_version}" ]]; then
  echo "error: could not find a version: field in ${CONFIG}" >&2
  exit 1
fi
if [[ -z "${cargo_version}" ]]; then
  echo "error: could not find [workspace.package] version in ${CARGO_TOML}" >&2
  exit 1
fi
if [[ "${docs_version}" != "${cargo_version}" ]]; then
  echo "error: docs/_config.yml version (${docs_version}) does not match Cargo.toml's" >&2
  echo "[workspace.package] version (${cargo_version})." >&2
  echo "Update docs/_config.yml's version: field to \"${cargo_version}\" and re-run" >&2
  echo "scripts/generate-docs-llms-full.sh." >&2
  exit 1
fi

TMPFILE="$(mktemp)"
trap 'rm -f "${TMPFILE}"' EXIT

# Strip YAML front matter (---...---) from a file into TMPFILE, then substitute the small set
# of Liquid variables used in page bodies (site.version/site.repository) with their resolved
# values — this script emits plain text, not Jekyll-rendered HTML, so unresolved `{{ ... }}`
# tags would otherwise leak into llms-full.txt verbatim. Front matter is only recognized when
# the very first line is `---`, matching Jekyll's own detection rule — this way a body's own
# `---` horizontal rules (at any position, in a page with or without front matter) are never
# misidentified as a front-matter delimiter.
strip_front_matter_to_tmp() {
  awk '
    NR==1 && /^---$/ { infm=1; next }
    infm && /^---$/  { infm=0; next }
    infm             { next }
    { print }
  ' "$1" \
    | sed -e "s|{{ *site\.version *}}|${docs_version}|g" -e "s|{{ *site\.repository *}}|${site_repository}|g" \
    > "${TMPFILE}"
}

SITE_URL="https://verveguy.github.io/liminis-context-graph"

# Pages in fixed order — do not reorder; CI drift checks require bitwise-identical output.
# Format: "relative-path-from-docs:canonical-url"
ORDERED=(
  "index.md:${SITE_URL}/"
  "getting-started.md:${SITE_URL}/getting-started"
  "configuration.md:${SITE_URL}/configuration"
  "ipc-mcp-reference.md:${SITE_URL}/ipc-mcp-reference"
  "telemetry.md:${SITE_URL}/telemetry"
  "ontology.md:${SITE_URL}/ontology"
  "operations.md:${SITE_URL}/operations"
  "testing-and-evaluation.md:${SITE_URL}/testing-and-evaluation"
  "eval-full-corpus-runbook.md:${SITE_URL}/eval-full-corpus-runbook"
  "extraction-quality-evaluation.md:${SITE_URL}/extraction-quality-evaluation"
  "adr/index.md:${SITE_URL}/adr/index"
)

> "$OUT"

for entry in "${ORDERED[@]}"; do
  file="${DOCS_DIR}/${entry%%:*}"
  url="${entry#*:}"

  strip_front_matter_to_tmp "$file"

  # Extract the first H1 heading from the body (reads from file, no SIGPIPE risk).
  title=$(awk '/^# /{sub(/^# /, ""); print; exit}' "${TMPFILE}")
  if [[ -z "$title" ]]; then
    echo "error: no H1 heading found in ${file}" >&2
    exit 1
  fi

  printf '# %s\n\nSource: %s\n\n' "$title" "$url" >> "$OUT"

  # Output body content, skipping leading blank lines and the first H1 heading
  # so the H1 appears exactly once (in the section header above).
  awk '
    BEGIN { skipping=1 }
    skipping && /^[[:space:]]*$/ { next }
    skipping && /^# /            { skipping=0; next }
    { skipping=0; print }
  ' "${TMPFILE}" >> "$OUT"

  printf '\n---\n\n' >> "$OUT"
done
