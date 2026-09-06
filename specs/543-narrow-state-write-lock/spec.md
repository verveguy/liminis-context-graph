# Feature Specification: Narrow `state.write_lock`'s critical section around the embedder round trip

**Feature Branch**: `fabrik/issue-543`
**Created**: 2026-09-05
**Status**: Specified
**Input**: User description: "Narrow state.write_lock: don't hold the instance-wide write lock across the embedder round trip — split out of #510 / ADR-0510, which names this as the better fix it deliberately did not make: dropping the lock during the embed call and re-resolving under a freshly-acquired guard immediately before insert (falling back to the update path if another writer won the race) removes the unbounded-hold hazard structurally rather than merely bounding it."

## Background

Since issue #444 (PR #487), `knowledge_assert_entity` and `knowledge_assert_relationship` resolve whether their target entity/edge already exists *before* deciding whether to call the embedder — a correct fix for a different bug (calling the embedder before knowing it was even needed). A side effect of that reordering is that the create branch's embedder call now happens **while `state.write_lock` is held**, because the lock is acquired once, up front, and released only after either the update or the insert completes. `write_lock` is the only thing preventing two concurrent creates of the same not-yet-existing `(name, group_id)` entity, or the same not-yet-existing `(source, target, predicate, group_id)` edge, from both resolving "not found" and both inserting — so today's code buys that correctness guarantee by serializing *every* writer in the process behind *any* in-flight embedder round trip.

ADR-0510 and #541 addressed the acute version of this hazard — an embedder that never responds, which could previously wedge every write on the instance until the process restarted — by adding request/connect timeouts (HTTP transport, ADR-0510) and a bound on the UDS transport (#541). Both bound the *worst case*: with a 30-second default timeout, a single slow-but-healthy embedder response can still serialize every writer behind it for up to 30 seconds. That is the normal, non-degraded cost this issue removes: it is not about hangs, it is about a single slow (but successful) embedder call being able to stall unrelated writes for the duration of that call, every time a new entity or edge is created.

ADR-0510 named the structural fix explicitly as something it deliberately left out of scope, to be filed as a follow-up: drop the lock for the embed call, then re-acquire and re-resolve immediately before the insert, falling back to the update path if a concurrent writer won the race in the meantime. This issue is that follow-up.

The re-resolve step needs the same discipline the #221 review applied to `get_entity_by_name_ci_with_scan_fallback`'s self-heal: a write must be scoped to the exact state a prior read observed, so that if that state has changed by the time the write happens, the write becomes a safe no-op (or takes the correct alternate branch) rather than clobbering whatever a concurrent writer did in between. Here, that means the post-embed insert-or-update decision must be made from a *fresh* resolution taken after the lock is re-acquired, never from the pre-embed resolution that was current before the lock was dropped.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Unrelated writes are not gated by another call's embedder latency (Priority: P1)

As a caller issuing concurrent `knowledge_assert_entity` / `knowledge_assert_relationship` requests (e.g., an ingestion pipeline processing multiple episodes in parallel), I want a request that doesn't involve creating a new entity/edge to complete without waiting for some other in-flight request's embedder round trip, so that overall write throughput is not held hostage to embedder response time.

**Why this priority**: This is the entire point of the issue — today's instance-wide serialization across the embedder round trip is the cost being removed. Without this, the fix has no observable effect.

**Independent Test**: With a mock/injectable embedder that can be made to delay its response, issue an `knowledge_assert_entity` call that will take the create branch (new name/group_id) concurrently with a second call that does not need to create anything (asserts an entity that already exists, or asserts a relationship whose edge already exists). Confirm the second call completes without waiting for the first call's embedder response.

**Acceptance Scenarios**:

1. **Given** entity E does not yet exist in group G, **When** a call to assert E is in flight and blocked inside its embedder round trip, **Then** a concurrent call that only updates an already-existing, unrelated entity in the same group completes without waiting for E's embedder call to return.
2. **Given** relationship edge X does not yet exist, **When** a call to assert X is in flight and blocked inside its embedder round trip, **Then** a concurrent call that only updates an already-existing, unrelated edge completes without waiting for X's embedder call to return.

---

### User Story 2 - Concurrent creates of the same entity/edge never produce a duplicate or a lost update (Priority: P1)

As a caller, when two of my concurrent requests both try to create the same not-yet-existing entity or edge (a race that today's lock-held-across-embed design prevents by serializing them), I want the outcome to be exactly what it is today — one create and one update, no duplicate row, no silently discarded write — even though the lock is no longer held across the embed call.

**Why this priority**: This is the correctness invariant the current design protects, and the one most at risk from narrowing the critical section. It must hold with the same strength as today, or the fix trades a throughput problem for a data-integrity one.

**Independent Test**: With a mock/injectable embedder, start two concurrent calls that both resolve "not found" for the identical entity (same `name`+`group_id`) — or the identical edge (same `source`, `target`, `predicate`, `group_id`) — before either has re-acquired the lock for insert. Confirm exactly one row is created and the other call's result reflects an update against that same row, with no error and no duplicate.

**Acceptance Scenarios**:

1. **Given** two concurrent `knowledge_assert_entity` calls for the same not-yet-existing `(name, group_id)`, **When** both proceed through their embedder calls with the lock dropped and then race to re-acquire it, **Then** exactly one entity is created, and the loser's call resolves via the update path against the winner's newly created row — not a duplicate, not an error.
2. **Given** two concurrent `knowledge_assert_relationship` calls for the same not-yet-existing `(source, target, predicate, group_id)`, **When** both proceed through their embedder calls with the lock dropped, **Then** exactly one edge is created, and the loser's call resolves via the update path against the winner's newly created edge.
3. **Given** a `knowledge_assert_entity` call that resolved "not found" and is in its embedder round trip, **When**, before it re-acquires the lock, a concurrent writer both creates a *different* active entity in the same group under the exact name the first call is trying to create, **Then** the first call's post-embed re-resolution finds that entity and takes the update path against it (per this codebase's existing name-based resolution semantics), rather than inserting a second, colliding row.

---

### Edge Cases

- **Rename-collision guard must still fire post-race.** `knowledge_assert_entity`'s existing check — rejecting a request that would rename an entity to a name already held by a different active entity — must continue to fire correctly when the collision only exists because of a write that happened during the dropped-lock window, not only when it existed at the time of the original (pre-embed) resolution.
- **Embedder failure during the dropped-lock window.** If the embedder call errors (today: falls back to a zero-vector embedding with a warning), that existing fallback behavior is unaffected by this change — the lock is simply not held while waiting for that call to fail or time out either.
- **`entity_uuid`-addressed calls never take the create branch.** When a caller supplies `entity_uuid`, resolution either finds that exact row or errors — it cannot resolve "not found" and fall to the create branch — so the create-race scenario in this issue does not arise for uuid-addressed calls. This issue's race applies to name-addressed entity creation and to relationship creation.
- **Losing the race changes `created` from `true` to `false` for the loser.** A caller who expected to create a row and instead observes `"created": false` (because a concurrent caller won the race) must see the same response shape and semantics as an ordinary "asserted an already-existing entity/edge" call today — this is not a new response shape, just a new way to arrive at the existing "already existed" outcome.
- **Both racing calls have their own embedder round trip.** Nothing shortcuts the loser's embedder call — it still computes its own embedding (which is then simply not persisted, since it takes the update path) — this issue is about not holding the lock during that call, not about skipping it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_assert_entity`'s create path MUST NOT hold `state.write_lock` while awaiting the embedder for either `name_embedding` or `summary_embedding`.
- **FR-002**: `knowledge_assert_relationship`'s create path MUST NOT hold `state.write_lock` while awaiting the embedder for `fact_embedding`.
- **FR-003**: After the embedder round trip completes and before performing the insert, each handler MUST re-acquire `state.write_lock` and re-resolve existence of the target entity/edge — the insert-or-update decision MUST be based on this fresh, post-embed resolution, never on the resolution taken before the lock was dropped.
- **FR-004**: If the post-embed re-resolution finds that the target entity (by `name`+`group_id`) or edge (by `source`, `target`, `predicate`, `group_id`) now exists — because a concurrent writer created or altered it during the window the lock was dropped — the handler MUST take the existing update path against that row instead of inserting a new one.
- **FR-005**: The externally observable result of losing the create race (response shape, `created` flag, resulting stored row) MUST be indistinguishable from calling assert against an already-existing entity/edge today.
- **FR-006**: Two concurrent calls targeting the same not-yet-existing entity identity (`name`+`group_id`) or edge identity (`source`, `target`, `predicate`, `group_id`) MUST NOT ever both insert — exactly one row results, regardless of how their embedder round trips interleave.
- **FR-007**: A write performed by one caller during the window another caller's lock is dropped for its embedder call MUST NOT be silently lost or overwritten when that other caller subsequently proceeds through its own post-embed insert-or-update step.
- **FR-008**: `knowledge_assert_entity`'s existing rename-collision guard (rejecting a rename that would collide with a different active entity of the same name) MUST continue to be correctly enforced when the change to a lock-narrowed critical section means the collision only becomes visible at the post-embed re-resolution rather than at the original pre-embed resolution.
- **FR-009**: Existing behavior that does not involve holding the lock across an embedder call (the update path, which never calls the embedder for either handler; error handling; the zero-vector embedder-failure fallback; response field names and shapes) MUST be unchanged.
- **FR-010**: An automated test MUST exercise two concurrent `knowledge_assert_entity` calls racing to create the identical not-yet-existing entity, and assert the outcome required by FR-006/FR-004 (exactly one created, the other resolved via update, no error, no duplicate).
- **FR-011**: An automated test MUST exercise two concurrent `knowledge_assert_relationship` calls racing to create the identical not-yet-existing edge, and assert the same class of outcome as FR-010.
- **FR-012**: An automated test or benchmark MUST demonstrate that an unrelated write (one that does not take the create branch) completes without being gated by a concurrent call's in-flight embedder round trip — e.g., using a mock embedder with controllable/injectable latency, showing the unrelated write's completion is not ordered after the slow embedder call's return.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With a mock embedder configured to delay its response, a concurrent unrelated write completes before the delayed embedder call returns, in an automated test (demonstrating FR-012 directly rather than by inference).
- **SC-002**: A concurrency test reproducing two racing creates of the same entity identity passes deterministically (not flaky/timing-dependent) and shows exactly one created row and zero duplicates across repeated runs.
- **SC-003**: The equivalent concurrency test for two racing creates of the same edge identity passes deterministically with the same one-row, zero-duplicate guarantee.
- **SC-004**: No existing test in the suite covering `knowledge_assert_entity`'s or `knowledge_assert_relationship`'s update path, collision guard, or embedder-failure fallback regresses.
- **SC-005**: Measured concurrent write throughput (calls/second, or total wall-clock time for a fixed batch of concurrent assert calls) does not scale linearly with injected embedder latency the way it does before this change — i.e., increasing embedder response time no longer proportionally increases the time to complete a batch of otherwise-unrelated concurrent writes.

## Assumptions

- The only two call sites in this codebase that currently hold `state.write_lock` across an embedder round trip are `knowledge_assert_entity`'s and `knowledge_assert_relationship`'s create branches (`crates/core/src/handlers.rs`, `handle_assert_entity` / `handle_assert_relationship`). `knowledge_add_cross_group_edge` already computes its embedding before acquiring the lock and is unaffected. Other embedder call sites (episode ingestion in `episode.rs`, dedup/merge in `canonicalize.rs`, search in `search.rs`, batch backfill) do not hold `state.write_lock` across an embed call and are out of scope.
- The update paths for both handlers never call the embedder (entity update never touches `name_embedding`/`summary_embedding`; `update_relates_to_core` never persists an embedding), so no lock-narrowing is needed there — the fix is entirely about the create branch.
- A concurrency test can use this codebase's existing mock/injectable embedder test infrastructure to force a deterministic interleaving (e.g., a barrier or controllable delay), rather than relying on real network timing races.
- "No lost update" (FR-007) is scoped to the specific race this issue addresses — a concurrent writer acting on the same entity/edge identity between the lock drop and re-acquire. It does not introduce new guarantees about writes to *different* entities/edges, which were never ordered against each other.
- SC-005's throughput measurement is satisfied by a targeted test or benchmark demonstrating the independence described, not by a full production load test.

## Out of Scope

- UDS transport timeout hardening (tracked separately, #541).
- Any change to embedder timeout values or configuration (#510 / ADR-0510).
- Retry-on-embedder-failure behavior — unchanged from today's zero-vector-embedding fallback with a warning.
- Any embedder call site that does not currently hold `state.write_lock` across the call (episode/batch ingestion, cross-group edge creation, search, dedup/canonicalization) — unless research surfaces that one of these shares the identical lock-held-across-embed hazard, in which case that is a finding for the Research stage to raise, not something assumed resolved by this spec.
- Changing the identity/upsert keys themselves (`(name, group_id)` for entities; `(source_node_uuid, predicate, target_node_uuid, group_id)` for edges) — this issue only changes when the lock is held around resolving and acting on those keys, not what the keys are.

## Source References

- ADR-0510 (`docs/adr/0510-oaiembedder-http-timeouts.md`), "Out of Scope" section — names this exact follow-up.
- `crates/core/src/handlers.rs`: `handle_assert_entity` (create branch, `EntityAssertOutcome::ToCreate`), `handle_assert_relationship` (create branch, `EdgeAssertOutcome::ToCreate`).
- Issue #444 / PR #487 — introduced the resolve-before-embed reordering that put the embed call inside the lock's critical section.
- Issue #221 — the TOCTOU discipline (scope a write to the exact state a prior read observed) that this issue's re-resolve step must apply.
- Issue #510 / #541 — bounded (but did not eliminate) the unbounded-hold version of this hazard via embedder timeouts.
