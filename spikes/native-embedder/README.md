# native-embedder spike

**Status**: Spike (#107) — GO/NO-GO decision for embedding BGE-base-en-v1.5 natively in Rust.

This is a standalone Rust workspace for benchmarking two Rust ML libraries:

| Library | Approach |
|---------|----------|
| [`candle`](https://github.com/huggingface/candle) | Pure-Rust ML framework from HuggingFace; reads safetensors directly |
| [`ort`](https://github.com/pykeio/ort) | Rust bindings for ONNX Runtime; requires an ONNX model export |

The deliverable is `../../docs/spikes/native-embedder-2026-05.md`.

## This workspace is NOT part of the root project

`spikes/native-embedder/` is a **standalone Cargo workspace** and is intentionally absent
from the root `Cargo.toml` members list. Running `cargo test` or `cargo clippy` from the
repo root will not touch this code. All ML library dependencies stay isolated here.

## Quick start

```bash
cd spikes/native-embedder
# See scripts/setup.md for full setup instructions
```

### 1. Build

```bash
cargo build --release -p candle-bench
cargo build --release -p ort-bench   # requires internet — downloads libonnxruntime
```

### 2. Download models

See [scripts/setup.md](scripts/setup.md) for model download and ONNX export steps.

### 3. Run candle-bench (macOS / Linux)

```bash
/usr/bin/time -l ./target/release/candle-bench \
  --model-dir models/bge-base \
  --warmup 100 --iters 200 \
  --parity-json reference_embeddings.json \
  --output-json results/candle-macos-arm64.json
```

### 4. Run ort-bench (macOS / Linux)

```bash
# CPU execution provider (default, cross-platform)
/usr/bin/time -l ./target/release/ort-bench \
  --model-dir models/bge-base-onnx \
  --warmup 100 --iters 200 \
  --parity-json reference_embeddings.json \
  --output-json results/ort-macos-arm64.json

# CoreML execution provider (macOS only — see scripts/setup.md Step 8 for two-pass protocol)
COREML_VERBOSE=1 /usr/bin/time -l ./target/release/ort-bench \
  --model-dir models/bge-base-onnx \
  --warmup 100 --iters 200 \
  --execution-provider coreml \
  --parity-json reference_embeddings.json \
  --output-json results/ort-coreml-macos-arm64.json
```

`--execution-provider` accepts `cpu` (default) or `coreml`. The `coreml` value
routes inference through ort's CoreML execution provider on macOS Apple Silicon.
On non-macOS platforms, `coreml` will exit with a clear error. Run the two-pass
cold-start protocol from `scripts/setup.md` Step 8 before benchmarking CoreML EP
to avoid recording CoreML's first-launch compilation time as benchmark overhead.

### 5. Run on Linux x86_64 (via Docker)

```bash
chmod +x scripts/run-linux.sh
./scripts/run-linux.sh
```

## Corpora

| File | Source | Purpose |
|------|--------|---------|
| `common/src/corpus.rs` | `bench.py` (SHORT×4 + MEDIUM×3 + LONG×6, first 200) | 200-sentence latency benchmark |
| `common/src/corpus.rs` | `verify-embedding-parity.py` | 50-sentence cosine parity reference |
| `reference_embeddings.json` | `scripts/gen_reference.py` output | PyTorch ground-truth embeddings |

## Output format

Each harness writes a JSON file:

```json
{
  "library": "candle|ort",
  "platform": "macos/aarch64",
  "cold_start_ms": 1234.5,
  "bench": { "p50_ms": …, "p95_ms": …, "p99_ms": …, "min_ms": …, "max_ms": …,
             "mean_ms": …, "batch_throughput_per_sec": …, "n_iters": … },
  "parity": { "min_cosine": …, "max_cosine": …, "mean_cosine": …,
              "n_below_threshold": …, "threshold": 0.999, "passed": … }
}
```

Memory (RSS) is measured externally: `/usr/bin/time -l` on macOS, `/usr/bin/time -v` on Linux.

## Success criteria (from spec SC-001 through SC-006)

| ID | Metric | Threshold |
|----|--------|-----------|
| SC-001 | Cosine similarity vs PyTorch | ≥0.999 (all 50 sentences) |
| SC-002 | p95 latency, macOS Apple Silicon | ≤25 ms |
| SC-003 | p95 latency, Linux x86_64 | ≤50 ms |
| SC-004 | Resident memory at steady state | ≤500 MB |
| SC-005 | Cold start to first embed | ≤2 s |
| SC-006 | Binary size growth | ≤50 MB |

## Decision report

Results and verdict: `../../docs/spikes/native-embedder-2026-05.md`
