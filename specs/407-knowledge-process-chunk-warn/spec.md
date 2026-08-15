# Feature Specification: `knowledge_process_chunk` — warn, document, and count oversized `chunk_text`

**Feature Branch**: `fabrik/issue-407`
**Created**: 2026-08-15
**Status**: Specified
**Input**: User description: "`knowledge_process_chunk` accepts any `chunk_text` with no size guard, no warning, and no chunking. Extraction quality collapses well before the model's context limit, and the call still returns `success` — so an integrator has no way to learn their input is too large short of noticing a degraded graph after the fact. This issue delivers visibility only: a structured warning, the documented contract, and a telemetry event. It deliberately does not change ingest behaviour."

## Background

`knowledge_process_chunk` sends the entirety of `chunk_text` through the extraction LLM in one call, with no upper bound on input size. Empirically, extraction quality degrades sharply as input grows — well before any context-window or output-token limit is reached.

The evidence comes from a real integrator's corpus (@totalslacker's WebBrain adapter, 4,374 pages, one `process_chunk` call per page):

```text
median  5,468 chars      p90  21,953      p99  70,783      max  593,272
>  8,000 chars:  1,589 pages (36.3%)
> 20,000 chars:    506 pages (11.6%)
> 50,000 chars:     91 pages (2.1%)
>100,000 chars:     28 pages (0.6%)
```

Entity recall is grossly sublinear in chunk length, while the edge-extraction pass keeps finding candidate facts across the whole document — so the gap between entities found and edges those entities could anchor widens with size, and unanchored edges are dropped:

| chars | entities | edges | edges dropped |
|---:|---:|---:|---:|
| 4,797 | 10 | 9 | 0 (0%) |
| 12,785 | 22 | 18 | 1 (5.6%) |
| 257,061 | 54 | 46 | 45 (97.8%) |

54x the text yields only 5x the entities. Nothing truncates and nothing errors — `stop_reason=tool_use` on both calls, well under the 8,192 output-token cap. This is **not** a context-window problem: 257,061 chars is 83,454 input tokens, comfortably inside the window, and raising `max_tokens` would not help. It is qualitative extraction degradation at a size the model can technically process without complaint.

Today, `knowledge_process_chunk` returns `"success": true` regardless of input size, so an integrator has no signal that their chunking strategy is producing a degraded graph short of noticing entity/edge counts look wrong after the fact — often long after ingestion.

This issue is a deliberately narrow, patch-level fix: make the problem **visible** (a structured warning on the oversized call, documented guidance, and an aggregable telemetry event), without changing what `knowledge_process_chunk` actually does with the input. It does not truncate, split, or reject oversized chunks.

### Relationship to prior issues

This re-scopes User Stories 1 and 2 of issue #282 for a patch release. #282 and #284 are being closed:

- #282's Specify/Research/Plan artifacts exist on branch `fabrik/issue-282`, which contains only `specs/282-.../spec.md` and no implementation. The advisory threshold, warning, and telemetry described in #284's background were specified there but never built — nothing is lost by closing it; the evidence above is carried forward from it.
- The remainder of that cluster — internal splitting of oversized chunks, `chunk_id` resubmission/idempotency semantics, per-chunk locking, the `Episodic.name` scan, and the foreign-episode delete — is being re-specified separately against a revised model of episode provenance and temporal validity, and is not a dependency of this issue.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Warned on an oversized chunk (Priority: P1)

An integrator calls `knowledge_process_chunk` with a chunk of text that is far larger than what the extraction pipeline handles well (e.g., an entire scraped web page, tens of thousands of characters). The call still succeeds and the graph is still updated exactly as it is today, but the response now names the problem: it says the chunk was too large, states its actual size, and states the recommended maximum — so the integrator can act on it (e.g., pre-split their input) instead of discovering degraded recall later by inspecting the graph.

**Why this priority**: This is the core of the issue. Without it, oversized ingestion remains silent and integrators keep shipping degraded graphs with no feedback loop.

**Independent Test**: Call `knowledge_process_chunk` with a `chunk_text` well above the advisory threshold and inspect the JSON-RPC response for the warning, its stated actual size, and its stated recommended maximum.

**Acceptance Scenarios**:

1. **Given** a `chunk_text` whose character count exceeds the advisory threshold, **When** `knowledge_process_chunk` is called, **Then** the response includes a structured warning naming both the chunk's actual size (in characters) and the recommended maximum, and `"success": true` is still returned with the graph updated as it is today.
2. **Given** a `chunk_text` whose character count is at or below the advisory threshold, **When** `knowledge_process_chunk` is called, **Then** the response contains no warning, and is otherwise identical in shape to today's response (no new required fields, no removed fields).
3. **Given** the advisory threshold is overridden via its environment variable, **When** a chunk is ingested whose size falls between the default and the overridden threshold, **Then** the warning fires or does not fire according to the overridden value, not the default.

---

### User Story 2 - Discoverable contract without reading source (Priority: P2)

An integrator building against this tool (e.g., via MCP `tools/list`, or by reading the project README) wants to know up front how large a chunk can be before quality degrades, and whose job it is to split larger input. Today that information does not exist anywhere an integrator would look before writing code.

**Why this priority**: Prevents the problem at the source — an integrator who knows the recommended maximum before they integrate is less likely to hit the warning at all. Depends on User Story 1 establishing what the threshold governs, but is independently valuable documentation.

**Independent Test**: Without reading any source file, read the `knowledge_process_chunk` tool description returned by `tools/list` (MCP) and/or the README's ingestion section, and confirm both state the recommended maximum chunk size, that it is measured in characters, and that splitting oversized input is the caller's responsibility.

**Acceptance Scenarios**:

1. **Given** an MCP client calls `tools/list`, **When** it reads the `knowledge_process_chunk` tool description, **Then** the description states the recommended maximum chunk size in characters and that chunking is the caller's responsibility.
2. **Given** a developer reads the README's ingestion section, **When** they look for guidance on chunk sizing, **Then** they find the recommended maximum and the caller-responsibility statement without needing to read `handlers.rs`.

---

### User Story 3 - Oversized ingestion visible in aggregate (Priority: P3)

An operator running the service wants to know, across all ingestion traffic, how often callers are sending oversized chunks — to gauge whether integrators are following the documented guidance, without manually correlating individual RPC responses.

**Why this priority**: Turns a per-call signal (the warning) into an operational one. Lower priority than Stories 1–2 because it's an operability nicety layered on top of a signal that already exists once Story 1 ships; the per-call warning is the load-bearing piece.

**Independent Test**: Ingest one oversized chunk and one normal-sized chunk, capture the telemetry stream (stderr JSONL per `docs/telemetry.md`), and confirm exactly one telemetry event was emitted for the oversized call and none for the normal one.

**Acceptance Scenarios**:

1. **Given** a chunk exceeding the advisory threshold is ingested, **When** the call completes, **Then** exactly one telemetry event documenting the oversized ingest is emitted to the telemetry stream, following the conventions in `docs/telemetry.md` (e.g., the existing `extraction_truncated` event's shape: a `type` discriminant, `ts_ms`, and event-specific fields).
2. **Given** a chunk within the advisory threshold is ingested, **When** the call completes, **Then** no oversized-chunk telemetry event is emitted for that call.

---

### Edge Cases

- A `chunk_text` exactly at the threshold value: MUST NOT warn (threshold is exceeded only when size is strictly greater than the configured value).
- The threshold environment variable is set to an invalid value (non-numeric, negative, zero): the service must not crash; it should fall back to the default and this fallback should be discoverable (consistent with how other `LCG_*` numeric overrides in this codebase already handle invalid values — see e.g. `LCG_WAL_MAX_EVENTS_PER_FILE` in `crates/core/src/app_state.rs`).
- A `chunk_text` that is oversized by the character-count definition but represents very few bytes in a non-UTF-8-dense encoding, or vice versa (many bytes, few characters, e.g. heavy multibyte content): the threshold and the warning both operate purely on character count, per FR-006 — byte size is irrelevant to whether the warning fires.
- Repeated calls with the same oversized `chunk_text` (e.g., a resubmission): each call is evaluated independently; the warning and telemetry event fire on every oversized call, not just the first.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_process_chunk` MUST include a structured warning in its result when `chunk_text`'s character count exceeds an advisory threshold. The warning MUST name both the chunk's actual size and the recommended maximum.
- **FR-002**: The advisory threshold MUST be configurable via an `LCG_*` environment variable, consistent with this project's existing environment-variable convention (see `crates/core/src/app_state.rs`, `token_budget.rs`, `replay.rs` for precedent). The default value is derived from the evidence in Background — 0% edge drop was observed at 4,797 characters, 5.6% drop was already present at 12,785 characters — with ~8,000 characters as the candidate default; the exact default is confirmed during the Research stage rather than guessed here.
- **FR-003**: The MCP tool description for `knowledge_process_chunk` (`crates/service/src/mcp/tools.rs`) and the README's ingestion section MUST state the recommended maximum chunk size and that splitting oversized input into multiple calls is the caller's responsibility.
- **FR-004**: Ingest behavior MUST remain accept-and-warn for oversized chunks: no rejection, no truncation, no internal splitting. The chunk is processed exactly as it is today regardless of size.
- **FR-005**: When the warning in FR-001 fires, the service MUST emit a telemetry event following the structured-event conventions documented in `docs/telemetry.md`, so oversized ingestion is visible in aggregate across a telemetry stream (not just in the single call's response).
- **FR-006**: The advisory threshold is measured in **characters**, not bytes. Both the response warning and the documentation (tool description, README, `docs/telemetry.md` entry) MUST state this unit explicitly.

### Key Entities

- **Advisory threshold**: A configurable character-count value above which `chunk_text` is considered oversized. Has a documented default (candidate ~8,000 characters, confirmed in Research) and is overridable via an `LCG_*` environment variable.
- **Oversized-chunk warning**: A structured element of `knowledge_process_chunk`'s JSON-RPC response, present only when the threshold is exceeded, naming the chunk's actual character count and the recommended maximum.
- **Oversized-chunk telemetry event**: A JSONL event emitted to the telemetry stream (per `docs/telemetry.md`) each time the warning fires, enabling aggregate visibility across many calls.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Ingesting a 257KB chunk (matching the Background evidence) returns a warning naming both the actual size and the recommended maximum.
- **SC-002**: Ingesting a 5KB chunk produces no warning, and the response is otherwise byte-compatible with today's shape (no new required fields, nothing removed).
- **SC-003**: An integrator reading only `tools/list`'s output for `knowledge_process_chunk` can determine the recommended maximum chunk size without reading source code.
- **SC-004**: The telemetry event defined by FR-005 fires exactly once per oversized `knowledge_process_chunk` call and is distinguishable in an aggregated telemetry stream (e.g., countable via a JSONL filter on its `type` discriminant).
- **SC-005**: The number of episodes created for any given input is unchanged from today's behavior, for both oversized and normal-sized chunks.

## Assumptions

- The advisory threshold's exact default value is a research/calibration question, not a product decision — the Research stage confirms the number against the evidence in Background; this spec fixes only the requirement that a default exists and is overridable.
- "Structured warning" means the response gains warning information in a form a caller can programmatically detect (as opposed to, say, only a human-readable log line) — the exact field name/shape is a Plan-stage/implementation decision, not fixed here, so long as SC-002's byte-compatibility requirement for non-oversized calls holds.
- The telemetry event's exact name and field set are likewise implementation decisions to be made consistent with `docs/telemetry.md`'s existing conventions (e.g., the `extraction_truncated` event is the closest existing precedent: a hot-path-adjacent event fired under a specific triggering condition, not on every call).
- This issue does not change `chunk_id` idempotency/resubmission semantics, per-chunk locking, or episode provenance — those remain exactly as they behave today and are being re-specified separately (see *Relationship to prior issues*).

## Out of Scope

- Internal splitting of oversized `chunk_text` into multiple chunks/episodes. That would change episode cardinality (one chunk becoming N episodes), which is an observable behavior change and therefore not patch-level. Deferred to a future release (0.14.0 per the issue).
- Any `chunk_id` resubmission / idempotency semantics.
- An IPC-layer request size cap (e.g., rejecting connections or requests above a hard byte limit).
- Any change to entity/edge extraction logic itself — this issue adds visibility only; it does not attempt to fix the underlying sublinear-recall behavior.

## Source References

- `crates/core/src/handlers.rs:635-704` — `handle_knowledge_process_chunk`, no size guard today.
- `crates/service/src/mcp/tools.rs` — the `knowledge_process_chunk` `ToolSpec`, around line 285.
- `docs/telemetry.md` — structured-event conventions (cf. `extraction_truncated`).
- `README.md` — the "Ingestion" paragraph describing `knowledge_process_chunk`.
- #282 (closed) — original report and evidence for this issue.
- #284 (closed) — background on telemetry/threshold design that informed FR-002 and FR-005.
