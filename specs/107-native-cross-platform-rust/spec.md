# Feature Specification: Spike — native cross-platform Rust embedder for liminis-graph (candle vs ort)

**Feature Branch**: `fabrik/issue-107`
**Created**: 2026-05-26
**Status**: Draft
**Input**: User description: "Before deciding the OSS bundling shape, spike whether liminis-graph can embed BGE-base-en-v1.5 natively in the Rust binary (no IPC, no sidecar) on macOS / Linux / (Windows). Compare `candle` vs `ort`. Output is a GO / NO-GO / GO-with-caveats decision with measured numbers — not a production-ready replacement. Productionization, if green, becomes a separate follow-up."

## Background

The CoreML BGE-base embedder cutover shipped 2026-05-25 (liminis#794, #809, #810, #811, liminis-graph#81). The Swift sidecar serves OpenAI-style `POST /v1/embeddings` over UDS with full quality parity (cosine sim ≥0.9999) and 20 ms p95 latency.

The Liminis app on macOS now routes all embedding traffic through that sidecar. End-to-end working in production today.

**The OSS bundling question:** the user wants to ship `liminis-graph` + an embedder as a drop-in open-source context graph engine (OSS-launch planning, embedder question). The Swift sidecar is **Mac-only** — bundling it as the embedder would make the OSS distribution Mac-only too, which contradicts "drop in to any project."

Three architectural alternatives surfaced during planning:

- **A.** Mac-first OSS with documented sidecar options for other platforms (lowest implementation cost, narrowest audience)
- **B.** Native cross-platform embedder in the Rust binary (one binary, works everywhere, no IPC) — **this spike answers whether (B) is viable**
- **D.** Stay pluggable and bundle multiple reference embedders (most surface area)

Option B violates the original liminis-graph design principle "no ML runtime in the Rust crate" (Principle V, scoped to the Apple-Silicon/ANE strategy where out-of-process embedding had architectural payoff). On Linux/Windows servers there is no ANE — out-of-process embedding is pure IPC cost with no architectural gain. The principle may be the wrong rule for OSS distribution; this spike is one of the inputs to that decision.

This spike is structurally a copy of the original CoreML spike (liminis#787): produce a GO/NO-GO with measured numbers, do not productionize. The output is a written decision report ready to be the input to the OSS-launch follow-up.

**Two candidate Rust libraries:**

- **`candle`** (huggingface/candle) — pure-Rust ML framework from HuggingFace, designed for inference, supports BGE-base, no external runtime dependency. Pure Rust build story; pulls in minimal native deps.
- **`ort`** — Rust bindings around Microsoft's ONNX Runtime. More mature, faster on most platforms (uses optimized native kernels), but requires `libonnxruntime` at build/runtime. Larger binary, broader optimization surface.

Both are plausible; the spike answers which (if either) clears the production bar for OSS bundling.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The user gets a yes/no answer with numbers (Priority: P1)

A maintainer reads the decision report and can immediately tell whether native Rust embedding is viable for the OSS bundle. The report contains measured latency, measured memory, measured quality parity, build complexity assessment, and a clear recommendation: GO with one of the two libraries, NO-GO with reasons, or GO-with-caveats listing the conditions.

**Why this priority**: This is the entire deliverable. Without measured numbers, the OSS bundling decision can't move forward responsibly. The original CoreML spike report (`liminis/docs/project_notes/designs/coreml-bge-embedder-spike-2026-05.md`) is the format model.

**Independent Test**: Read the produced decision report. Confirm it contains: (a) latency measurements for both `candle` and `ort` on at least Mac arm64 and Linux x86_64, (b) memory footprint per process, (c) cosine-parity numbers vs PyTorch BGE-base on the same 50-sentence reference set the CoreML spike used, (d) a stated GO / NO-GO / GO-with-caveats verdict with explicit reasons.

**Acceptance Scenarios**:

1. **Given** the spike has run, **When** a maintainer reads the report, **Then** they can answer "should we adopt candle or ort for native embedding in the OSS bundle?" without consulting code.
2. **Given** the report's verdict is GO or GO-with-caveats, **When** a productionization follow-up is filed, **Then** the follow-up cites specific numbers and conditions from the report as its requirements baseline.
3. **Given** the report's verdict is NO-GO, **When** the OSS bundle architecture is finalized, **Then** the NO-GO reasons are folded into the OSS-launch architecture doc as a documented dead end.

---

### User Story 2 - The spike does NOT productionize (Priority: P1)

The spike answers the feasibility question only. It does not modify `liminis-graph-core/src/embedder.rs` to use the new path, does not delete the existing `OaiEmbedder` HTTP/UDS path, does not change the Rust binary's default behavior. All measurement code lives in a separate crate or in a temporary `spikes/` directory and is not wired into the main build.

**Why this priority**: Spikes that incidentally ship implementation work create surface area to maintain that may never be wanted. Keep the blast radius surgical.

**Acceptance Scenarios**:

1. **Given** the spike branch has been validated and merged, **When** an integrator builds `liminis-context-graph` from main, **Then** the binary behaves identically to its pre-spike behavior — same embedder transport selection, same IPC contract, same CLI flags.
2. **Given** the spike concluded NO-GO, **When** the branch is merged, **Then** the only artifacts that land are the decision report and any benchmark code; no `Cargo.toml` dependencies are added that aren't behind a feature flag or in a separate test-only crate.

---

### Edge Cases

- What if `candle` cannot load BGE-base-en-v1.5 reliably from a HuggingFace checkpoint (e.g., tokenizer mismatch, weight format issue)? Document the gap as a known blocker; mark NO-GO for `candle` while still measuring `ort`.
- What if `ort` requires a system-installed `libonnxruntime` that isn't trivially available on all target platforms? Note the deployment complexity; weigh against the latency benefit. May still be GO-with-caveats if the deployment story is workable.
- What if both libraries clear the latency bar but one is 5× larger in binary size? Report both numbers and let the OSS-launch decision weigh them.
- What if the Apple Silicon GPU path through `candle` (Metal) requires a separate code path from CPU? Document; the OSS user on Apple Silicon getting GPU acceleration "for free" would be a meaningful win.
- What if the result is "candle works on Mac but is too slow on Linux x86_64"? Report per-platform; the OSS decision may accept a slower Linux experience.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The spike MUST measure single-input embedding latency for both `candle` and `ort` on at least two platforms: macOS Apple Silicon (the developer's primary machine) and Linux x86_64 (the typical OSS deployment target). Windows is nice-to-have but not blocking; if measurements are easy to obtain in CI on Windows, include; otherwise document as deferred.
- **FR-002**: The latency benchmark MUST use the same 200-sentence corpus as the original CoreML spike's `bench.py` (or document why a different corpus was substituted) so the numbers are comparable to the existing CoreML and Python baselines.
- **FR-003**: The spike MUST report p50, p95, p99, min, max for single-input embedding latency on each platform-library combination, plus a 200-sentence batch-throughput number per second.
- **FR-004**: The spike MUST validate cosine similarity vs reference PyTorch `sentence-transformers` BGE-base-en-v1.5 on the same 50-sentence reference set the CoreML spike used. Required threshold: ≥0.999. Report actual min/max/mean similarity.
- **FR-005**: The spike MUST measure resident-set memory at steady state for each library after model load + 100 warmup requests. Report in MB.
- **FR-006**: The spike MUST measure cold-start time from process launch to first successful embedding. Report seconds.
- **FR-007**: The spike MUST measure the impact on `liminis-graph`'s binary size and build time when each library is added. Report Cargo dependency count delta, total binary size delta, clean-build time delta.
- **FR-008**: The spike MUST surface deployment-story complexity for each library — does it require system libraries (e.g. libonnxruntime), does it bundle weights, what does first-launch model download look like for the OSS user, are there licensing concerns with the underlying ML framework.
- **FR-009**: The decision report MUST contain a single explicit verdict line for each library: GO, NO-GO, or GO-with-caveats (with caveats enumerated). A combined recommendation for which (if either) to adopt MUST be stated.
- **FR-010**: All measurement code MUST live in a separate test/benchmark crate or under `spikes/` — not wired into the main `liminis-graph-core` Cargo dependencies. The spike does NOT alter the existing `embedder.rs` runtime behavior or add a build-time dependency on either library to the production crate.
- **FR-011**: The decision report MUST be committed to a stable path under `liminis-graph/docs/` (e.g., `docs/spikes/native-embedder-2026-05.md`) so the follow-up productionization issue can reference it by path.

### Key Entities

- **Decision report**: Markdown document at `liminis-graph/docs/spikes/native-embedder-2026-05.md` (or similar), structured per the CoreML spike report format. Lives in `docs/`, not `ideas/`, because it's a recorded outcome rather than exploratory.
- **Benchmark harness**: Rust binary or test crate that runs the measurements. Located outside `liminis-graph-core` to avoid contaminating production dependencies.
- **Reference corpus**: The 200-sentence latency corpus and 50-sentence parity corpus from the original CoreML spike. Reused so numbers are comparable.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Cosine similarity vs PyTorch BGE-base ≥0.999 on the 50-sentence reference set for whichever library is recommended GO. Lower = NO-GO for that library on quality grounds.
- **SC-002**: Single-input p95 latency on Apple Silicon ≤25 ms for the recommended library. (Generous vs the Swift sidecar's 20 ms p95 because the spike measures library-in-isolation; the Rust binary integration would have no UDS hop, potentially trimming below 20 ms — but at this stage 25 ms is the floor that says "could plausibly beat the Swift sidecar after integration.")
- **SC-003**: Single-input p95 latency on Linux x86_64 ≤50 ms for the recommended library. Linux has no ANE; CPU-only inference is the baseline. 50 ms is acceptable for the OSS workload where the alternative (Python sentence-transformers) is in the 50-200 ms range.
- **SC-004**: Resident memory at steady state ≤500 MB for the recommended library, including loaded BGE-base weights. Lower is better; 500 MB is the soft ceiling above which the OSS adoption story gets awkward.
- **SC-005**: Cold start from process launch to first successful embedding ≤2 seconds for the recommended library on either platform. (CoreML spike's first-launch compile was 5–15 s; the OSS bundle would be a non-trivial regression if cold start is multi-second every run.)
- **SC-006**: Spike-induced binary size growth for `liminis-context-graph` ≤50 MB. Above this, the "single small binary" OSS aesthetic erodes meaningfully.
- **SC-007**: Decision report (the deliverable) is written, committed, and verdict-line clearly states GO / NO-GO / GO-with-caveats for each library plus a combined recommendation.

## Assumptions

- The 200-sentence latency corpus and 50-sentence parity corpus from the CoreML spike are accessible (at `liminis/liminis-app/native/local-inference/benchmark/`) and reusable. If not, the spike substitutes equivalent corpora and documents the choice.
- BGE-base-en-v1.5 is the target model. Other models (bge-small, bge-large, ada-002) are out of scope for the spike — they may be added later as deployment options.
- CPU is the baseline path for both libraries on all platforms. GPU paths (Metal via `candle`, CUDA via `ort`) are nice-to-have measurements but not required for the GO/NO-GO. If a library exposes them easily and the developer's hardware supports them, include the numbers.
- The Rust developer running the spike has Rust + Cargo installed on macOS and access to a Linux machine (or Docker) for cross-platform measurements. If the Linux measurements require non-trivial setup, document the setup steps as part of the spike artifacts.
- Tokenizer for BGE-base is loadable in both libraries via either bundled tokenizer files or a HuggingFace download. The original CoreML spike's tokenizer staging script (`liminis-app/native/local-inference/prepare-tokenizer.py`) produces compatible files.
- The spike's measurement code may evolve into the eventual productionization implementation, but is not required to. If the spike's measurement code is throwaway, document that explicitly.

## Out of Scope

- **Productionization** of the chosen library. If the verdict is GO or GO-with-caveats, a follow-up issue will spec the actual integration into `liminis-graph-core/src/embedder.rs`.
- **Removing the existing `OaiEmbedder`** path. The HTTP/UDS transport stays for external embedders (OpenAI, self-hosted text-embedding-inference, the Swift sidecar for non-OSS Liminis-app users).
- **Multi-model support** (bge-large, ada-002, etc.). The spike validates BGE-base only.
- **Quantization experiments** (int8, FP16 vs FP32 trade-offs). If the chosen library defaults to a particular precision, use that; document the choice.
- **GPU dispatch on Linux** (CUDA path). If trivially measurable, include; otherwise defer.
- **Choosing between Mac-first OSS (option A) vs cross-platform OSS (option B) vs bundled-multi-embedder OSS (option D)**. This spike provides the input to that decision; the decision itself is made elsewhere (likely in the OSS-launch architecture doc revision).
- **Linux distribution packaging details** (apt, dnf, brew, docker image). Out of scope; the spike answers "is the library workable," not "how do we package it."

## Source References

- OSS-launch planning notes (internal) — the planning this spike feeds; the embedder-bundling question.
- `ideas/cutover-plan.md` — context on why the engine reached production.
- `liminis/docs/project_notes/designs/coreml-bge-embedder-spike-2026-05.md` — the format model for the deliverable decision report.
- `liminis-app/native/local-inference/benchmark/bench.py` — original CoreML benchmark; corpus source.
- `liminis-app/native/local-inference/verify-embedding-parity.py` — original parity check; reusable methodology.
- `liminis-graph-core/src/embedder.rs` — current `OaiEmbedder` implementation; what native embedding would augment or replace.
- `candle` repository: https://github.com/huggingface/candle
- `ort` repository: https://github.com/pykeio/ort
- BGE-base-en-v1.5: https://huggingface.co/BAAI/bge-base-en-v1.5
