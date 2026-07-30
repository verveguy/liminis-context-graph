# Feature Specification: `knowledge_process_chunk`: internal splitting for oversized chunk_text (follow-up to #282)

**Feature Branch**: `fabrik/issue-284`
**Created**: 2026-07-29
**Status**: Draft
**Input**: User description: "Design and implement a bounded degradation path for `chunk_text` far above the advisory threshold introduced in #282 — either internal splitting into threshold-sized units sharing one `chunk_id`, or an actionable rejection — with defined idempotency semantics for repeated ingestion of the same `chunk_id`."

## Background

`knowledge_process_chunk` is the graph engine's documented ingestion entry point. #282 ("`knowledge_process_chunk` accepts unbounded input and silently degrades") established that entity recall collapses sublinearly as `chunk_text` grows while edge extraction keeps finding facts across the whole document, so a single oversized call silently succeeds at a small fraction of achievable extraction yield. #282 implemented its User Stories 1 and 2: a structured `warning` in the response when `chunk_text` exceeds a configurable advisory threshold (default 8,000 characters, `LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS`), documentation of the size contract in `crates/service/src/mcp/tools.rs` and `README.md`, and a `chunk_text_oversized` telemetry event. #282's own FR-004 requires that default behavior remain accept-and-warn, never reject — so the warning alone already makes the problem *visible*, but does nothing to bound the *damage* for a chunk far above the threshold.

This issue is #282's deferred User Story 3 and FR-006:

> **User Story 3 - Oversized input degrades predictably rather than catastrophically (Priority: P2)**
> For a `chunk_text` far above the threshold, the system's behavior should be a deliberate, bounded degradation path (either internal splitting into threshold-sized units, or an actionable rejection) rather than silent acceptance at a small fraction of achievable extraction yield.
>
> **FR-006**: If internal splitting is implemented, all resulting units MUST share the caller's `chunk_id` so re-ingest idempotency is preserved.

It was split out of #282 because FR-006 assumes `chunk_id`-based idempotency already exists as a baseline — it doesn't. `crates/core/tests/ipc_parity.rs::test_knowledge_process_chunk_duplicate_chunk_id` currently *asserts* that resubmitting the same `chunk_id` produces two distinct `episode_uuid`s (no dedup). Delivering FR-006 for real therefore requires first deciding what `chunk_id`-based idempotency means — for a chunk split into multiple units *and* for the existing non-split single-episode case (since #282 chose warn-only and left that case's behavior untouched) — and only then building it, with WAL and episode-accounting implications. That is materially larger than #282's warning + docs + telemetry scope, which is why it was deferred to this issue.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Oversized input degrades predictably rather than catastrophically (Priority: P1)

An integrator (e.g. a third-party adapter calling `knowledge_process_chunk` directly on whole document pages) submits a `chunk_text` far above the advisory threshold. Today the call silently succeeds while returning a warning, but extraction quality is a small fraction of what's achievable at the threshold size. Instead, the system MUST take a deliberate, bounded path: either split the oversized text internally into threshold-sized units (all tagged with the caller's `chunk_id`), or reject the call with an actionable error explaining why and what to do instead.

**Why this priority**: This is the entire scope of this issue — without it, oversized input still only gets a warning (#282's baseline), never a bounded outcome.

**Independent Test**: Call `knowledge_process_chunk` with a `chunk_text` far above the threshold (e.g. the 257KB example from #282's evidence) and confirm the outcome is either (a) multiple episodes, all carrying the caller's `chunk_id`, each within the threshold size, or (b) a rejection response naming the actual size, the threshold, and a corrective action — never a single low-yield episode accepted silently.

**Acceptance Scenarios**:

1. **Given** a `chunk_text` far above the advisory threshold, **When** `knowledge_process_chunk` is called, **Then** the result is either multiple episodes sharing the caller's `chunk_id` (each at or below the threshold), or an actionable rejection error — not a single accepted episode built from the full oversized text.
2. **Given** a `chunk_text` at or below the advisory threshold, **When** `knowledge_process_chunk` is called, **Then** behavior is unchanged from #282's baseline (single episode, warning only if applicable).
3. **Given** internal splitting is the chosen path and a unit boundary would fall inside a word, **When** the split is computed, **Then** the split prefers the nearest whitespace boundary over a hard mid-word cut, falling back to a hard cut only when a single unit contains no whitespace to split on (e.g. one long unbreakable token).

---

### User Story 2 - Resubmitting a chunk_id has deterministic, documented behavior (Priority: P1)

A caller resubmits a `chunk_id` it has submitted before — either as a safe retry (identical `chunk_text`) or because the upstream source changed (different `chunk_text` under the same `chunk_id`). Today this silently produces a second, unrelated episode with no relationship to the first (`test_knowledge_process_chunk_duplicate_chunk_id`). The system MUST instead have one deliberate, documented behavior for resubmission, and that behavior MUST apply the same way whether the chunk was split into multiple units or ingested as a single episode.

**Why this priority**: This is what makes FR-006's "so re-ingest idempotency is preserved" meaningful. Splitting into per-`chunk_id` units without also fixing resubmission semantics would leave the exact gap #282's Research flagged: a `chunk_id` that maps to N episodes with no defined behavior when the caller sends it again.

**Independent Test**: Submit the same `chunk_id` twice with identical `chunk_text` and confirm the documented outcome (e.g. a no-op second call, or a deterministic replace) instead of two unrelated episodes; then submit the same `chunk_id` with different `chunk_text` and confirm the documented outcome for that case too.

**Acceptance Scenarios**:

1. **Given** a `chunk_id` previously ingested, **When** the same `chunk_id` and identical `chunk_text` are submitted again, **Then** the result follows one documented, deterministic behavior (not the creation of a second, unrelated episode).
2. **Given** a `chunk_id` previously ingested, **When** the same `chunk_id` is submitted again with different `chunk_text`, **Then** the result follows one documented, deterministic behavior, and `knowledge_delete_chunk_episode` for that `chunk_id` continues to account for exactly the episode(s) currently associated with it (no orphaned episodes left unreachable by chunk-level deletion).
3. **Given** a `chunk_text` was split into multiple units under a `chunk_id`, **When** that `chunk_id` is resubmitted, **Then** the same idempotency behavior from Scenarios 1–2 applies across the whole set of units, not just within a single unit.

---

### Edge Cases

- A single unit that is one long unbreakable token (no whitespace to split on), if internal splitting is implemented — still carried over from #282's spec.
- This change alters episode counts for a given chunk (one episode becomes N for oversized input), which some existing tests assert against (e.g. tests that assume one `chunk_id` call → one `episode_uuid`) — any such change MUST be called out explicitly as a behavior change, per #282's own edge case note.
- `test_knowledge_process_chunk_duplicate_chunk_id` currently asserts the pre-idempotency baseline (`assert_ne!` on the two `episode_uuid`s) — this test's assertion must be reconciled with or explicitly superseded by whatever idempotency semantic is adopted.
- A multi-unit split where an intermediate unit fails (e.g. an extraction or embedding call errors partway through) — the resulting partial state (some units committed, some not) under the same `chunk_id` must have defined behavior, not be left ambiguous.
- `knowledge_delete_chunk_episode` already deletes by `chunk_id` via `remove_episodes_by_chunk_id`, which returns a list of UUIDs — i.e. the deletion path already tolerates a `chunk_id` mapping to more than one episode. Splitting must remain consistent with this existing multi-episode-per-`chunk_id` deletion behavior, not introduce a second, incompatible notion of "the episodes for this chunk."

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When `chunk_text` is at or below the #282 advisory threshold (`LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS`), behavior MUST be unchanged from #282's baseline.
- **FR-002**: When `chunk_text` exceeds the advisory threshold, the system MUST take one deliberate, bounded degradation path instead of #282's warn-and-accept-as-is behavior: either (a) split `chunk_text` into threshold-sized units and ingest each as a separate episode, or (b) reject the call with an actionable error. This issue does not introduce a second, higher threshold distinct from #282's — the same single threshold governs both the warning (#282) and whichever path is chosen here.
- **FR-003**: If option (a) (splitting) is implemented, every resulting episode MUST share the caller-supplied `chunk_id` (carrying forward #282's FR-006), remain independently identifiable for observability/debugging (e.g. a unit index distinct from the shared `chunk_id`), and `knowledge_delete_chunk_episode` MUST continue to delete every episode sharing that `chunk_id`.
- **FR-004**: If option (a) is implemented, splitting MUST prefer natural boundaries (e.g. whitespace) over hard character cuts, falling back to a hard cut only when a unit contains no such boundary (the unbreakable-token edge case).
- **FR-005**: If option (b) (rejection) is implemented, it MUST be opt-in, not the default — #282's FR-004 requires default behavior to remain accept-and-warn — and the rejection error MUST name the actual `chunk_text` size, the advisory threshold, and a corrective caller action.
- **FR-006**: Resubmitting the same `chunk_id` MUST produce one documented, deterministic behavior, covering both: (i) `chunk_text` unchanged from the prior ingest (safe retry), and (ii) `chunk_text` different from the prior ingest (upstream edit) — replacing the current baseline where duplicate `chunk_id` silently produces unrelated episodes.
- **FR-007**: The idempotency behavior in FR-006 MUST apply uniformly whether the chunk_text produces a single episode (at/below threshold) or multiple split units (above threshold) — the single-episode case must not remain non-idempotent while only the split case gains idempotency.
- **FR-008**: The choice between option (a) and option (b) in FR-002, and the specific idempotency semantic in FR-006 (e.g. replace-prior-episode(s), reject-as-duplicate, versioned accumulation), are Research/Plan-stage decisions. Research MUST evaluate both against `episode::add_episode`'s existing dedup/WAL model (`crates/core/src/episode.rs`) and recommend one; Plan MUST document the chosen approach given its WAL and episode-accounting implications.
- **FR-009**: Any change to episode-count-per-chunk behavior, and any change to `test_knowledge_process_chunk_duplicate_chunk_id`'s current assertion, MUST be explicitly called out in the Plan/Implement stage output as an intentional behavior change, not silently altered.

### Key Entities

- **Split unit**: A threshold-sized portion of an oversized `chunk_text`, one of potentially several ingested as separate episodes that all share the caller-supplied `chunk_id`, if option (a) is chosen.
- **Idempotency semantic**: The documented, deterministic rule governing what happens when a `chunk_id` already associated with one or more episodes is submitted again, for both unchanged and changed `chunk_text`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Ingesting a `chunk_text` far above the threshold (e.g. #282's 257KB evidence example) results in either N episodes all tagged with the caller's `chunk_id`, or a single rejection response naming the size, threshold, and corrective action — never a single low-yield episode silently accepted.
- **SC-002**: Resubmitting an identical `chunk_id` + `chunk_text` pair twice produces a documented, deterministic outcome (e.g. the second call no-ops or deterministically replaces) rather than two unrelated episodes.
- **SC-003**: Resubmitting the same `chunk_id` with different `chunk_text` produces a documented, deterministic outcome consistent with the chosen idempotency semantic, and `knowledge_delete_chunk_episode` for that `chunk_id` accounts for exactly the episode(s) currently associated with it.
- **SC-004**: `knowledge_delete_chunk_episode` continues to delete every episode associated with a `chunk_id`, including all units of a split chunk.
- **SC-005**: Every existing test whose assertions depended on the prior one-chunk-to-one-episode or non-idempotent-resubmission behavior is updated, with the change called out explicitly (not silently adjusted).

## Assumptions

- The single advisory threshold introduced by #282 (`LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS`, default 8,000 chars) is also the trigger for whichever bounded degradation path is chosen here; this issue does not introduce a second, higher threshold.
- This issue depends on #282's threshold/config mechanism and warning path existing in the codebase. #282 has not yet merged to `main` as of this spec; if it is still unmerged when this issue reaches Research/Plan, that prerequisite work must be available (via merge or rebase) before implementation proceeds.
- The decision between option (a) internal splitting and option (b) actionable rejection, and the specific idempotency semantic, are deliberately left open by this spec per FR-008 — Research and Plan own resolving them, consistent with this project's stage boundaries (Specify defines *what*; Research/Plan decide *how*).
- `group_id`, `source_file`, and `reference_time` are shared across all split units of a single oversized `chunk_text` (no per-unit override) if option (a) is chosen.

## Out of Scope

- Re-litigating the advisory threshold value or the warning/telemetry mechanism from #282 — those are settled.
- The endpoint-resolution defect in the companion issue #202/#281 — unrelated code path.
- Introducing a second, higher threshold distinct from the one #282 introduced.
- Per-call caller opt-in/opt-out of which degradation path applies, beyond what FR-005 requires if option (b) is chosen (i.e. rejection itself must be opt-in relative to the default, but this issue does not require a caller-selectable choice between splitting and rejection).

## Source References

- `crates/core/src/handlers.rs::handle_knowledge_process_chunk` — the handler, with #282's advisory-threshold check and `warning` response field.
- `crates/core/src/episode.rs::add_episode` — dedup/resolution/WAL-append/DB-write; where multi-unit splitting and any new idempotency semantics would live.
- `crates/core/src/handlers.rs::handle_delete_chunk_episode` and `crates/core/src/db.rs::remove_episodes_by_chunk_id` — deletion already returns a list of UUIDs per `chunk_id`, i.e. it already tolerates a `chunk_id` mapping to multiple episodes.
- `crates/core/tests/ipc_parity.rs::test_knowledge_process_chunk_duplicate_chunk_id` — documents the current non-idempotent baseline; must be reconciled with or explicitly superseded.
- `specs/282-knowledge-process-chunk-accepts/spec.md` (on the unmerged `fabrik/issue-282` branch) — parent spec with the extraction-quality evidence table and FR-001–FR-006.
- `docs/telemetry.md` — existing structured-event conventions, relevant if the chosen degradation path adds new telemetry.
