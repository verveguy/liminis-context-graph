# Feature Specification: Close (or formally accept) the resolve_ontology stale-drift-insert race

**Feature Branch**: `fabrik/issue-495`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "`AppState::resolve_ontology` (`crates/core/src/app_state.rs`) computes a group's drift status and inserts it into the `group_ontologies` cache without holding that cache's lock across the read-compute-insert sequence. If a concurrent remediation for the same never-before-resolved group — a WAL rebuild, or a successful `add_episode` — writes a new sidecar and clears drift between this call's sidecar read and its insert, the insert overwrites that clear with a stale `drifted: true` computed against the pre-remediation sidecar."

## Background

`AppState::resolve_ontology` is the sole place where a group's ontology and drift status are first computed and cached (issue #446/#451). On a group's first resolution in a process, it reads that group's `.lcg/ontology-hash.json` sidecar, computes whether the group has drifted from it, and inserts a `GroupOntologyEntry` into the `group_ontologies` cache — all without holding the cache's lock across the whole read‑compute‑insert sequence.

If a concurrent remediation for that same never-before-resolved group — a WAL rebuild or a successful `add_episode` — runs in the window between this call's sidecar read and its cache insert, that remediation writes a new sidecar and clears drift. Because `resolve_ontology`'s insert happens after that clear but was computed from data read before it, the insert unconditionally overwrites the cleared state with a stale `drifted: true`, computed against the pre-remediation sidecar.

The result is a false-positive report: `knowledge_status.group_ontology_drift` shows the group as drifted, and a spurious "drift detected — recommend Recreate + re-ingest" line is written to stderr, even though the group is not actually drifted. This condition is self-healing — FR-009 unconditionally clears drift on the group's next successful `add_episode` — but until that next ingest, an operator reading `knowledge_status` or the stderr log is told to Recreate and re-ingest a graph that does not need it.

This race is not a regression. It was identified and deliberately left open during the #451 code review, with the reasoning recorded in a code comment at the insert site in `app_state.rs`. It shipped in `0.13.4` on `maint/0.13.4` and reached `main` via the forward-port in #494; both an automated reviewer (Copilot) and `handarbeit-pruefer` re-flagged it on #494 and confirmed it matches the documented, accepted account, and #494 deliberately left it unchanged. This issue exists to give the race a proper resolution — either close it, or record the "accept permanently" decision somewhere more discoverable than a source comment.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator reads accurate drift status after a concurrent remediation (Priority: P1)

An operator (human or automated tooling) queries `knowledge_status` for a group's ontology drift immediately after another concurrent operation — a WAL rebuild, or an `add_episode` call — has just cleared that group's drift. The operator should see the group's true, post-remediation drift state, not a stale positive computed from before the remediation ran.

**Why this priority**: This is the core defect. A false positive here leads an operator to take a real, disruptive action (Recreate + re-ingest a graph) that isn't necessary — the entire reason this issue exists.

**Independent Test**: Drive `resolve_ontology` and a concurrent drift-clearing remediation (or their component operations) against the same never-before-resolved group with the interleaving from the Background section forced deterministically, and assert the cache's final state reflects the clear, not the stale computation. This is testable in isolation from the rest of the ingest/recovery pipeline.

**Acceptance Scenarios**:

1. **Given** a group has never been resolved in this process and its on-disk sidecar currently reflects a drifted state, **When** `resolve_ontology` reads that sidecar, and then — before `resolve_ontology`'s cache insert runs — a concurrent remediation for the same group writes a new sidecar and clears drift, **Then** the group's final cached drift state (as observed via `knowledge_status.group_ontology_drift` / `AppState::group_drift_status`) is "not drifted," reflecting the remediation, not the stale pre-remediation computation.
2. **Given** the interleaving in Scenario 1 does *not* occur (no concurrent remediation, or the remediation completes before `resolve_ontology`'s sidecar read, or after its insert), **When** `resolve_ontology` runs, **Then** its existing (already-correct) behavior for those orderings is unchanged.

---

### User Story 2 - Fix does not add lock contention for unrelated groups (Priority: P2)

A workspace has many groups. `resolve_ontology`'s cache lock is process-wide, shared by every group. Whatever change closes the race for one group's read-compute-insert sequence must not make other groups' cache operations (reads, inserts, drift clears) wait on a database round trip that wasn't there before.

**Why this priority**: The #451 review explicitly rejected the "hold the lock across a DB round trip" approach for this reason. A fix that reintroduces that cost regresses a decision already made deliberately, and would add latency and contention across every group sharing the process, not just the one being fixed.

**Independent Test**: Inspect (by code review, and/or a targeted concurrency/timing test if one already exists for this cache) that no code path added or changed by this fix holds `group_ontologies`'s lock while performing a database read or write.

**Acceptance Scenarios**:

1. **Given** the fix implemented for User Story 1, **When** `group_ontologies`'s lock is held (for any operation — read, insert, or drift-clear), **Then** no database round trip occurs while it is held.

---

### Edge Cases

- The group **has** been resolved before (an entry already exists in `group_ontologies` at the time `resolve_ontology` is called): this issue's race only applies to a never-before-resolved group, per the Background section and the existing code comment. Confirm the fix does not change behavior for the already-cached case.
- Two (or more) concurrent first-resolutions of the *same* group racing each other (no remediation involved) — confirm this ordering, if not already covered by existing behavior, is not made worse by the fix.
- A remediation that clears drift by **removing** the cache entry entirely, rather than mutating it in place — the issue body flags this as a specific interaction to check for whichever fix approach is chosen (see the code path currently used by `AppState::clear_group_drift`, which mutates an existing entry and is a no-op if none exists).
- Lock poisoning during the insert (already handled today via an `eprintln!` and a no-op) — confirm the fix's behavior on a poisoned lock is at least as good as today's.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When a concurrent remediation (WAL rebuild or successful `add_episode`) clears drift for a never-before-resolved group in the window between `resolve_ontology`'s sidecar read and its cache insert, the group's final cached drift state MUST reflect the remediation's clear, not the stale pre-remediation computation.
- **FR-002**: The behavior of `resolve_ontology` for orderings other than the race window in FR-001 (no concurrent remediation, or remediation entirely before or entirely after the affected window) MUST remain unchanged.
- **FR-003**: Whatever change satisfies FR-001 MUST NOT hold `group_ontologies`'s lock across a database round trip. No unrelated group's cache access may incur new lock-wait time as a result of this fix.
- **FR-004**: The resolution of this issue MUST include either (a) an automated test that deterministically forces the interleaving described in FR-001 and asserts the correct final state, or (b) if the decision is instead to accept the race permanently, a record of that decision in a location more discoverable than the existing source comment (e.g., an ADR), superseding the current in-code note.
- **FR-005**: If FR-004 is satisfied via (a), the existing code comment at the insert site (`// Known race (issue #451 review)...`) MUST be updated or removed to reflect the new, closed behavior — it must not continue to describe a race that no longer exists.

### Key Entities

- **`GroupOntologyEntry`** (`crates/core/src/app_state.rs`): the cached per-group resolution result — the resolved ontology plus its `GroupDriftStatus` (`drifted: bool`, `drift_summary: Option<String>`) — keyed by `group_id` in `AppState::group_ontologies`.
- **Ontology drift sidecar** (`.lcg/ontology-hash.json`, per group): the on-disk record `resolve_ontology` reads to compute drift, and that remediations (WAL rebuild, `add_episode`) write to when they clear it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The acceptance scenario in User Story 1 — a concurrent remediation's drift clear survives a subsequent stale `resolve_ontology` insert for the same never-before-resolved group — passes under either a deterministic automated test (if FR-004(a) is chosen) or is formally documented as an accepted, permanent tradeoff (if FR-004(b) is chosen).
- **SC-002**: No new database round trip occurs while `group_ontologies`'s lock is held, verified by code inspection of the change (and by existing or new tests if lock/DB-access assertions already exist for this cache).
- **SC-003**: All existing tests covering `resolve_ontology`, `group_drift_status`, `all_group_drift_statuses`, and `clear_group_drift` continue to pass unchanged.

## Assumptions

- "Concurrent remediation" is scoped to the two sources named in the issue and confirmed in the code: a WAL rebuild path, and a successful `add_episode` call — both of which clear a group's drift unconditionally on success (FR-009 from #451).
- The decision of *which* technical approach to take (versioned sidecar read, compare-and-insert under the lock, or accepting the race permanently) is a Research/Plan-stage decision, not a Specify-stage one. This spec fixes the observable requirements (FR-001 through FR-005) any chosen approach must satisfy, not the approach itself.
- "More discoverable than a code comment" (FR-004(b)) means at minimum a project ADR (this repo's convention for recorded decisions, per `docs/adr/`); the exact location is a Plan-stage decision.

## Out of Scope

- The previously-accepted, distinct false-negative risk mentioned in the issue's "Status" section (a stale `drifted: false` masking a real drift) is not in scope — only the false-positive race described here.
- Any general redesign of the `group_ontologies` caching strategy beyond what's needed to close (or formally accept) this specific race.
- Changes to how or when remediations (WAL rebuild, `add_episode`) clear drift — only how `resolve_ontology`'s insert interacts with a clear that has already happened.

## Source References

- `crates/core/src/app_state.rs` — `AppState::resolve_ontology` (race site), `AppState::clear_group_drift`, `AppState::group_drift_status`, `AppState::all_group_drift_statuses`, `GroupOntologyEntry`.
- ADR-0451 (per in-code references) — the per-group ontology/drift caching design this race was identified against during review.
- Issues #451, #494 — prior review and forward-port history referenced in this issue.
