# Feature Specification: Streaming WAL-rebuild's build_ok doesn't reflect a failed lookup_key backfill

**Feature Branch**: `fabrik/issue-491`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "Streaming WAL-rebuild's build_ok doesn't reflect a failed lookup_key backfill"

## Background

Follow-up from issue #221 (PR #483, human review 2026-08-24) — deliberately deferred rather than fixed inline, per the reviewer's explicit instruction.

In both streaming WAL-rebuild arms in `crates/core/src/handlers.rs` (the streaming path, whose backfill call currently sits at line 2501, and the mirrored background-job path at line 2958), `build_ok` is set from `conn.build_indices_and_constraints()`'s result only:

```rust
let mut build_ok = false;
...
crate::schema::backfill_entity_lookup_keys_and_record_status(&conn);
match conn.build_indices_and_constraints() {
    Ok(()) => build_ok = true,
    Err(e) => { ... }
}
```

`build_ok` backs `state.indices_built`, which is surfaced to callers as `indices_built` on both the rebuild-job result JSON (`handlers.rs:2708` and `:3089`) and `knowledge_status`. If the `Entity.lookup_key` backfill fails but the subsequent `build_indices_and_constraints()` call succeeds, `build_ok` becomes `true` — so `indices_built: true` is reported even though this rebuild's `lookup_key` backfill did not complete. `backfill_entity_lookup_keys_and_record_status` correctly records the failure to `SchemaState` and the in-process flag that backs `knowledge_status`'s `name_index_trusted` field, but `indices_built` is a separate, also-consulted signal and it does not reflect this particular failure mode.

The original issue framed this as an open design call between two directions — fold the backfill outcome into `build_ok` ("rebuild fully healthy"), or keep `indices_built` scoped to indexing only and document the split explicitly — and deliberately left the choice unresolved, since it affects what `indices_built` is supposed to mean as a public signal.

**This spec resolves that choice in favor of keeping `indices_built` scoped to index-build success only**, for a reason beyond either option as originally stated: `state.indices_built` is not purely an informational field — it is also read by the search auto-heal path (`handlers.rs:1007`, `:1053`) to decide whether to trigger `build_indices_once()` on a missing-index search error. If a `lookup_key` backfill failure were folded into `indices_built`, a backfill-only failure would flip that flag to `false` and cause the auto-heal path to re-run `build_indices_and_constraints()` on the next search miss — an operation that cannot fix a backfill problem, since backfill and index-build are separate steps. That would make the auto-heal path spuriously re-trigger index rebuilds it has no way to know are pointless, without actually addressing the underlying lookup_key gap. Because this is dictated by `indices_built`'s existing functional role rather than being a free product choice, it is treated as settled here rather than deferred again to the Plan stage.

Resolving *that* question exposes a second, previously unnoticed gap: neither rebuild arm's own result payload (the RPC/job-status JSON emitted at `handlers.rs:2708` and `:3089`) includes `name_index_trusted` or any other backfill-outcome field at all — only `indices_built`. So even under the "keep them separate, consult `name_index_trusted` instead" direction, a caller driving a rebuild via its job result currently has **no way** to learn that this specific rebuild's backfill failed without a separate, later `knowledge_status` call. This spec's requirements close that gap as part of the fix, not just the documentation gap the original issue anticipated.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A rebuild's own result reports its backfill outcome (Priority: P1)

An operator or automated caller that triggers a WAL rebuild (streaming or background-job) needs to know, from that rebuild's own result, whether the `Entity.lookup_key` backfill for that rebuild succeeded — not just whether the FTS/HNSW indexes were rebuilt, and not by having to separately poll `knowledge_status` afterward.

**Why this priority**: This is the core defect: `indices_built: true` is currently indistinguishable from "everything about this rebuild succeeded," even when it isn't, and there is no other field in the same response to check.

**Independent Test**: Force a `lookup_key` backfill failure (e.g. via a fault-injection hook or a database state that makes the backfill's Cypher fail) while leaving `build_indices_and_constraints()` able to succeed, trigger a WAL rebuild through each call site, and inspect that rebuild's own result payload.

**Acceptance Scenarios**:

1. **Given** a WAL rebuild via the streaming path where the `lookup_key` backfill fails but index build succeeds, **When** the rebuild completes, **Then** the streaming result reports `indices_built: true` AND a distinct, unambiguous indicator that the backfill failed for this rebuild.
2. **Given** the same failure combination via the background-job path, **When** the job completes, **Then** the job-status result reports the same two pieces of information, with the same meaning as the streaming path's result.
3. **Given** a WAL rebuild where both the backfill and the index build succeed, **When** the rebuild completes, **Then** the result reports both signals as healthy (`indices_built: true` and the backfill indicator succeeded).
4. **Given** a WAL rebuild where the index build itself fails (independent of backfill outcome), **When** the rebuild completes, **Then** `indices_built: false` as today, and the backfill indicator reflects the backfill's own outcome regardless of the index-build failure.

---

### User Story 2 - The scoping decision is documented at the source, not just in this issue (Priority: P2)

A future reader of either rebuild arm must be able to tell, from the code itself, that `build_ok`/`indices_built` intentionally excludes backfill outcome and why — without needing to find and read issue #491.

**Why this priority**: The issue this spec addresses exists specifically because this scoping was previously undocumented and looked like an oversight. Fixing the visibility gap (User Story 1) without also documenting the scoping decision would reintroduce the same risk of a future "drive-by fix" that reopens this exact question.

**Independent Test**: Read `crates/core/src/handlers.rs` at both call sites without other context and confirm the comment states the scoping decision and points to where backfill outcome is actually surfaced.

**Acceptance Scenarios**:

1. **Given** the streaming call site's `build_ok` assignment, **When** a reader inspects it, **Then** an inline comment explains that `build_ok` deliberately excludes backfill outcome, why (the auto-heal coupling), and where backfill outcome is reported instead.
2. **Given** the background-job call site's `build_ok` assignment, **When** a reader inspects it, **Then** the same explanation is present, worded consistently with the streaming site's comment.

---

### Edge Cases

- **Both backfill and index build fail in the same rebuild**: the result must report both failures independently — the index-build failure must not be allowed to make the backfill failure indicator ambiguous, and vice versa.
- **Dry-run rebuilds**: neither backfill nor index build runs during a dry run. The rebuild result must omit or clearly mark the backfill indicator the same way it already omits `indices_built` for dry runs (`handlers.rs:2702-2706`), rather than reporting a stale value left over from a previous non-dry-run rebuild.
- **A rebuild following a previous rebuild's backfill failure**: `ensure_lookup_key_backfill`'s existing retry logic re-attempts the backfill on the next opportunity. The backfill indicator in a given rebuild's result must reflect *that* rebuild's own outcome, not a cached value from an earlier one.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Both streaming WAL-rebuild call sites (`crates/core/src/handlers.rs`, the arms at the `backfill_entity_lookup_keys_and_record_status` calls near line 2501 and its mirrored line 2958) MUST continue to compute `build_ok`/`state.indices_built` from `conn.build_indices_and_constraints()`'s outcome alone. A `lookup_key` backfill failure MUST NOT cause `indices_built` to become `false`.
- **FR-002**: Each WAL-rebuild operation's own result payload — the streaming path's RPC result (currently `handlers.rs:2708`) and the background-job path's job-status result (currently `handlers.rs:3089`) — MUST additionally report whether *that specific rebuild's* `Entity.lookup_key` backfill succeeded, as a distinct, independently-checkable value from `indices_built`. A caller MUST be able to determine this outcome entirely from the rebuild's own result, without a follow-up `knowledge_status` call.
- **FR-003**: `knowledge_status`'s existing `name_index_trusted` field MUST remain the authoritative global signal for `lookup_key` correctness. It MUST NOT be repurposed to mean "indices are built," and its existing computation (via `lookup_key_migrated()`) MUST NOT change as part of this fix.
- **FR-004**: Both call sites MUST carry an inline comment stating explicitly that `build_ok`/`indices_built` is scoped to index-build success only, independent of backfill outcome; explaining why (the search auto-heal path at `handlers.rs:1007`/`:1053` consults `state.indices_built` to decide whether to retry index building, and a backfill failure folded into that signal would cause spurious, ineffective retries); and pointing to where backfill outcome is actually reported (FR-002's field, and `name_index_trusted` for the global signal).
- **FR-005**: The streaming and background-job call sites MUST behave identically for the same combination of backfill/index-build outcomes — this fix MUST NOT introduce or leave behind any divergence between the two arms.
- **FR-006**: The existing `indices_built` wire field's name and meaning (index-build success only) MUST NOT change. This fix is additive — it introduces a new way to observe backfill outcome per FR-002; it does not remove, rename, or redefine any existing field.
- **FR-007**: Automated test coverage MUST exercise the specific scenario this issue reports — backfill fails, index build succeeds — for both call sites, asserting that the result reports `indices_built: true` while the FR-002 backfill indicator reports failure, so this combination cannot silently regress back into looking like full success.

### Key Entities

- **`build_ok` / `state.indices_built` / `indices_built` (wire field)**: Tracks only whether `conn.build_indices_and_constraints()` (FTS + HNSW index rebuild) succeeded for the most recent non-dry-run rebuild. Also consulted by the search auto-heal path to decide whether to retry index building on a missing-index search error.
- **`Entity.lookup_key` backfill outcome**: Tracks whether `backfill_entity_lookup_keys_and_record_status` successfully backfilled every `NULL` `lookup_key` row for a given rebuild. Persisted globally via `SchemaState` and the in-process flag backing `knowledge_status`'s `name_index_trusted`; after this fix, also reported per-rebuild via FR-002.
- **`name_index_trusted`**: `knowledge_status`'s existing field surfacing the current global backfill-health state, independent of any specific rebuild.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: For a rebuild where the `lookup_key` backfill fails and the index build succeeds, both the streaming result and the background job-status result report index-build success and backfill failure as two distinct fields — never coalesced, never silently omitted.
- **SC-002**: A caller can determine the backfill outcome of a specific rebuild entirely from that rebuild's own result payload, with no additional `knowledge_status` call required.
- **SC-003**: All existing tests referencing `indices_built` (approximately 48 files across `crates/core/tests` and `crates/service/tests`) continue to pass unmodified in their expectations of that field's existing meaning; only new test cases (FR-007) are added.
- **SC-004**: A reader of either call site can explain, from the inline comment alone, why `build_ok` does not depend on backfill outcome and where to look for backfill status instead — without consulting this issue or git blame.

## Assumptions

- The scoping decision (keep `indices_built` limited to index-build success) is treated as resolved by this spec, not left open for the Plan stage, because it follows from `state.indices_built`'s existing functional role in the search auto-heal path — not from a free product-level choice between equally valid options.
- The concrete name and shape of the new per-rebuild backfill-outcome field required by FR-002 (e.g., a new JSON key, or reusing the same underlying check that backs `name_index_trusted`) is left to the Plan/Research stage. This spec requires only that the information be present, per-rebuild, and unambiguous.
- `ensure_lookup_key_backfill`'s retry semantics and `knowledge_status`'s global `name_index_trusted` computation are out of scope for change — this issue is about visibility of a single rebuild's outcome, not the backfill or retry mechanism itself.

## Out of Scope

- Redesigning `lookup_key` backfill retry mechanics (`ensure_lookup_key_backfill`).
- Any change to `build_indices_and_constraints()` itself or its existing failure handling.
- Renaming, removing, or redefining the existing `indices_built` wire field.
- Changing `knowledge_status`'s existing `name_index_trusted` computation.

## Source References

- Issue #221, PR #483 (original `lookup_key` backfill + `build_ok` interaction; the reviewer who deferred this to a follow-up issue).
- `crates/core/src/handlers.rs:2501` and `:2958` — `backfill_entity_lookup_keys_and_record_status` call sites within the two streaming/background-job rebuild arms.
- `crates/core/src/handlers.rs:2708` and `:3089` — the rebuild result/job-status JSON currently emitting `indices_built` alone.
- `crates/core/src/handlers.rs:1007` and `:1053` — the search auto-heal path consulting `state.indices_built` to decide whether to retry index building.
- `crates/core/src/schema.rs:366` (`backfill_entity_lookup_keys`), `:476` (`backfill_entity_lookup_keys_and_record_status`), `:512` (`ensure_lookup_key_backfill`).
- `crates/core/src/db.rs:1935` (`lookup_key_migrated`, backs `name_index_trusted`).
- `crates/core/src/handlers.rs:463`, `:591` — `knowledge_status`'s `name_index_trusted` field.
