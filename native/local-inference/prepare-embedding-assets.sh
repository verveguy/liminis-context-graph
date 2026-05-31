#!/usr/bin/env bash
# One-shot setup wrapper. Produces the two artifacts the Swift sidecar needs:
#   1. native/local-inference/bge-base-en-v1.5.mlpackage  (via convert-embedding-model.py)
#   2. resources/models/tokenizer/models/BAAI/bge-base-en-v1.5/  (via prepare-tokenizer.py)
#
# Re-running is safe — both helpers overwrite their outputs.
#
# After this script succeeds, build the sidecar with:
#   swift build -c release
# and run it with:
#   .build/release/LocalInference

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v uv >/dev/null 2>&1; then
  echo "ERROR: uv is required. Install with: brew install uv" >&2
  exit 1
fi

echo "==> Converting BGE model → bge-base-en-v1.5.mlpackage (this takes 2–5 minutes)"
uv run convert-embedding-model.py

echo
echo "==> Staging tokenizer files for offline swift-transformers loading"
uv run prepare-tokenizer.py

echo
echo "Done. Build the sidecar with: swift build -c release"
echo "Then run it with:           .build/release/LocalInference"
