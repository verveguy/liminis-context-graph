#!/usr/bin/env bash
# Run both embedding benchmarks on linux/amd64 via Docker.
#
# Prerequisites:
#   - Docker installed and running
#   - models/bge-base/ exists (safetensors for candle)
#   - models/bge-base-onnx/ exists (ONNX for ort)
#   - reference_embeddings.json exists
#
# Results land in results/
set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_DIR="$SPIKE_DIR/results"
mkdir -p "$RESULTS_DIR"

# Rust stable image with linux/amd64 (runs via Rosetta/QEMU on Apple Silicon)
RUST_IMAGE="rust:1.85-slim"

echo "=== Building and running candle-bench on linux/amd64 ==="
docker run --rm \
  --platform linux/amd64 \
  -v "$SPIKE_DIR":/spike:ro \
  -v "$RESULTS_DIR":/results \
  -w /spike \
  "$RUST_IMAGE" \
  bash -c "
    set -e
    apt-get update -q && apt-get install -y -q cmake pkg-config libssl-dev 2>/dev/null
    cargo build --release -p candle-bench
    /usr/bin/time -v ./target/release/candle-bench \
      --model-dir models/bge-base \
      --warmup 10 \
      --iters 100 \
      --parity-json reference_embeddings.json \
      --output-json /results/candle-linux-amd64.json \
      2>&1 | tee /results/candle-linux-amd64.log
  "

echo ""
echo "=== Building and running ort-bench on linux/amd64 ==="
docker run --rm \
  --platform linux/amd64 \
  -v "$SPIKE_DIR":/spike:ro \
  -v "$RESULTS_DIR":/results \
  -w /spike \
  "$RUST_IMAGE" \
  bash -c "
    set -e
    apt-get update -q && apt-get install -y -q cmake pkg-config libssl-dev 2>/dev/null
    cargo build --release -p ort-bench
    /usr/bin/time -v ./target/release/ort-bench \
      --model-dir models/bge-base-onnx \
      --warmup 10 \
      --iters 100 \
      --parity-json reference_embeddings.json \
      --output-json /results/ort-linux-amd64.json \
      2>&1 | tee /results/ort-linux-amd64.log
  "

echo ""
echo "Results written to $RESULTS_DIR/"
echo "  candle-linux-amd64.json + .log"
echo "  ort-linux-amd64.json    + .log"
echo ""
echo "Note: Docker on macOS runs via a Linux VM. Latency numbers from"
echo "Docker on Apple Silicon are indicative, not production-representative."
echo "For authoritative Linux numbers, run on a native Linux x86_64 host."
