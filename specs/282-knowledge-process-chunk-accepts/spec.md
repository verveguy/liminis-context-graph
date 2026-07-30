# Feature Specification: `knowledge_process_chunk` accepts unbounded input and silently degrades

**Feature Branch**: `fabrik/issue-282`
**Created**: 2026-07-29
**Status**: Draft
**Input**: User description: "`knowledge_process_chunk` accepts whatever `chunk_text` it is given with no size guard, no warning, and no chunking. A third-party integrator (WebBrain adapter) calling it directly on whole document pages sees entity recall collapse sublinearly with document length while edge extraction keeps finding facts across the whole document — nothing truncates and nothing errors, so the call reports success at a fraction of the achievable extraction quality."

## Background

Chunking lives in the liminis Electron app (TypeScript); the graph engine's documented ingestion entry point is `knowledge_process_chunk`, which accepts whatever `chunk_text` it is given (see `crates/core/src/handlers.rs::handle_knowledge_process_chunk` and the tool description in `crates/service/src/mcp/tools.rs`). The liminis app already chunks below any plausible threshold before calling it, so this defect was invisible until a third-party integrator — @totalslacker's WebBrain adapter — called the entry point directly with unchunked whole-document text and got no chunking, no size guard, and no warning. Extraction quality collapses with document length and nothing about the call's success response says so.

This was surfaced while diagnosing #202. #202 covers a separate defect (an endpoint-resolution mechanism) and is out of scope here; this issue is only about the fact that a single extraction call has a practical quality ceiling far below its context ceiling, and that ceiling is currently invisible to callers.

### Evidence

@totalslacker's corpus export, 4,374 pages, one `process_chunk` call per page:

```
median  5,468 chars      p90  21,953      p99  70,783      max  593,272
>  8,000 chars:  1,589 pages (36.3%)
> 20,000 chars:    506 pages (11.6%)
> 50,000 chars:     91 pages (2.1%)
>100,000 chars:     28 pages (0.6%)
```

Entity recall is grossly sublinear in length, while the edge pass keeps finding facts across the whole document. The widening gap between the two is what destroys the graph:

| chars | entities | edges | edges dropped |
|---:|---:|---:|---:|
| 4,797 | 10 | 9 | 0 (0%) |
| 12,785 | 22 | 18 | 1 (5.6%) |
| 257,061 | 54 | 46 | 45 (97.8%) |

54× the text yields 5× the entities. Nothing truncates and nothing errors — `stop_reason=tool_use` on both calls, well under the 8,192 output cap. The call returns success.

This is **not** a context-window problem: 257,061 chars is 83,454 input tokens, comfortably inside the window. Raising `max_tokens` would not help. The failure mode is qualitative extraction degradation at a size the model can technically still process end-to-end.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - An integrator finds out their input is too large (Priority: P1)

A developer integrating against the MCP/socket API sends whole documents to `knowledge_process_chunk`. Today they get no signal that anything is wrong — the call succeeds, entities and edges come back, and there is no indication that most of the document's facts were missed. They need to learn, from the system itself, that their input exceeds the size at which extraction is reliable.

**Why this priority**: This is the core defect — a silent quality collapse with no observable signal. Without this, integrators have no way to discover the problem short of noticing degraded graph quality after the fact, as happened with the WebBrain adapter.

**Independent Test**: Call `knowledge_process_chunk` with a `chunk_text` above and below the advisory threshold and inspect the result payload for the warning field.

**Acceptance Scenarios**:

1. **Given** a `chunk_text` above the advisory threshold, **When** `knowledge_process_chunk` is called, **Then** the result includes a structured warning naming the actual chunk size and the recommended maximum.
2. **Given** a `chunk_text` at or below the threshold, **When** `knowledge_process_chunk` is called, **Then** no warning field is present and the rest of the result is byte-compatible with today's response shape.

---

### User Story 2 - The size contract is documented where integrators look (Priority: P1)

An integrator reading the MCP tool catalog or the README should be able to learn the recommended maximum `chunk_text` size and that chunking is their responsibility, without needing to read the Rust source or trigger the warning experimentally first.

**Why this priority**: Equal priority to User Story 1 — a warning that only appears after the fact is reactive. Integrators building against this API for the first time (as WebBrain did) should be able to learn the contract from documentation before they hit the problem.

**Independent Test**: Read the `tools/list` output for `knowledge_process_chunk` and the README's ingestion section; confirm both state the recommended maximum size without needing to read source.

**Acceptance Scenarios**:

1. **Given** the MCP `tools/list` output, **When** an integrator reads the `knowledge_process_chunk` description, **Then** it states the recommended maximum `chunk_text` size and that the caller owns chunking.
2. **Given** the README's ingestion section, **When** an integrator reads it, **Then** the same contract is stated with the measured rationale (i.e., referencing that extraction quality degrades well before the size the model can technically process).

---

### User Story 3 - Oversized input degrades predictably rather than catastrophically (Priority: P2)

For a `chunk_text` far above the threshold, the system's behavior should be a deliberate, bounded degradation path (either internal splitting into threshold-sized units, or an actionable rejection) rather than silent acceptance at a small fraction of achievable extraction yield.

**Why this priority**: Lower priority than the warning and documentation, because FR-004 already requires default behavior to remain accept-and-warn (not reject) to avoid breaking existing callers such as the liminis app. This story addresses what an *optional*, stronger mitigation could look like, but the warning alone (User Story 1) already satisfies the primary need: making the problem visible.

**Independent Test**: Ingest a `chunk_text` far above the threshold and confirm the outcome is either (a) multiple episodes sharing one `chunk_id` from internal splitting, or (b) a clear rejection error — not a silent, low-yield success.

**Acceptance Scenarios**:

1. **Given** a `chunk_text` far above the threshold, **When** it is ingested, **Then** either it is split internally into threshold-sized units sharing one `chunk_id`, or it is rejected with an actionable error — not silently accepted at a small fraction of achievable edge yield.

### Edge Cases

- Multi-byte text: the threshold is measured in **characters**, not bytes (the docs must state which, to avoid ambiguity for non-ASCII content).
- A single chunk that is one long unbreakable token (e.g., no whitespace to split on), if internal splitting is implemented.
- Callers that treat any non-empty warning field as an error condition — this is why FR-004 requires the default behavior to remain accept-and-warn, never reject, so existing callers (including the liminis app) are not broken by this change.
- If internal splitting (User Story 3) is implemented, it changes episode counts for a given chunk, which some existing tests assert against — any such change must be called out explicitly as a behavior change.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_process_chunk` MUST emit a structured warning in its result when `chunk_text` exceeds a configurable advisory threshold. The default threshold MUST be set from measurement (the evidence above shows 0% edge drop at 4,797 chars and 5.6% edge drop already present at 12,785 chars; ~8,000 chars is the candidate default, to be confirmed against the companion issue's fixtures during research) — not guessed.
- **FR-002**: The advisory threshold MUST be configurable via an environment variable, consistent with the existing `LCG_*` convention (see README's environment variable table).
- **FR-003**: The MCP tool description for `knowledge_process_chunk` (`crates/service/src/mcp/tools.rs`) and the README's ingestion section MUST state the size contract (recommended maximum `chunk_text` size, and that chunking is the caller's responsibility).
- **FR-004**: Default behavior MUST remain accept-and-warn, not reject — rejecting by default would break existing callers, including the liminis app.
- **FR-005**: The warning MUST be counted in telemetry (see `docs/telemetry.md`'s existing event conventions, e.g. `extraction_truncated`) so oversized ingest is visible in aggregate, not just per-call.
- **FR-006**: If internal splitting is implemented (User Story 3), all resulting units MUST share the caller's `chunk_id` so re-ingest idempotency is preserved.

### Key Entities

- **Advisory threshold**: A configurable character-count value (default derived from measurement, overridable via an `LCG_*` environment variable) above which `chunk_text` is considered likely to degrade extraction quality.
- **Structured warning**: A field added to the `knowledge_process_chunk` result, present only when the threshold is exceeded, naming the actual `chunk_text` size and the recommended maximum.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Ingesting a 257KB chunk produces a warning naming both the actual and recommended size.
- **SC-002**: Ingesting a 5KB chunk produces no warning, and the rest of the result is identical to today's response shape.
- **SC-003**: An integrator reading only `tools/list` can determine the recommended maximum `chunk_text` size without reading source.
- **SC-004**: Replaying the 4,374-page corpus reports the count of oversized chunks (expected ~1,589 above 8KB, per the evidence above).

## Assumptions

- The advisory default is set from measurement in the companion issue's fixtures, not guessed — the evidence table in this spec (0% edge drop at 4,797 chars, 5.6% already present at 12,785 chars) is the basis for the ~8,000-char candidate; the exact figure should be confirmed against those fixtures during research.
- The liminis Electron app already chunks text below any plausible threshold before calling `knowledge_process_chunk`, so it sees no behavior change from this work.
- The threshold is measured in characters, not bytes, consistent with how `chunk_text` is measured elsewhere in this spec's evidence.

## Out of Scope

- The endpoint-resolution defect itself (companion issue #202) — this issue is only about the size contract and its visibility, not the underlying mechanism that makes the entity/edge gap worse as size grows.

## Source References

- `crates/core/src/handlers.rs::handle_knowledge_process_chunk` — current handler, no size guard today.
- `crates/service/src/mcp/tools.rs` — `knowledge_process_chunk` `ToolSpec` entry (description, input schema, `write` scope).
- `README.md` — ingestion section (`## Ingestion` narrative) and the `LCG_*` environment variable table.
- `docs/telemetry.md` — existing structured-event conventions (e.g. `extraction_truncated`) that a new oversized-chunk telemetry event should follow.
- Issue #202 — companion issue covering the endpoint-resolution mechanism that this issue explicitly does not address.
