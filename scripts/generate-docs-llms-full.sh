#!/usr/bin/env bash
# generate-docs-llms-full.sh — Generate docs/llms-full.txt from canonical documentation pages.
#
# Usage: scripts/generate-docs-llms-full.sh
#   Run from the repo root or any directory — uses paths relative to this script.
#
# Output: docs/llms-full.txt — committed concatenated bundle checked by CI
#
# The version comes from Cargo.toml's [workspace.package]. It used to live in
# docs/_config.yml as well, with this script asserting the two matched (FR-010);
# the Jekyll site is gone and with it that second copy, so there is nothing left
# to disagree and nothing to check.
#
# Workflow:
#   1. Run this script after modifying any canonical doc page listed in ORDERED below,
#      or after bumping Cargo.toml's [workspace.package] version
#   2. Commit docs/llms-full.txt alongside your doc changes
#   3. CI (docs-drift.yml) verifies the committed file matches what this script produces

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DOCS_DIR="${REPO_ROOT}/docs"
OUT="${DOCS_DIR}/llms-full.txt"
CARGO_TOML="${REPO_ROOT}/Cargo.toml"

# ── Version, from the one place it is written down ──────────────────────────

docs_version=$(awk -F'"' '/^\[workspace.package\]/{found=1} found && /^version = /{print $2; exit}' "${CARGO_TOML}")
site_repository="verveguy/liminis-context-graph"

if [[ -z "${docs_version}" ]]; then
  echo "error: could not find [workspace.package] version in ${CARGO_TOML}" >&2
  exit 1
fi

TMPFILE="$(mktemp)"
OUT_TMP="$(mktemp)"
trap 'rm -f "${TMPFILE}" "${OUT_TMP}"' EXIT

# Strip YAML front matter (---...---) from a file into TMPFILE, then substitute the small set
# of Liquid variables used in page bodies (site.version/site.repository) with their resolved
# values — this script emits plain text, not Jekyll-rendered HTML, so unresolved `{{ ... }}`
# tags would otherwise leak into llms-full.txt verbatim. (The site resolves the same two
# variables in site/scripts/sync-docs.mjs, from the same source.)
#
# The generated <picture> beside each ```c4 fence is dropped: it points at a relative SVG
# path that means nothing in a flat text bundle. The fence itself stays — it is the diagram
# in textual form, which is exactly what this file is for. Front matter is only recognized when
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
    | sed -e '/<picture>.*\/diagrams\//d' \
    > "${TMPFILE}"
}

# Must match `url` + `baseurl` in docs/_config.yml. Not verveguy.github.io: the account
# carries a Pages custom domain, so those URLs 301-redirect to v3rv.com (GitHub reports
# html_url as http://v3rv.com/liminis-context-graph/ while this repo's cname is null).
SITE_URL="https://v3rv.com/liminis-context-graph"

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
  "release-process.md:${SITE_URL}/release-process"
  # The decision records are not part of the site any more — the site redirects
  # /adr/* to GitHub. The index still belongs in this bundle, since it is real
  # documentation of how the records are meant to be read; only its canonical
  # URL moves.
  "adr/index.md:https://github.com/verveguy/liminis-context-graph/blob/main/docs/adr/index.md"
)

> "$OUT_TMP"

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

  printf '# %s\n\nSource: %s\n\n' "$title" "$url" >> "$OUT_TMP"

  # Output body content, skipping leading blank lines and the first H1 heading
  # so the H1 appears exactly once (in the section header above).
  awk '
    BEGIN { skipping=1 }
    skipping && /^[[:space:]]*$/ { next }
    skipping && /^# /            { skipping=0; next }
    { skipping=0; print }
  ' "${TMPFILE}" >> "$OUT_TMP"

  printf '\n---\n\n' >> "$OUT_TMP"
done

# Only replace the committed file once every page has validated successfully — a mid-loop
# failure (e.g. a missing H1) must never leave docs/llms-full.txt truncated or partial.
mv "${OUT_TMP}" "$OUT"
