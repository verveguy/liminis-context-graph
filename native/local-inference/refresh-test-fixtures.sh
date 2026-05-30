#!/usr/bin/env bash
# Regenerate all integration test fixtures from the fixture generator scripts.
#
# Steps 1–2 run the stub generator scripts in Tests/…/Fixtures/ (documented
# stubs — see Fixtures/README.md for the rationale).  Step 3 runs the
# production prepare-tokenizer.py.  convert-embedding-model.py is not run
# (its sentinel is updated only, not its output — see ADR-062).
#
# Run this command after changing any of the generator scripts:
#   prepare-tokenizer.py, convert-embedding-model.py,
#   Fixtures/generate-stub-model.py, Fixtures/generate-bad-stub-models.py
#
# The script writes updated sentinel hashes so check-fixture-freshness.sh
# (and CI) will confirm the fixtures are fresh.
#
# Usage:
#   bash refresh-test-fixtures.sh
#
# Requirements: uv (https://github.com/astral-sh/uv) must be on PATH.
#   The tokenizer step requires network access to HuggingFace (revision is pinned).
#   The stub mlpackage steps are offline (coremltools only).

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
TOKENIZER_OUT="$FIXTURES/tokenizer-cache/models/BAAI/bge-base-en-v1.5"

echo "=== refresh-test-fixtures: regenerating all sidecar integration test fixtures ==="
echo ""

# 1. Positive-path stub models (fp32 + fp16) via generate-stub-model.py
# Version pins are read from requirements.txt (single source of truth).
echo "--- Step 1/3: generating stub-bge-base.mlpackage (fp32) ---"
uv run --no-project --with-requirements "$FIXTURES/requirements.txt" \
    "$FIXTURES/generate-stub-model.py" --precision fp32

echo ""
echo "--- Step 1b/3: generating stub-bge-base-fp16.mlpackage (fp16) ---"
uv run --no-project --with-requirements "$FIXTURES/requirements.txt" \
    "$FIXTURES/generate-stub-model.py" --precision fp16

echo ""

# 2. Negative-path bad-dtype/shape/name stubs via generate-bad-stub-models.py
echo "--- Step 2/3: generating stub-bge-base-bad-*.mlpackage (negative-path fixtures) ---"
uv run --no-project --with-requirements "$FIXTURES/requirements.txt" \
    "$FIXTURES/generate-bad-stub-models.py"

echo ""

# 3. Tokenizer cache via prepare-tokenizer.py (requires network; revision is pinned)
echo "--- Step 3/3: staging tokenizer cache (pinned revision from PINNED_BGE_REVISION) ---"
uv run --no-project "$HERE/prepare-tokenizer.py" --output "$TOKENIZER_OUT"

echo ""

# Write sentinel hashes so check-fixture-freshness.sh sees the fixtures as fresh.
echo "--- Writing sentinel hashes ---"
sha256 "$FIXTURES/generate-stub-model.py"    | awk '{print $1}' > "$FIXTURES/generate-stub-model.script-hash"
sha256 "$FIXTURES/generate-bad-stub-models.py" | awk '{print $1}' > "$FIXTURES/generate-bad-stub-models.script-hash"
sha256 "$HERE/convert-embedding-model.py"    | awk '{print $1}' > "$FIXTURES/convert-embedding-model.script-hash"
sha256 "$HERE/prepare-tokenizer.py"          | awk '{print $1}' > "$FIXTURES/tokenizer-cache/.script-hash"

echo "  generate-stub-model.script-hash    → $(cat "$FIXTURES/generate-stub-model.script-hash")"
echo "  generate-bad-stub-models.script-hash → $(cat "$FIXTURES/generate-bad-stub-models.script-hash")"
echo "  convert-embedding-model.script-hash → $(cat "$FIXTURES/convert-embedding-model.script-hash")"
echo "  tokenizer-cache/.script-hash        → $(cat "$FIXTURES/tokenizer-cache/.script-hash")"
echo ""
echo "=== Done. Review the diff and commit the updated fixtures. ==="
