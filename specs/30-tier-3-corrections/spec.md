# Feature Specification: Tier 3 — Corrections Workflow (apply_corrections, validate_corrections, reprocess_entity_types)

**Feature Branch**: `fabrik/issue-30`
**Created**: 2026-05-22
**Status**: Draft
**Input**: User description: "Tier 3 of the liminis-graph ↔ liminis integration. Three WRITE methods that let users post-hoc fix the graph: knowledge_apply_corrections, knowledge_validate_corrections, and knowledge_reprocess_entity_types."

## Background

This is Tier 3 of the staged Rust reimplementation of the Python `graphiti_service.py`. It adds three correction-related JSON-RPC methods to the liminis-graph daemon that power the corrections UI in liminis-app:

- **`knowledge_validate_corrections`** — schema-validates the workspace corrections file, checks referenced entities/edges exist in the live graph, and detects circular alias chains. Called by the corrections UI before showing the Apply button.
- **`knowledge_apply_corrections`** — reads `.liminis/knowledge-corrections.yaml`, applies un-applied corrections (entity merging via `same_as`, fact invalidation via `retract`). Supports `dry_run`. Has 5 call sites in liminis-app.
- **`knowledge_reprocess_entity_types`** — for entities with only the generic `Entity` label, retroactively classifies them into specific types via LLM. Has 2 call sites in liminis-app.

These are the most feature-rich methods in the integration surface. They have the lowest cutover priority (the daily ingestion + browsing flow doesn't need them), but liminis-app does call them — without these, the corrections UI is dead.

**Blocked by**: Tier 1a (#26) and Tier 1b (#27) — corrections need the inventory/lookup methods to validate entity/edge references against the live graph.

Python implementations (canonical behaviour reference):
- `apply_corrections`: `graphiti_service.py:3616-3880` (includes helpers `_apply_same_as_correction`, `_apply_retract_correction`, `_validate_*_correction`)
- `validate_corrections`: `graphiti_service.py:3881-4080`
- `reprocess_entity_types`: `graphiti_service.py:2541-2570`

## User Scenarios & Testing *(mandatory)*

### User Story 1 — User validates corrections file before applying (Priority: P1)

The corrections UI in liminis-app calls `knowledge_validate_corrections` before showing the user the "Apply" button. Without this, the user has no way to know whether the corrections file (which they may have hand-edited) is structurally valid or refers to entities that no longer exist.

**Why this priority**: Load-bearing gate for the corrections UI. If validation is unavailable, the Apply button cannot be shown safely and the entire corrections workflow is blocked for users.

**Independent Test**: Author a `.liminis/knowledge-corrections.yaml` with one valid `same_as` and one invalid one (referring to a non-existent canonical entity). Call `knowledge_validate_corrections`. Assert the response reports `valid: false`, lists the invalid correction's id in `issues`, and counts `total_corrections: 2` and `unapplied_corrections: 2`.

**Acceptance Scenarios**:

1. **Given** a corrections file with N total entries (some applied, some not), **When** client sends `knowledge_validate_corrections` with empty params, **Then** response is `{valid: bool, message: string, total_corrections: N, unapplied_corrections: <int>, issues: [...], warnings: [...]}`.
2. **Given** no corrections file exists at `.liminis/knowledge-corrections.yaml`, **When** called, **Then** `{valid: true, message: "No corrections file found", total_corrections: 0, unapplied_corrections: 0, issues: [], warnings: []}` — not an error.
3. **Given** a `same_as` entry whose `canonical_uuid` does not exist in the graph, **When** validated, **Then** `valid: false`, the entry's id is in `issues`.
4. **Given** an alias chain that forms a cycle (A→B, B→C, C→A), **When** validated, **Then** `valid: false`, the cycle is named in `issues`.
5. **Given** a `same_as` correction missing both `canonical` and `canonical_uuid`, **When** validated, **Then** `valid: false`, the entry's id is in `issues`.

---

### User Story 2 — User applies pending corrections (Priority: P1)

After validation passes, the user clicks Apply. liminis-app calls `knowledge_apply_corrections`. Each correction is processed in order; already-applied corrections are skipped via the `applied_at` timestamp.

**Why this priority**: Direct enabler of the corrections UI's primary action. Without this, no correction the user authors is ever applied to the graph.

**Independent Test**: Author a corrections file with 3 corrections (one already has `applied_at`, two don't). Call `knowledge_apply_corrections`. Assert `applied: 2`, `skipped: 1`, the two newly-applied corrections gained `applied_at` timestamps in the file, and the graph reflects the changes.

**Acceptance Scenarios**:

1. **Given** a corrections file with `same_as` and `retract` entries that haven't been applied, **When** client sends `knowledge_apply_corrections` with no `dry_run`, **Then** each correction is applied: `same_as` merges alias entities into the canonical; `retract` invalidates the named edge/fact. Response: `{success: true, applied: N, skipped: M, errors: [], details: [...]}`. Each applied correction gains an `applied_at` ISO timestamp in the corrections file.
2. **Given** a correction with `applied_at` already set, **When** apply runs, **Then** that correction is counted in `skipped`, not re-applied.
3. **Given** `dry_run: true`, **When** apply runs, **Then** corrections are validated (same checks as Story 1) but NOT applied; the corrections file is NOT modified.
4. **Given** a correction whose `type` is unknown (not `same_as` or `retract`), **When** apply runs, **Then** that correction is counted in `errors`, processing continues for the rest.
5. **Given** an empty corrections list, **When** apply runs, **Then** `{success: true, message: "Corrections file is empty", applied: 0, skipped: 0, errors: []}`.
6. **Given** no corrections file exists, **When** apply runs, **Then** `{success: true, message: "No corrections file found", applied: 0, skipped: 0, errors: []}` — not an error.

---

### User Story 3 — User reclassifies generically-typed entities (Priority: P2)

Entities extracted before the freeform classification work still have only the generic `Entity` label. liminis-app's corrections UI exposes a "Reclassify" button that calls `knowledge_reprocess_entity_types` to LLM-classify them with specific labels.

**Why this priority**: P2 because the daily ingestion/browsing flow doesn't require reclassification — the corrections UI feature is non-blocking for primary workflows, but it must exist for the UI button to function.

**Independent Test**: Seed the graph with 5 entities labelled only `Entity` and 3 with specific labels. Call `knowledge_reprocess_entity_types`. Assert response reports `reclassified_count: 5` (or fewer if some couldn't be classified) and the 3 already-specific entities are not touched.

**Acceptance Scenarios**:

1. **Given** entities in `group_id` (default `"liminis"`) that have only the generic `Entity` label, **When** client sends `knowledge_reprocess_entity_types` with optional `group_id`, **Then** the service runs LLM classification on those entities and assigns specific labels. Response: `{success: true, reclassified_count: N}`.
2. **Given** entities that already have specific labels, **When** reprocess runs, **Then** those entities are NOT re-classified.
3. **Given** no entities need reclassification, **When** called, **Then** `{success: true, reclassified_count: 0}`.
4. **Given** the LLM call fails, **When** reprocess runs, **Then** `{success: false, group_id, error: "Failed to reprocess entity types: ..."}`.

---

### Edge Cases

- Two `apply_corrections` calls in flight simultaneously → must serialize via writer lock; the second sees the `applied_at` timestamps written by the first and skips them.
- `apply_corrections` interrupted mid-way (process killed) → corrections applied so far have their `applied_at` set; remaining corrections are picked up on the next call. The corrections file must be written atomically per correction (write to temp + rename), not at the end.
- `apply_corrections` with a `same_as` whose canonical was deleted between validate and apply → the apply step re-checks existence and records the failure in `errors`, continues with the rest.
- `validate_corrections` on a malformed YAML (parse error) → `{valid: false, issues: ["YAML parse error: ..."]}` — does not crash.
- `validate_corrections` on a corrections file with entries that aren't dicts (e.g., a stray string) → that entry is reported in `issues`, others are still validated.
- `reprocess_entity_types` called when LLM extraction service is unavailable → returns a structured error, does not crash the daemon.
- `reprocess_entity_types` invoked on a workspace with millions of generically-labelled entities → must not OOM; should process in batches (Python's `reprocess_entity_types` handles this internally — verify and preserve batch behaviour).
- Corrections file lives in `.liminis/knowledge-corrections.yaml` — workspace-relative, not in the graph. If the workspace root isn't available to liminis-graph, the methods must error explicitly (not silently look in CWD or HOME).

## Requirements *(mandatory)*

### Functional Requirements

**Common**

- **FR-001**: All three methods are WRITE methods. `apply_corrections` and `reprocess_entity_types` MUST acquire the writer lock. `validate_corrections` is read-only and MUST NOT acquire the writer lock.
- **FR-002**: All three methods MUST return errors as JSON-RPC error objects, MUST NOT crash the daemon, and MUST NOT leave the corrections file in an inconsistent state (e.g., partial `applied_at` write).
- **FR-003**: All three methods locate the corrections file at `{workspace_root}/.liminis/knowledge-corrections.yaml`. The workspace root must be available at service init time; if it is not, calls MUST return an explicit structured error.

**`knowledge_validate_corrections`**

- **FR-004**: Accepts no required params.
- **FR-005**: Response: `{valid: bool, message: string, total_corrections: int, unapplied_corrections: int, issues: [string], warnings: [string]}`.
- **FR-006**: Missing corrections file → `{valid: true, message: "No corrections file found", total_corrections: 0, unapplied_corrections: 0, issues: [], warnings: []}`.
- **FR-007**: For `same_as` entries: validates presence of `canonical` OR `canonical_uuid`; validates `aliases` is non-empty; validates referenced UUIDs exist in the graph; detects circular alias chains across the full set of corrections (A→B→C→A).
- **FR-008**: For `retract` entries: validates presence of the field naming the edge/fact to invalidate; validates that the named edge/fact exists in the graph.
- **FR-009**: For entries that are not dicts or are missing `type`: each is reported in `issues`, validation continues for the remaining entries.
- **FR-010**: `valid: false` is set when any blocking issue is found. Warnings (e.g., entries with no `applied_at` that will be re-applied) do not flip `valid` to false.

**`knowledge_apply_corrections`**

- **FR-011**: Accepts `dry_run` (bool, default `false`).
- **FR-012**: Reads the corrections file; for each entry without `applied_at`, processes it based on `type`.
- **FR-013**: `same_as` (non-dry-run): merges alias entities into the canonical. The merge logic moves edges from aliases onto the canonical (de-duplicating) and marks alias entities as merged (NOT hard-deleted, to preserve provenance).
- **FR-014**: `retract` (non-dry-run): marks the named edge/fact as invalidated (graphiti's retract operation; sets an invalidation timestamp rather than hard-deleting).
- **FR-015**: `dry_run: true` validates each correction (same per-type checks as FR-007–FR-009) without applying. The corrections file MUST NOT be modified. Response shape is the same as a real run, with `applied: 0` and `skipped` counting already-applied entries.
- **FR-016**: On successful application of a correction, the entry's `applied_at` field is set to an ISO-8601 UTC timestamp and the corrections file is rewritten **atomically** (write to temp + rename) so a crash mid-write doesn't corrupt the file.
- **FR-017**: Errors processing one correction (unknown type, missing required field, target not found) MUST NOT abort the run. The error is captured in `errors[]` and processing continues with the remaining corrections.
- **FR-018**: Response: `{success: bool, applied: int, skipped: int, errors: [string], details: [object], message: string (optional)}`.

**`knowledge_reprocess_entity_types`**

- **FR-019**: Accepts `group_id` (string, optional, default `"liminis"`).
- **FR-020**: Identifies all entities in the named group whose only label is the generic `Entity`.
- **FR-021**: For each such entity, runs LLM classification (same path as extraction) and assigns the specific label(s) the model returns. Entities with any specific label already set MUST NOT be re-classified.
- **FR-022**: Response: `{success: true, reclassified_count: int}` on success. LLM-cost-snapshot fields are deferred per Tier 1a — omit if liminis-graph doesn't yet track cost.
- **FR-023**: On LLM failure: `{success: false, group_id: string, error: string}`.
- **FR-024**: Processing MUST be batched to avoid OOM on workspaces with very large numbers of generically-labelled entities. The batch size is an implementation choice and MUST be documented in code.

### Key Entities

- **Corrections file**: `.liminis/knowledge-corrections.yaml` — workspace-relative YAML file with top-level key `corrections: [{id, type, ...}]`. User-editable; may contain hand-edits and YAML comments between calls. The implementation MUST tolerate reformatting/comment additions.
- **`same_as` correction**: A correction entry with `type: same_as`, specifying a `canonical` name or `canonical_uuid`, and `aliases` (list of entity names/UUIDs to merge into the canonical).
- **`retract` correction**: A correction entry with `type: retract`, specifying the edge or fact to invalidate.
- **`applied_at`**: ISO-8601 UTC timestamp written to a correction entry after it is successfully applied. Presence of this field marks the correction as already-applied and causes it to be skipped on subsequent calls.
- **Entity merge (same_as)**: A semantic operation that moves all edges from alias entities onto the canonical entity (de-duplicating), then marks alias entities as merged. Hard-delete is NOT acceptable — provenance must be preserved.
- **Retraction**: A semantic invalidation operation that sets an invalidation timestamp on an edge or fact rather than hard-deleting it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Unmodified Python `reader_server.py` / `writer_server.py` can call all three methods against liminis-graph without code changes; responses parse correctly (IPC Parity).
- **SC-002**: For a corrections file with 10 valid + 2 invalid entries, `knowledge_validate_corrections` correctly identifies all 2 invalid entries with their ids in `issues` and `valid: false`.
- **SC-003**: For a corrections file with a 3-node alias cycle (A→B, B→C, C→A), `knowledge_validate_corrections` detects and names the cycle in `issues`.
- **SC-004**: For a `same_as` correction merging alias A into canonical C, after `knowledge_apply_corrections`: all of A's prior edges connect to C (or are de-duplicated against existing C edges); A is marked merged; the corrections file's entry gained an `applied_at` ISO timestamp.
- **SC-005**: A second `knowledge_apply_corrections` call on the same file does NOT re-apply any correction (all are counted as `skipped`).
- **SC-006**: `knowledge_apply_corrections` with `dry_run: true` leaves the corrections file byte-identical before and after the call (verified by hash).
- **SC-007**: `knowledge_reprocess_entity_types` on a seeded set of 10 generically-labelled entities reclassifies all 10 (assuming LLM is available); a follow-up `knowledge_list_entities` call shows all 10 with specific labels.
- **SC-008**: Concurrent `knowledge_apply_corrections` calls do not double-apply any correction — the second call sees `applied_at` from the first and skips.

## Assumptions

- The corrections file format (YAML schema with `corrections: [{id, type, ...}]`) is the canonical contract — defined by the existing Python implementation. Schema documentation lives in the corrections UI / framework docs; this spec defers to that.
- liminis-graph receives the workspace root at service init (per the audit, the Python service takes it as a CLI arg). If not, that's a Tier 1a integration concern, not Tier 3.
- LLM extraction for `knowledge_reprocess_entity_types` uses the same configured extractor as `add_episode` (Sonnet per `[[project-llm-routing]]`).
- The graphiti `reprocess_entity_types` utility used by Python is **not** directly portable to Rust — the spec describes the behaviour to replicate, not the code to translate. Implementation chooses how to bulk-classify (likely via batched extractor calls).
- "Merge" in `same_as` is semantic: edges move, alias is marked merged. Hard-delete is **not** acceptable — it loses provenance and breaks the audit trail.
- "Retract" in `retract` is semantic: invalidation timestamp, **not** hard-delete.
- The corrections file may have manual hand-edits between calls. The implementation MUST tolerate reformatting and YAML comment additions — the file is the canonical record, not an opaque database.

## Out of Scope

- A UI for editing corrections (lives in liminis-app).
- Undo: there is no `unapply_corrections` method. Reversing a correction requires hand-editing the file (remove `applied_at`) and re-running.
- Cross-workspace corrections: a corrections file applies to its workspace only.
- LLM cost tracking (deferred per Tier 1a).
- A correction type beyond `same_as` and `retract` (future spec when needed).
- Concurrent reprocess + ingest — Python serializes via the writer lock; this spec preserves that, no fancy interleaving.

## Source References

- `graphiti_service.py:3616-3880` — canonical Python `apply_corrections` implementation
- `graphiti_service.py:3881-4080` — canonical Python `validate_corrections` implementation
- `graphiti_service.py:2541-2570` — canonical Python `reprocess_entity_types` implementation
- Issue #26 — Tier 1a: establishes handler-dispatch pattern, JSON-RPC error shape, and writer lock
- Issue #27 — Tier 1b: inventory/lookup methods used by `validate_corrections` to check entity/edge existence
- `.specify/memory/constitution.md` — Principle I (IPC Parity)
