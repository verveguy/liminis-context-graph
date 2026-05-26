# Native Rust Embedder Spike: candle vs ort — Decision Report

> **DRAFT — measurements pending**
>
> Measurement tables contain TBD placeholders. Final numbers will be populated
> after macOS Apple Silicon and Linux x86_64 benchmark runs complete.

**Date**: 2026-05-26
**Branch**: `fabrik/issue-107`
**Issue**: [#107](https://github.com/verveguy/liminis-graph/issues/107)
**Spike harness**: `spikes/native-embedder/`
**Verdict**: TBD

---

## Background

The liminis-graph OSS bundling question requires a cross-platform embedder. The
Swift CoreML sidecar (shipped 2026-05-25, liminis#794, #809, #810, #811,
liminis-graph#81) is macOS-only. For a "drop-in to any project" OSS distribution
that works on Linux servers, we need either a native Rust embedder or a different
bundling strategy.

This spike evaluates two Rust ML libraries against the BGE-base-en-v1.5 model:

| Library | Version | Approach |
|---------|---------|----------|
| [`candle`](https://github.com/huggingface/candle) | 0.8.4 | Pure Rust; reads safetensors directly; no external runtime |
| [`ort`](https://github.com/pykeio/ort) | 2.0.0-rc.12 | ONNX Runtime bindings; downloads libonnxruntime at build time |

**Model**: BAAI/bge-base-en-v1.5 (109M parameters, 768-dim embeddings, 438 MB on disk)

**This spike does NOT modify production code.** `liminis-graph-core/src/embedder.rs`
and the existing `OaiEmbedder` path are unchanged. See ADR-0044 and ADR-0048.

**Principle V re-evaluation**: ADR-0044 established "no ML runtime in the Rust crate"
scoped to the Apple-Silicon/ANE out-of-process strategy. On Linux servers there is no
ANE — out-of-process embedding yields pure IPC cost with no hardware gain. This spike
is the evidence to evaluate whether Principle V should be scoped to macOS-production
use only, and relaxed for the cross-platform OSS distribution.

---

## Methodology

### Corpus

- **200-sentence latency benchmark**: SHORT×4 + MEDIUM×3 + LONG×6 sentences, first 200
  (identical to `liminis-app/native/local-inference/benchmark/bench.py`)
- **50-sentence parity reference**: `PARITY_SENTENCES` from `verify-embedding-parity.py`
  — PyTorch ground truth in `spikes/native-embedder/reference_embeddings.json`

### Measurements

| Measurement | Method |
|-------------|--------|
| Single-input latency p50/p95/p99/min/max | Rust `Instant::now()`, 200 timed iters after 100 warmup |
| Batch throughput | 200 sentences × 3 trials, best wall time |
| Cold start | Process launch → first successful embed call |
| Memory (RSS) | `/usr/bin/time -l` (macOS) / `/usr/bin/time -v` (Linux) |
| Cosine parity | cosine_similarity(Rust embed, PyTorch embed) for all 50 sentences |
| Binary size | `ls -lh target/release/<binary>` after `--release` build |
| Build time | `time cargo build --release -p <crate>` clean build |

### Platforms

| Platform | Machine | Notes |
|----------|---------|-------|
| macOS arm64 | Apple Silicon (developer machine) | Native run |
| Linux x86_64 | Docker on macOS (linux/amd64) | Via Rosetta/QEMU; latency indicative only |

### Success criteria

| ID | Metric | Threshold |
|----|--------|-----------|
| SC-001 | Cosine similarity vs PyTorch BGE-base | ≥0.999 (all 50 sentences) |
| SC-002 | p95 latency, macOS arm64 | ≤25 ms |
| SC-003 | p95 latency, Linux x86_64 | ≤50 ms |
| SC-004 | Resident memory (steady state after 100 warmup reqs) | ≤500 MB |
| SC-005 | Cold start to first embed | ≤2 s |
| SC-006 | Binary size (proxy for `liminis-graph` growth) | ≤50 MB |

---

## Results

### 1. Cosine parity vs PyTorch BGE-base (FR-004, SC-001)

| Library | min cosine | max cosine | mean cosine | n below 0.999 | SC-001 |
|---------|------------|------------|-------------|---------------|--------|
| candle | TBD | TBD | TBD | TBD | TBD |
| ort | TBD | TBD | TBD | TBD | TBD |

### 2. Single-input latency — macOS Apple Silicon (FR-001, FR-003, SC-002)

| Metric | candle (CPU) | candle (Metal) | ort |
|--------|--------------|----------------|-----|
| p50 ms | TBD | TBD | TBD |
| p95 ms | TBD | TBD | TBD |
| p99 ms | TBD | TBD | TBD |
| min ms | TBD | TBD | TBD |
| max ms | TBD | TBD | TBD |
| mean ms | TBD | TBD | TBD |
| SC-002 (≤25 ms p95) | TBD | TBD | TBD |

### 3. Batch throughput — macOS Apple Silicon (FR-003)

| Library | 200-sentence batch (best of 3) | Throughput (sent/s) |
|---------|-------------------------------|---------------------|
| candle CPU | TBD ms | TBD |
| candle Metal | TBD ms | TBD |
| ort | TBD ms | TBD |

### 4. Single-input latency — Linux x86_64 via Docker (FR-001, FR-003, SC-003)

> **Note**: Docker on macOS adds virtualization overhead. Numbers are indicative.
> Native Linux measurements preferred; see `scripts/setup.md`.

| Metric | candle | ort |
|--------|--------|-----|
| p50 ms | TBD | TBD |
| p95 ms | TBD | TBD |
| p99 ms | TBD | TBD |
| min ms | TBD | TBD |
| max ms | TBD | TBD |
| SC-003 (≤50 ms p95) | TBD | TBD |

### 5. Memory — resident set size at steady state (FR-005, SC-004)

| Platform | candle | ort |
|----------|--------|-----|
| macOS arm64 | TBD MB | TBD MB |
| Linux x86_64 | TBD MB | TBD MB |
| SC-004 (≤500 MB) | TBD | TBD |

### 6. Cold start — process launch to first embed (FR-006, SC-005)

| Platform | candle | ort |
|----------|--------|-----|
| macOS arm64 | TBD s | TBD s |
| Linux x86_64 | TBD s | TBD s |
| SC-005 (≤2 s) | TBD | TBD |

### 7. Build impact (FR-007, SC-006)

| Metric | candle-bench | ort-bench |
|--------|-------------|-----------|
| Binary size (release) | TBD MB | TBD MB |
| Clean build time | TBD min | TBD min |
| Extra Cargo deps | TBD | TBD |
| SC-006 (≤50 MB binary) | TBD | TBD |

### 8. Deployment story (FR-008)

#### candle

| Aspect | Assessment |
|--------|-----------|
| External runtime | None — pure Rust |
| Model format | safetensors (available on HuggingFace) |
| Model download | HuggingFace Hub (~438 MB on first launch) |
| Licensing | Apache 2.0 |
| Metal GPU (macOS) | TBD: did it build cleanly? |
| Build complexity | Moderate: large Rust dep tree, 5–10 min first build |

#### ort

| Aspect | Assessment |
|--------|-----------|
| External runtime | `libonnxruntime` — downloaded at build time automatically |
| Model format | ONNX (requires `optimum-cli export onnx` from HuggingFace checkpoint) |
| Model download | ONNX export step required (~438 MB) |
| Licensing | MIT; libonnxruntime under Microsoft license (OSS-permissive) |
| CUDA GPU (Linux) | Available via ort ExecutionProvider; not measured in this spike |
| Build complexity | High: requires Python + optimum for model prep; internet during `cargo build` |

---

## Verdict per Library

### candle — TBD (GO / NO-GO / GO-with-caveats)

**Verdict**: TBD

**Reasons**:
- SC-001 (parity): TBD
- SC-002 (macOS p95): TBD
- SC-003 (Linux p95): TBD
- SC-004 (memory): _Expected miss_: BGE-base fp32 weights are 438 MB on disk; RSS
  typically lands 500–700 MB including inference buffers. This is a likely NO-GO on
  memory grounds unless fp16 weights are used (out of scope for this spike).
- SC-005 (cold start): TBD
- SC-006 (binary size): TBD

**Caveats if GO-with-caveats**:
- TBD

---

### ort — TBD (GO / NO-GO / GO-with-caveats)

**Verdict**: TBD

**Reasons**:
- SC-001 (parity): TBD
- SC-002 (macOS p95): TBD — ort uses optimized native kernels; expect lower latency
  than candle CPU but higher than candle Metal.
- SC-003 (Linux p95): TBD — CPU-only ONNX Runtime is well-optimized for x86_64.
- SC-004 (memory): TBD — ONNX Runtime may load fp32 or fp16 depending on the
  exported model precision.
- SC-005 (cold start): TBD — ONNX session initialization is typically fast (<1 s).
- SC-006 (binary size): TBD — ort includes or links libonnxruntime (~50-80 MB).

**Caveats if GO-with-caveats**:
- ONNX model export requires Python + optimum toolchain (one-time setup step).
- If OSS users must run `optimum-cli export onnx` before first use, the deployment
  story is more complex than candle (which reads safetensors directly).
- Alternative: ship a pre-exported ONNX file in the OSS bundle (increases bundle
  size by ~438 MB).

---

## Combined Recommendation

**TBD**: GO / GO-with-caveats for [library] / NO-GO for both.

_To be populated after measurements are complete._

Recommended library for productionization (if any):

| Library | Recommended? | Primary reason |
|---------|-------------|----------------|
| candle | TBD | |
| ort | TBD | |

---

## Notes on ADR-0044 Principle V

ADR-0044 established Principle V: "no ML runtime in the Rust crate," scoped to the
Apple-Silicon/ANE out-of-process strategy. The spike results should inform whether
Principle V should be **amended for the OSS bundle scope**:

- If this spike returns GO or GO-with-caveats for either library, a productionization
  follow-up issue should be filed. That follow-up should create **ADR-0051** recording
  the amended scope of Principle V: "retained for macOS production Liminis-app; relaxed
  for the cross-platform OSS distribution where no ANE is available."
- If this spike returns NO-GO for both libraries, Principle V is moot — the OSS bundle
  must pursue Option A (Mac-first with documented sidecar options for other platforms)
  or Option D (pluggable multi-embedder bundle).

---

## Follow-up Actions

If verdict is **GO or GO-with-caveats**:

1. File a productionization issue referencing this report and citing measured numbers
   from SC-001 through SC-006 as requirements baseline.
2. Productionization issue should create ADR-0051 (amended Principle V).
3. Productionization implementation modifies `liminis-graph-core/src/embedder.rs` to
   add a `NativeEmbedder` trait implementation behind a feature flag, keeping
   `OaiEmbedder` as the default for existing macOS Liminis-app users.

If verdict is **NO-GO**:

1. Update `liminis-graph/ideas/oss-launch-architecture.md` to record this spike's
   findings as a documented dead end for Option B (native Rust embedder).
2. OSS bundle architecture should pursue Option A (Mac-first) or Option D (pluggable).

---

## Appendix: Raw Measurement Output

_To be attached after measurements are taken._

JSON output files from each run:
- `spikes/native-embedder/results/candle-macos-arm64.json`
- `spikes/native-embedder/results/ort-macos-arm64.json`
- `spikes/native-embedder/results/candle-linux-amd64.json`
- `spikes/native-embedder/results/ort-linux-amd64.json`
