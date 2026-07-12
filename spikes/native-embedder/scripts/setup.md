# Setup Guide: native-embedder spike

## Prerequisites

- Rust + Cargo (via `rustup`)
- ~3 GB free disk space (model weights + build artifacts)
- Internet access for crate downloads and model downloads

## Step 1: Build the harnesses

Build both benchmark binaries from the spike root directory:

```bash
cd spikes/native-embedder
cargo build --release -p candle-bench
cargo build --release -p ort-bench
```

First build: expect 5-10 minutes (candle downloads and compiles a large dep tree;
ort downloads `libonnxruntime` automatically).

## Step 2: Download model files for candle-bench

`candle-bench` reads safetensors directly. Download the HuggingFace checkpoint:

```bash
# Option A: huggingface-cli (pip install huggingface_hub)
huggingface-cli download BAAI/bge-base-en-v1.5 \
  --include "model.safetensors" "config.json" "tokenizer.json" "tokenizer_config.json" \
  --local-dir models/bge-base

# Option B: manual download
mkdir -p models/bge-base
wget -P models/bge-base \
  https://huggingface.co/BAAI/bge-base-en-v1.5/resolve/main/model.safetensors \
  https://huggingface.co/BAAI/bge-base-en-v1.5/resolve/main/config.json \
  https://huggingface.co/BAAI/bge-base-en-v1.5/resolve/main/tokenizer.json

# Verify
ls -lh models/bge-base/
# model.safetensors should be ~438 MB
```

## Step 3: Export ONNX model for ort-bench

`ort-bench` requires an ONNX model. BAAI/bge-base-en-v1.5 does not ship one;
export it using the `optimum` library:

```bash
pip install optimum[exporters]
optimum-cli export onnx \
  --task feature-extraction \
  --model BAAI/bge-base-en-v1.5 \
  models/bge-base-onnx

# Verify
ls -lh models/bge-base-onnx/
# model.onnx should be ~438 MB
# tokenizer.json should be present
```

**If optimum export fails**: Document the exact error in the spike report as a
deployment complexity finding for `ort`. The spike may still produce a partial
ort assessment based on build behavior.

## Step 4: Generate reference embeddings (if not already committed)

`reference_embeddings.json` is pre-committed. If you need to regenerate it:

```bash
uv run scripts/gen_reference.py
# or: pip install sentence-transformers torch numpy && python scripts/gen_reference.py
```

## Step 5: Run candle-bench on macOS

```bash
# Release build recommended for fair latency numbers
cargo build --release -p candle-bench

# Run benchmark (adjust warmup/iters as needed)
/usr/bin/time -l ./target/release/candle-bench \
  --model-dir models/bge-base \
  --warmup 100 \
  --iters 200 \
  --parity-json reference_embeddings.json \
  --output-json results/candle-macos-arm64.json \
  2>&1 | tee results/candle-macos-arm64.log

# The peak RSS (memory) is reported by /usr/bin/time -l
# Look for "maximum resident set size" in the output
```

## Step 6: Run ort-bench on macOS

```bash
cargo build --release -p ort-bench

/usr/bin/time -l ./target/release/ort-bench \
  --model-dir models/bge-base-onnx \
  --warmup 100 \
  --iters 200 \
  --parity-json reference_embeddings.json \
  --output-json results/ort-macos-arm64.json \
  2>&1 | tee results/ort-macos-arm64.log
```

## Step 7: Run on Linux x86_64

See `scripts/run-linux.sh`. Docker is required.

```bash
chmod +x scripts/run-linux.sh
./scripts/run-linux.sh
```

## Step 8: Run ort-bench with CoreML execution provider (macOS only)

The `--execution-provider coreml` flag routes inference through ort's CoreML EP.
This requires a **two-pass protocol** because CoreML compiles the ONNX subgraph
on first launch (30–120 s for a 110M-parameter model); the SC-005 cold-start
threshold (≤2 s) applies to the *cached* run only.

### Clear the CoreML cache (for a clean first-launch time)

```bash
rm -rf ~/Library/Caches/com.microsoft.onnxruntime*
```

### Pass 1: prime the CoreML cache and record compilation time

```bash
COREML_VERBOSE=1 /usr/bin/time -l ./target/release/ort-bench \
  --model-dir models/bge-base-onnx \
  --warmup 5 --iters 5 \
  --execution-provider coreml \
  2>&1 | tee results/ort-coreml-primer.log
```

Pass 1 may take 30–120 s on first launch while CoreML compiles the model.
Record the `cold_start_ms` value — it measures time from process start to first
embedding, which on first launch includes CoreML model load and compilation time.
The `COREML_VERBOSE=1` environment variable may emit per-op dispatch info to
stderr; capture it in the log file. If no dispatch info appears, observe ANE
activity separately via `sudo powermetrics --samplers gpu_power`.

### Pass 2: run the actual benchmark (warm CoreML cache)

```bash
COREML_VERBOSE=1 /usr/bin/time -l ./target/release/ort-bench \
  --model-dir models/bge-base-onnx \
  --warmup 100 --iters 200 \
  --execution-provider coreml \
  --parity-json reference_embeddings.json \
  --output-json results/ort-coreml-macos-arm64.json \
  2>&1 | tee results/ort-coreml-macos-arm64.log
```

Pass 2 `cold_start_ms` reflects the cached CoreML load time (≤2 s expected
if the CoreML cache is warm from Pass 1).

**Outputs**:
- `results/ort-coreml-primer.log` — Pass 1 log (includes compilation time)
- `results/ort-coreml-macos-arm64.json` — Pass 2 benchmark JSON
- `results/ort-coreml-macos-arm64.log` — Pass 2 full log

**Note**: `coreml` is macOS-only. Running `--execution-provider coreml` on
Linux will exit with: `Error: CoreML EP is only available on macOS`.

## Measuring binary size delta

The spike crate is not linked into `liminis-context-graph`. To estimate the binary
growth if it were, compare the compiled binary sizes directly:

```bash
# Build both release binaries
cargo build --release -p candle-bench
cargo build --release -p ort-bench

# Compare sizes
ls -lh target/release/candle-bench target/release/ort-bench
```

The `common` crate is a library with no binary target; it has no entry in
`target/release/`. The bench binary sizes are the best available proxy for
the ML library overhead that would accrue to `liminis-context-graph` if the embedder
were integrated. Note that `libonnxruntime` (~60 MB) is downloaded separately
and not included in the `ort-bench` binary size reported above.

## Interpreting results

Each benchmark writes a JSON file with this schema:

```json
{
  "library": "candle|ort",
  "platform": "macos/aarch64",
  "execution_provider": "cpu|coreml",  // ort-bench only
  "cold_start_ms": 1234,
  "bench": {
    "p50_ms": 15.2,
    "p95_ms": 22.1,
    "p99_ms": 28.4,
    "min_ms": 12.0,
    "max_ms": 45.0,
    "mean_ms": 16.1,
    "batch_throughput_per_sec": 58.0,
    "n_iters": 200
  },
  "parity": {
    "min_cosine": 0.9991,
    "max_cosine": 0.9999,
    "mean_cosine": 0.9996,
    "n_below_threshold": 0,
    "threshold": 0.999,
    "passed": true
  }
}
```

Compare `p95_ms` against SC-002 (≤25 ms macOS) and SC-003 (≤50 ms Linux x86_64).
Check `parity.passed` against SC-001 (≥0.999 cosine).
Check `/usr/bin/time -l` peak RSS against SC-004 (≤500 MB).
