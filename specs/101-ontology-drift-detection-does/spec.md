# Feature Specification: Fix Ontology Drift Detection for First-Ever Ontology Addition

**Feature Branch**: `fabrik/issue-101`
**Created**: 2026-05-26
**Status**: Draft
**Input**: Live observation 2026-05-26 — added `ontology.yaml` to a workspace that had previously ingested without one, restarted the service, and expected `knowledge_status.ontology.drifted` to be `true` per liminis-graph#98's User Story 1 acceptance scenario 2. Actual: `drifted: false`.

## Background

liminis-graph#98 (merged 2026-05-26 as PR #99) added ontology drift detection. The User Story 1 acceptance scenarios from #98's spec included:

> 2. **Given** a workspace with no ontology when ingestion happened and now an ontology.yaml exists, **When** the service starts, **Then** drift is reported (added ontology counts as a kind of drift — the existing graph has no types from this ontology).
> 3. **Given** a workspace that had an ontology when ingesting and now has no ontology.yaml, **When** the service starts, **Then** drift is reported (removed ontology — graph types may no longer match anything declared).

The implementation correctly handles the edit-and-restart drift case (scenario 1) and no-change case (scenario 4), but silently fails for the addition case (scenario 2). The demo-notebook reproduces the bug: graph was ingested without an ontology, an ontology was added, restart shows `drifted: false`.

**Likely cause**: the persisted ontology hash for a "no ontology" state is stored as null/empty/matching-anything, and when a new ontology is added later, the comparison treats the absence-of-prior as "no drift" rather than "ontology added".

**Why this matters**:
- **Defeats the whole point of #98 in the common case.** First-time ontology adoption is the single most common ontology-change event. Users dropping in their first ontology will silently get a mixed graph with no warning.
- **Inconsistent behavior.** Detecting edit-drift but not addition-drift erodes trust in the indicator.
- **Implementation drift from the merged spec.** User Story 1 of #98 explicitly covered this; this bug is a missed acceptance scenario.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — First Ontology Addition Triggers Drift (Priority: P1)

When a workspace has been ingested under no ontology and the user subsequently adds an `ontology.yaml` file, the next service restart MUST report `drifted: true` with a summary that makes clear the ontology has been newly added.

**Why this priority**: this is the single most common ontology-change pattern (first adoption), was explicitly in the merged spec, and the demo-notebook is blocked on it today.

**Independent Test**: Take a workspace where `knowledge_status.ontology.loaded: false`. Add a valid `ontology.yaml`. Restart. Call `knowledge_status`. Assert `ontology.drifted: true` and `drift_summary` is non-null and mentions ontology addition.

**Acceptance Scenarios**:

1. **Given** a workspace that has been ingested without an ontology, **When** the user adds a valid `ontology.yaml` and restarts, **Then** `knowledge_status.ontology.drifted` is `true`.
2. **Given** the same scenario, **When** `knowledge_status` is queried, **Then** `drift_summary` reads something like `"ontology added: N entity types, M relation types"`.
3. **Given** the user runs Recreate and re-ingests under the new ontology, **When** the next `knowledge_status` is queried, **Then** `drifted: false` (the new ontology is now the baseline).

---

### User Story 2 — Ontology Removal Also Triggers Drift (Priority: P1)

Symmetrically, a workspace that had an ontology when ingested and now has no `ontology.yaml` MUST report `drifted: true` with a summary that makes clear the ontology has been removed.

**Why this priority**: symmetric treatment of removal closes the same logical gap and is covered by the same fix.

**Independent Test**: Take a workspace where `knowledge_status.ontology.loaded: true`. Remove `ontology.yaml`. Restart. Assert `drifted: true` and `drift_summary` mentions ontology removal.

**Acceptance Scenarios**:

1. **Given** a workspace ingested under an ontology, **When** the user deletes `ontology.yaml` and restarts, **Then** `drifted: true`.
2. **Given** the same scenario, **When** `drift_summary` is read, **Then** it mentions ontology removal (e.g. `"ontology removed (was N entity types, M relation types)"`).

---

### User Story 3 — "No Ontology" State Is Hashed Distinctly from "No Hash Persisted" (Priority: P1)

The persisted hash representation MUST distinguish three states:

- **No ontology ever** (no hash persisted — first-ingest state, no drift possible yet)
- **Ingested with no ontology** (hash persisted representing the empty/null state)
- **Ingested with a specific ontology** (hash persisted representing that ontology's content)

Comparison logic MUST treat "no prior hash" as first-ingest (no drift), but treat "prior hash = no-ontology" + "current = has-ontology" as drift.

**Why this priority**: this is the foundation for both User Stories 1 and 2. Without this three-way distinction, the fix cannot be correct.

**Acceptance Scenarios**:

1. **Given** a workspace with no prior writes (no persisted hash), **When** the service starts, **Then** `drifted: false` regardless of whether `ontology.yaml` is present (nothing to compare against yet).
2. **Given** a workspace whose persisted hash represents "no ontology" and the current state has an ontology loaded, **When** the service starts, **Then** `drifted: true`.
3. **Given** a workspace whose persisted hash represents a specific ontology and the current state has no ontology loaded, **When** the service starts, **Then** `drifted: true`.

---

### Edge Cases

- **Workspace ingested under #98's existing implementation** (hash represents "no ontology" via missing file, not explicit sentinel). After fix: restart with no ontology → `drifted: false`. After adding ontology → `drifted: true`. Handled by backward-compat logic in FR-006.
- **Workspace ingested with ontology, no changes since, restart.** Must report `drifted: false` — already correct in #98's implementation, must not regress.
- **`ontology.yaml` has only comment changes** (semantically unchanged). Per #83/#98 FR-001, comment edits don't change the semantic hash. Behavior unchanged: `drifted: false`.
- **`ontology.yaml` goes from one valid ontology to an empty/malformed file.** Treat as "no ontology loaded due to error", but DO report drift against the previous valid state (the user has lost ontology coverage).
- **Multiple ontology edits between ingest events.** The persisted hash reflects the in-effect ontology at the last ingest. Only the most-recent state matters for the drift comparison — unchanged from the existing implementation.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001.** The persisted ontology-hash representation MUST distinguish "no ontology in use" from "no hash persisted yet". Recommended: persist an explicit sentinel value (e.g. `{"hash": null, "had_ontology": false}`) rather than treating absence-of-file as both states.
- **FR-002.** The drift-comparison function MUST report `drifted: true` when the persisted state represents "no ontology" and the current state represents "has ontology" — and vice versa.
- **FR-003.** The drift summary string MUST describe the transition clearly. Suggested wording: `"ontology added: N entity types, M relation types"`, `"ontology removed (was N entity types, M relation types)"`, or `"ontology changed: ..."` for the edit case (which already works).
- **FR-004.** The hash file MUST be written on first ingest, even when no ontology is loaded — this captures the "we ingested without an ontology" state so future ontology additions can be detected as drift.
- **FR-005.** Regression tests MUST cover all three drift transitions: none→has, has→none, and has-A→has-B (the existing case from #98).
- **FR-006.** The fix MUST be additive and backward-compatible: existing workspaces with hash files from #98's initial implementation should be handled gracefully (treat missing `had_ontology` field as ambiguous and proceed without false-positive drift).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001.** With the demo-notebook in its current state (ingested without ontology, ontology added today), restarting the service reports `ontology.drifted: true` with a meaningful `drift_summary`.
- **SC-002.** All three drift transitions (none→has, has→none, has-A→has-B) report `drifted: true`.
- **SC-003.** No-change cases (no ontology before and now, same ontology before and now) report `drifted: false`.
- **SC-004.** New regression tests pass. Existing #98 tests pass unchanged.
- **SC-005.** Demo-notebook user experience: after this fix lands + restart, the user sees a clear drift indicator they can act on (Recreate prompt is visible and actionable).

## Assumptions

- **A1.** The current #98 implementation has a hash representation that conflates "no ontology" with "no hash". The fix is a small addition to the hash format, not a rewrite.
- **A2.** Persisting a hash on the first ingest (even when no ontology is loaded) is safe — it adds a few hundred bytes of metadata to every workspace.
- **A3.** Backward compatibility is straightforward — old hash files can be migrated lazily on next read (assume "ambiguous prior state" = "no drift" until next ingest writes the new format).

## Out of Scope

- Changing the drift-summary structure beyond what's needed to describe addition/removal (FR-003 clarifies the string only).
- Hot-reload of ontology (still deferred per #83).
- UI changes — the IPC field already exists from #98; this just makes it report correctly.
- Resolving the sibling "directory schism" issue (filed alongside this) — both should land before drift detection works reliably end-to-end, but they are separate fixes.

## Source References

- **liminis-graph#98 (merged 2026-05-26):** the parent issue. This bug is a missed acceptance scenario from #98's User Story 1.
- **liminis-graph#83 (merged):** ontology support. The empty/no-ontology state semantics this fix needs to formalize.
- **Demo-notebook 2026-05-26:** live workspace where the bug reproduces. Useful as the manual SC-001 test fixture.
- **Implementation files:** `liminis-graph-core/src/ontology.rs` (hash representation and comparison), `liminis-graph-core/src/app_state.rs` or `handlers.rs` (hash persistence on first ingest).
