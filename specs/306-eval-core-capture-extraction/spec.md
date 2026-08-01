# Feature Specification: Capture extraction failures whole, and surface truncation in the eval report

**Feature Branch**: `fabrik/issue-306`
**Created**: 2026-08-01
**Status**: Draft
**Input**: User description: "eval/core: capture extraction failures whole, and surface truncation in the report"

## Background

The extraction pipeline currently discards the evidence needed to diagnose its own failures.

- `RecordingExtractor::extract` (`crates/core/src/cassette.rs:320`) is `self.inner.extract(opts).await?` — the `?` returns before `record()` on line 322, so **failed calls are never recorded**. The qwen3.6-35b-a3b capture holds 221 records for 228 chunks; the 7 missing records are exactly the 7 failures.
- What *is* recorded is `serde_json::to_value(&result)` (line 321) — the parsed `ExtractionResult`. **Raw model output is never persisted**, on success or failure. There is no `.text()` capture or `raw_response` field anywhere in `crates/core/src/extractor.rs`.
- `TelemetryEvent::StructuredOutputParse` and `ExtractionTruncated` (`crates/core/src/telemetry.rs:58`) carry counts and metadata but **no payload**, and `ExtractionTruncated` has no chunk identifier — only `chunk_len_bytes`.
- The eval's `CountingSink` (`crates/eval/src/runner.rs:145`) matches **only** `StructuredOutputParse` and discards every other event, including `ExtractionTruncated`.

The combined effect: on edge budget exhaustion the extractor returns `Ok(vec![])` (`extractor.rs:410`, `:1334` — deliberate, "not fatal"), which is byte-identical in the cassette to a model genuinely emitting `{"edges": []}`, and the one signal that would distinguish them is thrown away. **A truncated chunk is reported as clean.**

This is currently blocking a real conclusion: `qwen3.6-35b-a3b` returned zero edges on two chunks where Haiku found 36 and 38 and `qwen3.6-27b` found 49 each (~98 edges, ~3.5% of its total). That is enough to account for its entire 2.2pp edge-recall gap, and we cannot tell whether it truncated or genuinely returned nothing. See `docs/history/extraction-eval-2026-07.md`.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Diagnose why an extraction failed (Priority: P1)

An engineer runs a capture, sees a non-zero error rate, and needs to know what the model actually returned. They open the failures sidecar and read the complete response body, its `finish_reason`, and its token count, and can classify the failure as truncation, runaway generation, or malformed structure without re-running anything.

**Why this priority**: Without this, a failed extraction call leaves no trace at all — the engineer cannot distinguish "the model returned garbage" from "the model returned nothing" from "the HTTP call itself failed." This is the minimum needed to make any failure actionable.

**Independent Test**: Run a capture against a backend known to produce at least one malformed or truncated response. Confirm a `<cassette>.failures.jsonl` sidecar is created containing a record with the complete raw body, `finish_reason`, `completion_tokens`, `max_tokens`, and a failure classification for that call — without re-running the capture.

**Acceptance Scenarios**:

1. **Given** a capture run against a live backend, **When** an extraction call returns a parse error, budget exhaustion after retry, or an HTTP error, **Then** exactly one record is appended to `<cassette>.failures.jsonl` containing the chunk key, `call_type` (`entities`/`edges`), the complete raw response body, `finish_reason`, `completion_tokens`, the `max_tokens` in force, and a failure classification.
2. **Given** a response body that is large (bounded by `max_tokens`, ≤16384 tokens post-retry, ~64KB) and truncated mid-value (e.g. `Unterminated string`, `Expecting ':' delimiter at column 2926`), **When** it is written to the sidecar, **Then** the full body is stored — not a prefix — so the tail defect that caused the failure is visible.

---

### User Story 2 - Distinguish an empty result from a suppressed one (Priority: P2)

An engineer sees a chunk with zero edges. The report tells them whether that chunk hit budget exhaustion or whether the model returned an empty list, so quality conclusions are not drawn from suppressed output.

**Why this priority**: This is the specific defect blocking the qwen3.6-35b-a3b analysis referenced in Background — without it, edge-recall comparisons between backends can be silently wrong.

**Independent Test**: Run a capture against a backend forced to exhaust its edge-extraction token budget on at least one chunk. Confirm the eval report's per-candidate summary shows a non-zero `truncated` count (distinguishing `retry_succeeded` from exhausted-after-retry) instead of reporting that chunk as clean.

**Acceptance Scenarios**:

1. **Given** an edge-extraction call that exhausts its token budget after one retry, **When** the eval report is generated, **Then** the affected candidate's `truncated` count is non-zero and the report distinguishes the exhausted-after-retry case from a `retry_succeeded` case.
2. **Given** a chunk where the model genuinely emits zero edges (no truncation occurred), **When** the eval report is generated, **Then** that chunk is reported as clean, not truncated.

---

### User Story 3 - Reproduce a failure deterministically (Priority: P3)

Because the request is already recorded alongside, an engineer can replay the exact failing call and compare against the stored body.

**Why this priority**: Lower priority than P1/P2 because it depends on data (the request) that the cassette already persists today — this story is about using existing request data together with the new failure record, not persisting anything new.

**Independent Test**: Given a failure record in the sidecar and the corresponding request already present in the cassette or its logs, confirm the two can be correlated via the chunk key and `call_type`, allowing the exact failing call to be reconstructed.

**Acceptance Scenarios**:

1. **Given** a failure record in `<cassette>.failures.jsonl`, **When** an engineer looks up its chunk key and `call_type`, **Then** they can locate the corresponding request content and reconstruct the exact call that failed.

---

### Edge Cases

- HTTP-level failures with no body — record the status and an empty body rather than skipping.
- A failure whose body is not valid UTF-8 — store lossily rather than dropping the record.
- Concurrent writers to the sidecar during a parallel run.
- A cassette in replay mode, where no live failure can occur — the sidecar should simply not be created.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: On any extraction failure (parse error, budget exhaustion after retry, HTTP error), the system MUST write one record to a sidecar `<cassette>.failures.jsonl` containing: the chunk key, `call_type` (`entities`/`edges`), the **complete** raw response body, `finish_reason`, `completion_tokens`, the `max_tokens` in force, and the failure classification.
- **FR-002**: The raw body MUST be stored whole, not truncated to a prefix. Rationale: the response is already bounded by `max_tokens` (≤16384 tokens post-retry, ~64KB), failures are rare, and the defects observed in practice (`Unterminated string`, `Expecting ':' delimiter at column 2926`) are **tail** defects that a prefix would hide.
- **FR-003**: The system MUST bound the aggregate rather than the specimen: cap or rotate the failures file so a long-running service cannot grow it without limit. Individual records stay complete.
- **FR-004**: The system MUST add the chunk key to `TelemetryEvent::ExtractionTruncated`, so a truncation event identifies which chunk it belongs to.
- **FR-005**: `CountingSink` MUST tally `ExtractionTruncated` alongside `StructuredOutputParse`, and the eval report MUST expose a `truncated` count per candidate, distinguishing `retry_succeeded` from exhausted-after-retry.
- **FR-006**: Edge budget exhaustion MUST remain non-fatal for now (semantics are decided separately), but MUST be visible in the report rather than counted as clean.
- **FR-007**: The cassette's success-only invariant and the `#279` duplicate-key/identical-backend guards MUST be unaffected — failures go to the sidecar, not the cassette.

### Key Entities *(include if feature involves data)*

- **Failure Record**: One JSONL row in the `<cassette>.failures.jsonl` sidecar, written once per extraction-call failure. Attributes: chunk key, `call_type` (`entities`/`edges`), complete raw response body, `finish_reason`, `completion_tokens`, `max_tokens` in force, and failure classification (e.g. truncation, malformed/parse error, HTTP error).
- **Extraction Truncated Event**: The existing `TelemetryEvent::ExtractionTruncated`, extended with a chunk key so a truncation signal can be attributed to the chunk that produced it.
- **Per-Candidate Truncated Count**: A new figure in the eval report, per backend/candidate, tallying truncation events and distinguishing calls where a retry ultimately succeeded from calls that remained exhausted after retry.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Re-running the qwen3.6-35b-a3b capture yields, for each of the two zero-edge chunks, a definitive classification of truncation vs. genuine empty result.
- **SC-002**: A capture whose edge calls exhaust budget reports a non-zero `truncated` count; today it reports `clean`.
- **SC-003**: Total failure-sidecar size for a 228-chunk run stays under 20MB in the pathological all-fail case.
- **SC-004**: No change to judged or strict F1 for any existing cassette — this is observability only.

## Assumptions

- Storing full bodies adds no sensitivity beyond what cassettes already hold: `episode_body` and the rendered prompts are already persisted in full.
- Observability lands first and separately from any change to the `max_tokens` policy or edge-exhaustion semantics.

## Out of Scope

- Changing edge-budget-exhaustion semantics (e.g. making it fatal, changing the retry/doubling policy) — explicitly deferred per Assumptions; this issue is observability only.
- Any change to the live production service's extraction behavior beyond the underlying telemetry/error data the sidecar draws from. The `<cassette>.failures.jsonl` sidecar is a capture/eval-tooling artifact tied to cassette recording (`RecordingExtractor`); production extraction outside a capture is unaffected, and no sidecar is created in replay mode.
- Any new UI or dashboard for browsing failure records — the sidecar is a JSONL file for direct inspection; report changes are limited to the existing eval report's per-candidate summary (FR-005).

## Source References

- `crates/core/src/cassette.rs:320` — `RecordingExtractor::extract`, where the `?` on the inner call currently bypasses `record()` on failure.
- `crates/core/src/extractor.rs:410`, `:1334` — the two "edge budget exhaustion is not fatal" `Ok(vec![])` returns (Anthropic and OAI-compatible paths respectively).
- `crates/core/src/telemetry.rs:58` — `TelemetryEvent::ExtractionTruncated` and `StructuredOutputParse` definitions.
- `crates/eval/src/runner.rs:145` — `CountingSink`, which currently matches only `StructuredOutputParse`.
- `docs/history/extraction-eval-2026-07.md` — the qwen3.6-35b-a3b edge-recall analysis this issue is blocking.
