#!/usr/bin/env bash
# Pre-build gate for `pnpm package`. Validates that the embedding assets
# produced by `pnpm prepare-embedding-assets` are present AND non-empty,
# so a half-populated directory cannot slip through into a broken .app bundle.

set -euo pipefail

# Resolve repo paths relative to this script (native/local-inference/<this>),
# so the gate works regardless of caller cwd.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ROOT="$(cd "$HERE/../.." && pwd)"

FIX_HINT="Run: pnpm --filter liminis-app prepare-embedding-assets"

fail() {
  echo "ERROR: $1" >&2
  echo "  $FIX_HINT" >&2
  exit 1
}

MLPACKAGE="$APP_ROOT/native/local-inference/bge-base-en-v1.5.mlpackage"
[ -d "$MLPACKAGE" ] || fail "missing: $MLPACKAGE"
WEIGHTS="$MLPACKAGE/Data/com.apple.CoreML/weights/weight.bin"
[ -s "$WEIGHTS" ] || fail "empty or missing model weights: $WEIGHTS"

TOK_DIR="$APP_ROOT/resources/models/tokenizer/models/BAAI/bge-base-en-v1.5"
[ -d "$TOK_DIR" ] || fail "missing tokenizer directory: $TOK_DIR"
for f in tokenizer.json tokenizer_config.json vocab.txt special_tokens_map.json config.json; do
  [ -s "$TOK_DIR/$f" ] || fail "empty or missing tokenizer file: $TOK_DIR/$f"
done

# Verify that offline metadata markers exist for every staged tokenizer file.
# The swift-transformers HubApi offline loader requires a .metadata file under
# .cache/huggingface/download/ for each top-level file it walks; a missing
# marker causes offlineModeError("Metadata not available for <filename>").
METADATA_DIR="$TOK_DIR/.cache/huggingface/download"
[ -d "$METADATA_DIR" ] || fail "missing tokenizer metadata directory: $METADATA_DIR (re-run pnpm prepare-embedding-assets)"
for f in tokenizer.json tokenizer_config.json vocab.txt special_tokens_map.json config.json; do
  [ -s "$METADATA_DIR/${f}.metadata" ] || fail "missing or empty .metadata marker for $f in $METADATA_DIR (re-run pnpm prepare-embedding-assets)"
done
