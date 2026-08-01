# Feature Specification: Decide the max_tokens Policy and Edge Budget-Exhaustion Semantics

**Feature Branch**: `fabrik/issue-307`
**Created**: 2026-08-01
**Status**: Draft
**Input**: User description: "core: decide the max_tokens policy and edge budget-exhaustion semantics (needs ADR)"

## Background

`INITIAL_MAX_TOKENS: u32 = 8192` is hardcoded at four call sites in `crates/core/src/extractor.rs`: Anthropic entities (`:270`), Anthropic edges (`:409`), OAI-compatible entities (`:1438`), OAI-compatible edges (`:1571`). There is no env var or CLI override. A one-shot doubling retry on budget exhaustion gives an effective ceiling of 16,384 tokens.

Two problems, one of which is a data-integrity issue rather than a tuning question:

**1. The number is arbitrary.** It is not derived from measurement, it is uniform across entity calls (small output) and edge calls (large output) despite the ontology adding +1,734 chars to the edge prompt alone, and it is not proportional to input — a 204-char chunk and a 16,670-char chunk get the identical budget, so it binds hardest exactly where content is densest. Corpus chunks measure median 743 chars, p95 6,411, max 16,670 (`docs/history/extraction-eval-2026-07.md`).

**2. Edge exhaustion loses data silently.** Entity exhaustion after retry returns `Err` and the chunk fails visibly (`extractor.rs:364`, `:1531`). Edge exhaustion after retry returns `Ok(vec![])` (`extractor.rs:528`, `:1686` — deliberate, "not fatal", "matches Anthropic"), so the chunk is recorded as a **success with zero edges**. Downstream that is silent missing knowledge, not a visible error.

**#306 has since landed** (merged as PR #308) and changes what's still open here. It added, for every extraction failure including edge-budget exhaustion: a `chunk_key` on `TelemetryEvent::ExtractionTruncated`, a per-candidate `truncated` count in the eval report (distinguishing `retry_succeeded` from exhausted-after-retry), and a `<cassette>.failures.jsonl` sidecar carrying the complete raw response body, `finish_reason`, `completion_tokens`, and `max_tokens` for the failing call. So the truncation event itself is no longer thrown away — it's captured in telemetry and on disk. #306's own scope statement is explicit that it stopped there: *"Changing edge-budget-exhaustion semantics (e.g. making it fatal, changing the retry/doubling policy) — explicitly deferred ... this issue is observability only"* (`docs/adr/0306-extraction-failure-sidecar-and-truncation-visibility.md`). What #306 did **not** change is the *return contract* of `do_extract_edges` — it still returns `Ok(vec![])`, so a chunk-level consumer (the WAL writer, an eval scorer working chunk-by-chunk) still cannot tell "truncated" from "genuinely empty" without separately cross-referencing telemetry or the sidecar. Deciding whether that return contract should also change is this issue's scope.

This is not hypothetical: `qwen3.6-35b-a3b` returned zero edges on two chunks where Haiku found 36 and 38 and `qwen3.6-27b` found 49 each, which is enough to account for its entire measured edge-recall deficit. #306's sidecar now makes it possible to inspect the raw response for those two chunks and determine whether that was truncation or a genuine empty result (SC-004).

### Constraints that bound the design

- **The Anthropic Messages API requires `max_tokens`.** It is a mandatory request field, so "no limit" is not available on the hosted path; some number must be named.
- **A cap is a genuine runaway guard, and runaways are real here.** Ollama's gemma-4 generated the full 8192 tokens from a 1500-char input. Uncapped, that becomes generation until context exhaustion — minutes of GPU per chunk, and unbounded spend on a hosted model.
- **It bounds tail latency**, already p99 321s on the MoE.

So the question is not "cap or no cap" but "what shape of cap, and what happens when it binds".

### Resolution

- **Edge budget exhaustion (FR-004)**: returns `Err`, matching the entity path. `Ok(vec![])` did not just hide the failure from telemetry — it corrupted eval measurement, scoring a suppressed edge call identically to a model that genuinely found nothing. `qwen3.6-35b-a3b`'s edge recall (0.878) is close enough to `qwen3.6-27b`'s (0.900) that the gap may be entirely an artifact of the two zero-edge chunks. `Err` excludes the chunk and counts it as an error instead, which the existing scored-chunk arithmetic (`scored = 228 − own_errors − 1`) already handles correctly. The one real cost of `Err` — discarding entities already extracted before the edge call failed — is answered by FR-007 (a forensics record), not by keeping `Ok(vec![])`.
- **Uniform ceiling, sized for headroom (FR-006)**: cost control is not the driving concern — the requirement is narrower, to stop non-termination (a response that never ends), and less-than-optimal token efficiency is acceptable. So the ceiling is a single uniform value across the hosted Anthropic path and self-hosted/OAI-compatible models, not a provider-specific clamp; a per-provider clamp only earns its complexity if per-call spend is being optimized, and it is not. It is sized generously — well above plausible legitimate need — since its job is to catch genuine non-termination (the measured example: Ollama's gemma-4 generating a full 8,192 tokens from a 1,500-char input), not to trim well-behaved responses. Headroom is preferred over tightness because the two failure modes are asymmetric: a few extra tokens cost a little money, while truncating a well-behaved response corrupts a measurement (FR-004) or silently loses production knowledge — the more expensive failure by far. `max_tokens` remains a weak instrument for cost control specifically, but since cost is not the driver here, the design does not contort around it (see Out of Scope).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A dense chunk is not silently truncated (Priority: P1)

A large, relation-dense chunk is ingested. Either the extraction completes within budget, or the truncation is visible at the point where the result is consumed — not only in aggregate telemetry that consumer may not see. The chunk is never treated as a clean success with zero edges when the model was actually cut off.

**Why this priority**: This is the data-integrity defect motivating the issue. #306 made truncation observable in aggregate (per-candidate counts, a sidecar file), but a chunk-level consumer still can't distinguish a truncated chunk from a clean one from the `ExtractionResult` alone.

**Independent Test**: Force a chunk's edge extraction to exhaust its token budget after the retry (e.g. against a cassette or live backend with a deliberately small budget). Confirm the call now returns `Err` rather than `Ok(vec![])`, and that the already-extracted entities for that chunk are recoverable from the `ExtractionFailureRecord` sidecar (FR-007) even though they are not returned to the caller.

**Acceptance Scenarios**:

1. **Given** an edge-extraction call that exhausts its token budget after retry, **When** the call completes, **Then** it returns `Err` (excluding the chunk and counting it as an error) rather than `Ok(vec![])`, so it is never indistinguishable from a call where the model genuinely returned zero edges.
2. **Given** the corpus's largest chunk (16,670 chars), **When** it is extracted under the new token-budget policy, **Then** it completes without truncation on both the incumbent (Haiku) and the MoE (qwen3.6-35b-a3b).

---

### User Story 2 - A runaway is still contained (Priority: P1)

A degenerate model that will not stop generating (e.g. Ollama's gemma-4, observed generating the full 8192-token budget from a 1,500-char input) is halted rather than left to run to context exhaustion, and the halt is attributable to a specific chunk and model.

**Why this priority**: Equal priority to Story 1 — the fix for silent truncation must not remove the runaway guard that a mandatory `max_tokens` currently provides. Without it, an uncapped local run is minutes of GPU per chunk, and an uncapped hosted run is unbounded spend.

**Independent Test**: Run extraction against a model known to ignore stop conditions. Confirm generation halts at the configured ceiling and the halt is visible in telemetry/report output, attributable to the specific chunk and model.

**Acceptance Scenarios**:

1. **Given** a model that would otherwise generate without bound, **When** it is extracted under the new policy, **Then** generation halts at the configured ceiling and the event is recorded with enough detail (chunk, model) to attribute it.

---

### User Story 3 - The limit can be tuned without a rebuild (Priority: P2)

An operator running a verbose local model whose responses legitimately need more than the default ceiling raises it via configuration, without recompiling the binary.

**Why this priority**: Lower than Stories 1–2 because it's an operability improvement rather than a correctness fix — the current hardcoded constant works, it's just inflexible. Still necessary because the "shape of cap" chosen for Stories 1–2 will only be safe in practice if operators can adjust it for their own model's behavior.

**Independent Test**: Set the relevant configuration to a non-default value, run an extraction, and confirm the effective budget used in the request reflects the configured value rather than a compiled-in constant.

**Acceptance Scenarios**:

1. **Given** an operator sets a configuration value for the token-budget policy, **When** an extraction call is made, **Then** the request uses the configured value without requiring a rebuild.

---

### Edge Cases

- A chunk whose input alone approaches the model's context window — the output budget cannot simply scale linearly, since input + output together are bounded by the same context window.
- Models with very different context windows behind the same OAI-compatible endpoint — there is currently no per-model context-window registry in the codebase (`crates/core/src/extractor.rs`) for a proportional ceiling to consult.
- The doubling retry interacting with a proportional cap — does it still double, or is one attempt now sufficient once the initial budget is no longer a uniform, possibly-too-small default? With a proportional cap (FR-002), exhaustion should become rare, which is also why the FR-004/FR-006 semantics below are optimized for being safe in the rare case rather than cheap.
- **Entity salvage on edge failure (resolved)**: `do_extract` (`extractor.rs:544-555`, `:1708-1719`) calls `do_extract_edges(...).await?`; the `?` now intentionally propagates `Err`, discarding entities from the caller's return value per the FR-004 decision. The entities are not lost entirely — FR-007 requires them (or their count) to be captured in `ExtractionFailureRecord` for forensics, even though the API caller does not receive them.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST replace the four hardcoded `INITIAL_MAX_TOKENS` constants with a single policy, applied consistently across the Anthropic and OAI-compatible paths.
- **FR-002**: The output token budget MUST scale with input size rather than being uniform, with a floor for small chunks and a ceiling bounded by the model's context window.
- **FR-003**: The token-budget policy MUST be configurable without recompilation.
- **FR-004**: On edge budget exhaustion after retry, the system MUST propagate `Err`, matching the entity path — an edge-exhausted chunk is excluded and counted as an error rather than recorded as a success with zero edges. This is a deliberate reversal of the previous "not fatal" behavior (see Background: Resolution). The entity-loss consequence of this choice is addressed by FR-007, not by retaining the old `Ok(vec![])` behavior.
- **FR-005**: The decision MUST be recorded in an ADR numbered with this issue number (`docs/adr/0307-*.md`) per this repository's ADR convention. The ADR MUST record the reasoning behind both decisions, including specifically: for FR-004, the measurement-corruption argument (silent `Ok(vec![])` misattributes suppressed edges to the model's own recall, as demonstrated by the qwen3.6-35b-a3b comparison); for FR-006, that the ceiling exists to stop non-termination rather than to optimize spend, so it is uniform across providers and deliberately generous — the asymmetry between a slightly-too-large response (a little money) and a truncated one (corrupted measurement or lost knowledge) is why headroom is preferred over tightness.
- **FR-006**: The token-budget policy MUST use a single uniform ceiling across the hosted Anthropic path and self-hosted/OAI-compatible models — not a provider-specific clamp — sized generously so it interrupts genuine non-termination (e.g. the observed gemma-4 case: a full 8,192 tokens generated from a 1,500-char input) rather than trimming well-behaved responses. The design MUST bias toward headroom over tightness: truncating a well-behaved response is more expensive (corrupted measurement or lost knowledge, per FR-004) than the modest extra spend of a generous ceiling (see Background: Resolution).
- **FR-007**: When edge extraction fails after entity extraction has already succeeded for the same chunk, the system MUST include the already-extracted entities (or at minimum their count) as a new optional field on `ExtractionFailureRecord` (added by #306), so that data is recoverable for forensics even though `Err` (FR-004) discards it from the caller's return value.

### Key Entities

- **`ExtractionResult`** (`crates/core/src/types.rs:89`): the parsed `{entities, edges}` pair returned per chunk. Unaffected by this issue's schema — edge-budget exhaustion now returns `Err` instead of populating this type with an empty edge list.
- **`ExtractionFailureRecord`** (added by #306; backed by `TelemetryEvent::ExtractionFailure` and written to `<cassette>.failures.jsonl` via `crates/core/src/extraction_failures.rs`): gains a new optional field carrying the already-extracted entities (or their count) from a chunk whose edge extraction failed after its entity extraction succeeded (FR-007).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: No chunk in a full-corpus run is recorded as a success while having had its edge output truncated — an edge-exhausted chunk now returns `Err` and is counted as an error, not scored as a success with zero edges.
- **SC-002**: A synthetic runaway is still halted, and the halt is visible in the report.
- **SC-003**: The corpus max chunk (16,670 chars) extracts without truncation on both the incumbent (Haiku) and the MoE (qwen3.6-35b-a3b).
- **SC-004**: Re-running the qwen3.6-35b-a3b comparison under the new policy resolves whether its edge deficit on the two zero-edge chunks was truncation or a genuine empty result.

## Assumptions

- #306 has landed (merged via PR #308, 2026-08-01) — the failure-capture and truncation-visibility infrastructure this issue builds on already exists; it does not need to be re-implemented here.
- The eval corpus is representative enough to set the proportionality constant for FR-002, with the WebBrain 257KB page noted as a known pathological input beyond it.
- The exact proportionality formula, floor, and ceiling values for FR-002/FR-006 are an empirical/technical determination left to Research and Plan, bounded by the testable outcome in SC-003 — this spec establishes that the ceiling MUST be uniform across providers and MUST be sized generously (headroom over tightness), not the specific numbers.

## Out of Scope

- Re-implementing or extending #306's failure-capture sidecar, telemetry fields, or eval-report counters, beyond the new field added by FR-007 — that infrastructure already exists and this issue consumes/extends it narrowly.
- Determining the exact proportionality constant, floor, and ceiling values for the token-budget formula — an empirical Research/Plan task, not a spec-level decision (see Assumptions).
- Any extraction backend other than the Anthropic Messages API path and the OAI-compatible path already present in `crates/core/src/extractor.rs`.
- Changing how entity-extraction budget exhaustion behaves — it already propagates `Err` today and that is not in question here; only the edge path's semantics are being decided.
- Any provider-specific token-budget clamp, per-run/aggregate cost-budget guard, or spend-tracking machinery. FR-006 explicitly rejects a hosted-specific ceiling — cost is not the concern this issue optimizes for, and contorting the design around it is out of scope.
- Bounding a runaway *session* (repeated over-budget calls across many chunks in a run). FR-006's ceiling bounds a single call; session/job-level runaway bounding, if needed, belongs at that layer and is a separate issue.

## Source References

- `crates/core/src/extractor.rs:270`, `:409`, `:1438`, `:1571` — the four `INITIAL_MAX_TOKENS` constants.
- `crates/core/src/extractor.rs:528`, `:1686` — the two `Ok(vec![])` non-fatal edge-budget-exhaustion returns (Anthropic, OAI-compatible) that FR-004 replaces with `Err`.
- `crates/core/src/extractor.rs:544-555`, `:1708-1719` — `do_extract`, where the `?` on the edges call propagates `Err` and discards already-extracted entities from the return value (see Edge Cases, FR-007).
- `crates/core/src/telemetry.rs` — `TelemetryEvent::ExtractionTruncated` (carries `chunk_key`) and `TelemetryEvent::ExtractionFailure`, both added/extended by #306; `ExtractionFailure` gains the FR-007 field.
- `crates/core/src/extraction_failures.rs` — the `<cassette>.failures.jsonl` sidecar writer added by #306.
- `docs/adr/0306-extraction-failure-sidecar-and-truncation-visibility.md` — the ADR for #306; its Out of Scope explicitly deferred this issue's decision.
- `docs/history/extraction-eval-2026-07.md` — the qwen3.6-35b-a3b analysis this issue references, including corpus chunk-length percentiles and edge-recall figures.
