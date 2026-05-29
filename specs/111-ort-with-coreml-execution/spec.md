# Feature Specification: Spike — ort with CoreML execution provider on macOS (can it match or beat the Swift sidecar?)

**Feature Branch**: `fabrik/issue-111`
**Created**: 2026-05-28
**Status**: Draft
**Input**: User description: "Building on liminis-graph#107 (which measured ort on CPU at 26 ms p95 on macOS arm64), measure ort with the CoreML execution provider. Question: can ort+CoreML EP on macOS match or beat the Swift sidecar's 20 ms p95? If yes, the OSS bundle can consolidate on a single ort-based embedder path for both Mac and Linux. If no, keep the split: Swift sidecar for Mac, ort for Linux/Windows."

## Background

liminis-graph#107 (closed 2026-05-26, decision report at `liminis-graph/docs/spikes/native-embedder-2026-05.md`) measured two Rust ML libraries against BGE-base-en-v1.5:

- **candle**: NO-GO (163 ms p95 on macOS CPU, 838 MB RSS on Linux)
- **ort with CPU execution provider**: GO-with-caveats (26 ms p95 macOS, 25 ms p95 Linux, 397 MB RSS)

The Liminis app on macOS currently uses the **Swift CoreML sidecar** (shipped 2026-05-25) which measured at **20 ms p95** in live keep-alive testing. The Swift path dispatches to CoreML's runtime-selected compute units (ANE/GPU/CPU) and outperforms ort+CPU by ~6 ms on the same hardware.

The recommendation from #107 was to keep both: Swift CoreML sidecar for Liminis-app (faster on Mac), ort for cross-platform OSS. But that recommendation rested on ort's **CPU-only** numbers. **ort also supports a CoreML execution provider** on macOS — the CPU number is a lower bound, not the ceiling.

This spike answers the open question: when ort dispatches through its CoreML execution provider on Apple Silicon, does it match or beat the Swift sidecar's 20 ms? The answer determines whether the OSS bundle can adopt a single Rust embedder path everywhere, or whether the split (Swift sidecar on Mac, ort on Linux) is the right long-term architecture.

This is structurally a focused follow-up to #107 — reuse its benchmark harness, corpus, and methodology. New measurement axis: execution provider. New question: is ort+CoreML EP ≤ 20 ms p95 on macOS arm64?

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The user gets a numeric answer to the consolidation question (Priority: P1)

A maintainer reads the spike's decision and can immediately answer: "should the OSS bundle use one Rust embedder path everywhere (ort+CoreML EP on Mac, ort+CPU on Linux), or two paths (Swift sidecar on Mac, ort on Linux)?" The answer is grounded in measured numbers comparable to liminis-graph#107 and to the Swift sidecar's live production numbers.

**Why this priority**: This is the entire deliverable. Without the measurement, the OSS bundle's Mac story can't be finalized — we either over-engineer with two embedder paths or under-engineer with a slower one. The spike result resolves the question with data.

**Independent Test**: Read the updated decision report section (or addendum to `liminis-graph/docs/spikes/native-embedder-2026-05.md`). Confirm it contains: (a) ort+CoreML EP measurements for macOS arm64 single-input latency (p50/p95/p99/min/max), (b) batch throughput, (c) RSS, (d) cold start, (e) cosine parity numbers, (f) an explicit verdict line — "ort+CoreML EP on macOS: GO for consolidation (≤20 ms p95)" or "ort+CoreML EP on macOS: keep split (>20 ms p95)" — with reasoning.

**Acceptance Scenarios**:

1. **Given** the spike has run, **When** a maintainer compares ort+CoreML EP p95 against the Swift sidecar's 20 ms p95, **Then** they can state with measured confidence whether ort+CoreML EP is faster, comparable, or slower.
2. **Given** the verdict is GO for consolidation, **When** the OSS productionization issue is filed, **Then** it can specify ort+CoreML EP on macOS and ort+CPU on Linux as a single feature-flagged path, rather than dual code paths.
3. **Given** the verdict is "keep split", **When** the OSS productionization issue is filed, **Then** it specifies ort for OSS only, while Liminis-app continues using the Swift sidecar via the existing `OaiEmbedder` UDS path.

---

### Edge Cases

- What if ort+CoreML EP fails to load BGE-base ONNX with the CoreML EP enabled (compatibility gap)? Document the gap; verdict becomes "ort+CoreML EP not viable; keep split."
- What if ort+CoreML EP latency is highly variable (large p99-p50 gap)? The Swift sidecar's p99 is within 0.5 ms of p50 — extreme determinism. If ort+CoreML EP has wide tail latency even when its median is fast, that may be enough to keep the Swift sidecar for the desktop app where stutter matters.
- What if ort+CoreML EP works on Apple Silicon but not on macOS Intel? Document; the OSS adopter on Intel Mac gets ort+CPU (the #107 numbers); the Apple Silicon adopter gets the CoreML EP path. This still counts as "single ort embedder path with optional acceleration" not "two embedder paths."
- What if ort+CoreML EP fixes the 1/50 tokenizer parity issue from #107 (because CoreML uses a different tokenization path internally)? Unlikely — the tokenizer is Rust-side; the EP only affects compute — but verify. If yes, that's a bonus; if no, the parity issue remains a productionization task regardless.
- What if ort+CoreML EP requires a system component (CoreML.framework, certain macOS version) that's not universally available on all supported macOS versions? Document the floor; the verdict can still be GO conditional on macOS version.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The spike MUST measure ort with the CoreML execution provider on macOS Apple Silicon, using the same BGE-base-en-v1.5 model that liminis-graph#107 measured.
- **FR-002**: Measurements MUST reuse the harness, corpus, and methodology of liminis-graph#107: 200-sentence latency corpus, 50-sentence parity reference, p50/p95/p99/min/max single-input latencies, 200-sentence batch throughput per second, cold start, RSS.
- **FR-003**: The spike MUST measure p50/p95/p99/min/max single-input latency with at least 100 warmup + 200 timed iterations (the #107 macOS-arm64 protocol), so the numbers are directly comparable.
- **FR-004**: The spike MUST measure cosine similarity vs the same PyTorch BGE-base reference embeddings (`spikes/native-embedder/reference_embeddings.json`) the #107 spike used. Report identically-formatted parity statistics.
- **FR-005**: The spike MUST report whether ort+CoreML EP requires explicit ANE/GPU/CPU compute unit configuration, and which compute unit was actually used at runtime (verifiable via CoreML's logging or via `Instruments`/`powermetrics` if needed).
- **FR-006**: The decision report MUST contain a single verdict line comparing ort+CoreML EP p95 against the 20 ms Swift-sidecar baseline: "ort+CoreML EP: GO for consolidation" (if ≤20 ms p95 with comparable tail behavior to Swift sidecar) or "ort+CoreML EP: keep split" (if >20 ms p95 or significantly worse tail behavior), with stated reasoning.
- **FR-007**: The decision report MUST be appended to or merged with the existing `liminis-graph/docs/spikes/native-embedder-2026-05.md` (not a separate file). The existing report's "Combined Recommendation" section should be updated to reflect the new measurement.
- **FR-008**: All measurement code MUST live alongside the existing #107 harness at `spikes/native-embedder/` and not be wired into `liminis-graph-core` production code. This spike does not productionize anything.
- **FR-009**: The spike MUST NOT attempt to fix the 1/50 tokenizer parity issue from #107. That fix belongs in the productionization issue, not in this measurement-only spike. The parity check is reported only for comparison; failure to improve parity is not a NO-GO criterion for this spike.
- **FR-010**: If ort+CoreML EP fails to load the BGE-base ONNX model at all (compatibility gap), the spike MUST document the failure mode in detail and recommend "keep split" — no further measurement required.

### Key Entities

- **ort CoreML execution provider**: The macOS-specific backend in ONNX Runtime that dispatches model operations to CoreML's runtime, which itself selects ANE/GPU/CPU per-operation. Distinct from ort's CPU execution provider (the #107 baseline).
- **Decision report addendum**: New section appended to `native-embedder-2026-05.md`. Documents the additional measurement and updates the combined recommendation.
- **Comparison baseline**: The Swift CoreML sidecar's 20 ms p95 from live keep-alive measurement, and #107's ort+CPU 26 ms p95.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: ort+CoreML EP single-input p95 latency on macOS Apple Silicon is measured and reported with the same precision as #107 (n=200 iters minimum, 100+ warmup).
- **SC-002**: A clear GO / NO-GO consolidation verdict is stated in the report. Verdict is GO when ort+CoreML EP p95 ≤ 20 ms AND tail-latency behavior is comparable to the Swift sidecar (p99/p50 ratio ≤ 1.5×, similar to the Swift sidecar's ~1.02× ratio per the original spike). Verdict is NO-GO ("keep split") otherwise, with the specific failing condition cited.
- **SC-003**: Cosine parity numbers are reported and compared against #107's ort+CPU numbers. Identical parity = expected (tokenizer-side issue unchanged). Different parity = surprising; report and flag for productionization investigation.
- **SC-004**: RSS at steady state is reported. Acceptable threshold ≤500 MB (the #107 threshold). ort+CoreML EP is expected to be similar to or higher than ort+CPU (CoreML may keep intermediate buffers); a significant increase (>700 MB) is a yellow flag worth noting.
- **SC-005**: Cold-start time is reported. CoreML compilation on first launch could add seconds; the threshold remains ≤2 s for a cached run, but first-launch compilation time should be reported separately.
- **SC-006**: The decision report's "Combined Recommendation" section is updated to reflect the new measurement. If GO, the recommendation shifts toward single-path ort everywhere with EP selection per platform. If NO-GO, the recommendation remains the dual-path (Swift sidecar + ort) architecture from #107.

## Assumptions

- The ort crate's CoreML execution provider is available and functional in the ort version pinned by #107 (2.0.0-rc.12) or a successor. If the rc.12 build does not support CoreML EP, document and pin to a version that does (or report blocker).
- BGE-base ONNX model produced by the #107 spike (via `optimum-cli export onnx` or equivalent) is compatible with the CoreML EP. If the ONNX export needs different options for CoreML compatibility (e.g., different opset version), document the differences.
- The macOS Apple Silicon hardware running the spike is the same physical machine as the #107 measurements, so platform/load variability does not muddy the comparison.
- The decision report format established in #107 (sections for methodology, results, verdicts, follow-up actions) is the right structure to extend. The addendum follows the same template.
- The spike is measurement-only. If the productionization decision later requires ort+CoreML EP integration, that work is filed as a separate follow-up referencing this report.

## Out of Scope *(optional)*

- **Tokenizer parity fix**: the 1/50 sentence cosine 0.922 outlier is a productionization concern, not a spike concern.
- **Productionization of ort with CoreML EP**: if this spike returns GO, the actual integration into `liminis-graph-core/src/embedder.rs` is a separate follow-up.
- **Linux + CUDA EP** measurements: the original #107 spike measured Linux+CPU; CUDA dispatch on Linux is a separate question that may or may not matter for OSS adoption.
- **macOS Intel measurements**: the spike targets Apple Silicon. Intel Mac is a fallback that should work but isn't where the CoreML EP matters most.
- **Comparison against alternative model quantizations** (int8, fp16): use whatever ort+CoreML EP defaults to. Quantization tradeoffs are a separate study.
- **Re-measuring candle**: ruled out by #107 with two independent failures; not revisiting here.

## Source References *(optional)*

- `liminis-graph/docs/spikes/native-embedder-2026-05.md` — the #107 decision report this spike extends.
- `liminis-graph/spikes/native-embedder/` — the existing benchmark harness to reuse and extend.
- `liminis-graph/spikes/native-embedder/reference_embeddings.json` — PyTorch BGE-base ground truth for parity.
- `liminis-graph/spikes/native-embedder/results/ort-macos-arm64.json` — #107's ort+CPU macOS arm64 result, the direct comparison baseline.
- `liminis/docs/project_notes/designs/coreml-bge-embedder-spike-2026-05.md` — original Swift CoreML spike (the 20 ms p95 baseline).
- ort CoreML execution provider docs: https://onnxruntime.ai/docs/execution-providers/CoreML-ExecutionProvider.html
- ort Rust crate CoreML EP usage: https://ort.pyke.io/perf/execution-providers#coreml (or equivalent ort docs URL — verify at spike time).
