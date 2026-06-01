#!/usr/bin/env bash
# Verify that the Swift integration test fixtures match the current versions of
# the scripts that generate them.  Exits non-zero with a clear message when any
# fixture is stale so a developer (or CI) is reminded to regenerate.
#
# Checks four sentinels:
#   Fixtures/tokenizer-cache/.script-hash        — tracks prepare-tokenizer.py
#   Fixtures/generate-stub-model.script-hash     — tracks generate-stub-model.py
#   Fixtures/generate-bad-stub-models.script-hash — tracks generate-bad-stub-models.py
#   Fixtures/convert-embedding-model.script-hash  — tracks convert-embedding-model.py
#
# Usage: bash check-fixture-freshness.sh
# Run from anywhere — paths are resolved relative to this script.

set -euo pipefail

# sha256: portable SHA-256 digest — sha256sum on Linux, shasum on macOS
sha256() {
    if command -v sha256sum &>/dev/null; then
        sha256sum "$@"
    else
        shasum -a 256 "$@"
    fi
}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="$HERE/Tests/LocalInferenceTests/Fixtures"
FAILED=0

check_sentinel() {
    local script="$1"
    local sentinel="$2"
    local label="$3"

    if [ ! -f "$sentinel" ]; then
        echo "ERROR: fixture sentinel not found: $sentinel" >&2
        echo "  Regenerate: bash $HERE/refresh-test-fixtures.sh" >&2
        FAILED=1
        return
    fi

    local current_hash sentinel_hash
    current_hash=$(sha256 "$script" | awk '{print $1}')
    sentinel_hash=$(tr -d '[:space:]' < "$sentinel")

    if [ "$current_hash" != "$sentinel_hash" ]; then
        echo "ERROR: $label fixture is stale." >&2
        echo "  $(basename "$script") has changed since the fixture was last generated." >&2
        echo "  Regenerate: bash $HERE/refresh-test-fixtures.sh" >&2
        FAILED=1
    else
        echo "OK: $label (hash: ${current_hash:0:8}...)"
    fi
}

# Lightweight existence checks — verify fixtures are present before checking sentinels.
# This catches accidental git rm or corruption independently of script-hash drift.
check_exists() {
    local path="$1"
    local label="$2"
    if [ ! -e "$path" ]; then
        echo "ERROR: fixture missing: $path ($label)" >&2
        echo "  Regenerate: bash $HERE/refresh-test-fixtures.sh" >&2
        FAILED=1
    fi
}

# Required tokenizer files (mirrors TOKENIZER_FILES in prepare-tokenizer.py)
TOKENIZER_DIR="$FIXTURES/tokenizer-cache/models/BAAI/bge-base-en-v1.5"
for f in tokenizer.json tokenizer_config.json vocab.txt special_tokens_map.json config.json; do
    check_exists "$TOKENIZER_DIR/$f" "tokenizer-cache/$f"
done

# Each mlpackage must have a Manifest.json (minimal structural integrity check)
for pkg in stub-bge-base.mlpackage stub-bge-base-fp16.mlpackage \
           stub-bge-base-bad-dtype.mlpackage stub-bge-base-bad-shape.mlpackage \
           stub-bge-base-bad-output-name.mlpackage; do
    check_exists "$FIXTURES/$pkg/Manifest.json" "$pkg/Manifest.json"
done

if [ "$FAILED" -ne 0 ]; then
    echo "" >&2
    echo "One or more fixtures are missing. Run:" >&2
    echo "  bash $HERE/refresh-test-fixtures.sh" >&2
    exit 1
fi

echo "OK: all fixture files present"
echo ""

check_sentinel \
    "$HERE/prepare-tokenizer.py" \
    "$FIXTURES/tokenizer-cache/.script-hash" \
    "tokenizer-cache"

check_sentinel \
    "$FIXTURES/generate-stub-model.py" \
    "$FIXTURES/generate-stub-model.script-hash" \
    "stub mlpackage (fp32/fp16)"

check_sentinel \
    "$FIXTURES/generate-bad-stub-models.py" \
    "$FIXTURES/generate-bad-stub-models.script-hash" \
    "bad-stub mlpackages (negative-path)"

check_sentinel \
    "$HERE/convert-embedding-model.py" \
    "$FIXTURES/convert-embedding-model.script-hash" \
    "convert-embedding-model (production script mirror)"

# Verify that PINNED_BGE_REVISION is consistent across both production scripts.
# The two constants must match so tokenizer and model always pin the same upstream snapshot.
tokenizer_rev=$(grep 'PINNED_BGE_REVISION\s*=' "$HERE/prepare-tokenizer.py" | head -1 | sed 's/.*= *"\(.*\)".*/\1/')
converter_rev=$(grep 'PINNED_BGE_REVISION\s*=' "$HERE/convert-embedding-model.py" | head -1 | sed 's/.*= *"\(.*\)".*/\1/')
if [ "$tokenizer_rev" != "$converter_rev" ]; then
    echo "ERROR: PINNED_BGE_REVISION mismatch between production scripts." >&2
    echo "  prepare-tokenizer.py:      $tokenizer_rev" >&2
    echo "  convert-embedding-model.py: $converter_rev" >&2
    echo "  Update both to the same revision and re-run refresh-test-fixtures.sh." >&2
    FAILED=1
else
    echo "OK: PINNED_BGE_REVISION consistent (${tokenizer_rev:0:8}...)"
fi

if [ "$FAILED" -ne 0 ]; then
    echo "" >&2
    echo "One or more fixtures are stale. Run:" >&2
    echo "  bash $HERE/refresh-test-fixtures.sh" >&2
    exit 1
fi

echo ""
echo "All fixture sentinels are fresh."
