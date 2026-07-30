# Feature Specification: #219's NameIndex silently narrowed #218's global endpoint fallback

**Feature Branch**: `fabrik/issue-283`
**Created**: 2026-07-29
**Status**: Draft
**Input**: User description: "#219's NameIndex silently narrowed #218's global endpoint fallback"

## Background

Found while investigating #202. This is not the cause of the drops reported there, but it means the #209 fix (landed as PR #218) is weaker than its own tests suggest, and the weakness is invisible until a specific class of write happens.

Two commits landed about three hours apart, and the second undercut the first:

| commit | when | what |
|---|---|---|
| `04aacec` (PR #218, issue #209) | 2026-07-24 20:16 -0700 | Added a persisted-entity fallback to edge-endpoint validation, calling `get_entity_by_name_ci`, which was then a `WHERE lower(e.name) = $name` table scan — slow but always correct. |
| `f0a8ed3` (issue #219) | 2026-07-25 02:06 -0400 | Replaced that scan with the in-process `NameIndex`, with no scan fallback on a miss. |

`crates/core/src/db.rs:1134-1152` (`get_entity_by_name_ci`) now resolves entirely through `self.name_index.lookup(name, group_id)` and returns `Ok(None)` immediately on a miss. The doc comment on that method says so outright: *"There is deliberately no scan fallback on a miss."* ADR-0038 accepts the consequence explicitly — a stale index degrades to *"a spurious miss, falling through to the embedding-based dedup path **or a dropped edge**, both pre-existing behaviors."*

That reasoning holds for the dedup path, where a miss costs an extra embedding comparison. It does not hold for #218's fallback, whose entire purpose is to be the authority on "does this entity exist anywhere in the group." After `f0a8ed3` that question is answered by an in-memory map rather than the database, so #218's guarantee is now conditional on index coherence — which the #218 tests never exercise, because they build state through `insert_entity`, the one path that keeps the index in sync.

### Ways the index can be blind to a persisted entity

1. **Raw Cypher writes.** `handle_query_cypher` (`crates/core/src/handlers.rs:630-652`) takes the write lock and runs arbitrary caller Cypher with no `NameIndex` hook. Any `CREATE (:Entity …)` or `SET e.name = …` desyncs the index until restart. `SET e.name` is doubly bad: the old key fails verify-on-hit (`db.rs:1146`) and the new name has no key at all. Reachable from the MCP `cypher` scope.
2. **WAL replay whose index rebuild failed.** Replay executes raw templates, bypassing `insert_entity`. The repair is `rebuild_name_index()`, and at both `knowledge_rebuild_from_wal` branches it is non-fatal and only logged (`handlers.rs:1499-1503` and `handlers.rs:1771-1775`). `crates/core/tests/name_index_coherence.rs:255-289` proves replayed entities are invisible until an explicit rebuild.
3. **A second process writing the same database** — eval harness, migration (`crates/service/src/migration.rs:248`, `:402`), a CLI. The running service never learns.
4. **Verify-on-hit failure with no second chance.** `lookup` returns only the `BTreeSet` minimum (`name_index.rs:78`). If that UUID fails verification, the call returns `None` without trying the other same-named rows. Covered by `name_index_coherence.rs:200-232`.
5. **`Db::open_or_rebuild` only rebuilds when the DB file was absent** (`db.rs:100-110`); the DB-exists branch returns an empty index. No production caller today, but a live trap.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Endpoint resolution is authoritative regardless of index state (Priority: P1)

An entity that genuinely exists in the persisted graph must be found by edge-endpoint resolution, even when the in-process `NameIndex` doesn't know about it. Today, resolution trusts the index alone and gives up on a miss, so a real entity can be treated as nonexistent purely because of how or when it was written — not because it's actually absent.

**Why this priority**: This is the core correctness guarantee #218 was built to provide. Without it, edges to legitimately-existing entities are dropped, and the failure is silent — nothing in the current tests or logs points at it.

**Independent Test**: Persist an entity through a path that does not update the `NameIndex` (raw Cypher, or WAL replay with a forced index-rebuild failure), then ingest a chunk whose extracted edge names that entity. Verify the edge is retained rather than dropped.

**Acceptance Scenarios**:

1. **Given** an entity persisted via raw Cypher (not `insert_entity`), **When** a later chunk emits an edge naming it, **Then** the edge resolves and is retained.
2. **Given** an entity restored by WAL replay whose `rebuild_name_index()` failed, **When** a later chunk emits an edge naming it, **Then** the edge resolves.

---

### User Story 2 - Index desync is observable (Priority: P2)

When the `NameIndex` misses on a name that a full scan would have resolved, that event must be countable and visible through existing status/telemetry surfaces, so index desync can be diagnosed operationally instead of only by reading source code or reproducing a bug locally.

**Why this priority**: Once User Story 1 restores correctness via a fallback path, desync becomes a performance/health concern rather than a correctness one — but it still needs to be visible, since a fallback that fires constantly signals a deeper index-maintenance bug worth fixing (e.g. an untracked write path).

**Independent Test**: Force an index miss that a scan resolves (e.g. via the raw-Cypher path from User Story 1), then query `knowledge_status` or equivalent telemetry and confirm the fallback/miss is reflected in the counters.

**Acceptance Scenarios**:

1. **Given** a `NameIndex` miss that a scan would have resolved, **When** it occurs, **Then** it is counted and surfaced so desync is diagnosable without reading source.

---

### Edge Cases

- A scan fallback on a large graph must not reintroduce the full-table-scan cost #219 removed. It must be bounded to the set of names a batch actually failed to resolve — this set is already deduplicated at `episode.rs:255-268` (`missing_names`), so the fallback should reuse that shape rather than scanning per-edge.
- `merge_entities` leaves aliases in the index deliberately (`name_index_coherence.rs:158-166`), so a lookup can resolve to a `Merged`-labelled tombstone. Whether a scan fallback should honor that same behavior (resolve to the tombstone) or treat it differently needs to be confirmed once a fallback path exists — see Assumptions.
- `group_id` is never normalized on either side of the index (lookup or scan). `"liminis"`, `" liminis"`, and `"Liminis"` are three distinct namespaces today, and this issue does not change that — a fallback scan must reproduce the same non-normalization behavior as the index it's backing up, not silently start normalizing.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Edge-endpoint resolution (`get_entity_by_name_ci` as called from #218's fallback path) MUST NOT depend solely on the in-process `NameIndex` being coherent. Either restore a scan fallback on a miss for this call site, or make the index authoritative by construction (e.g. every write path that can create/rename/delete an `Entity` row updates or invalidates the index before the write is considered complete).
- **FR-002**: If a scan fallback is restored, it MUST be bounded so #219's performance win is not lost on the common case (an index that is coherent). Concretely: scan only for the names a batch actually failed to resolve — a deduplicated set already computed at `episode.rs:255-268` — not per-edge and not the whole batch.
- **FR-003**: `rebuild_name_index()` failure after WAL replay (`handlers.rs:1499-1503`, `handlers.rs:1771-1775`) MUST NOT remain silently non-fatal. It MUST either fail the reload/replay operation outright, or mark the index untrusted so that subsequent lookups fall back to a scan until the index is successfully rebuilt.
- **FR-004**: `handle_query_cypher` (`crates/core/src/handlers.rs:630-652`) MUST either invalidate the affected portion of the `NameIndex` when it executes an entity-mutating statement, or reject entity-mutating statements on that path, rather than leaving the index silently stale after an arbitrary write.
- **FR-005**: `NameIndex::lookup` (`crates/core/src/name_index.rs:71-79`) verify-on-hit failure MUST try the remaining same-named candidates (the rest of the `BTreeSet` for that key) before returning `None`, instead of giving up after the first (deterministic-minimum) candidate fails verification.
- **FR-006**: Tests for this fix MUST build state through a non-`insert_entity` path (raw Cypher or WAL replay) rather than exclusively through `insert_entity`, so this class of bug — index-bypassing writes silently breaking endpoint resolution — cannot pass undetected again.

### Key Entities

- **`NameIndex`**: In-process, in-memory accelerator (`crates/core/src/name_index.rs`) mapping `(group_id, lowercased name)` to a `BTreeSet` of `(created_at, uuid)` candidates, keeping a deterministic "winner" per name. Populated by `insert`/`update_created_at` (kept in sync automatically) and by `rebuild` (a full one-shot scan used at startup, after recovery, and after WAL rebuild).
- **`get_entity_by_name_ci`**: The lookup used by #218's edge-endpoint fallback (`db.rs:1134-1152`). Currently resolves solely through `NameIndex.lookup`, re-verifying the result against the database but never retrying or scanning on a miss.
- **Index trust state**: Not currently modeled. This issue introduces the need for some notion of "the index may be stale" (FR-003) that downstream lookups can consult to decide whether to fall back to a scan.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test that creates an entity via raw Cypher, then ingests an edge naming it, retains the edge. This test fails on `main` today (pre-fix) and passes after the fix.
- **SC-002**: A test that replays a WAL with a forced `rebuild_name_index()` failure still resolves endpoints correctly (does not silently drop edges to entities restored by that replay).
- **SC-003**: The #219 dedup benchmark (`dedup_overlap_check`, see the project's bench workflow) shows no meaningful regression from this change on the common case (a coherent index, no fallback scans triggered). "No meaningful regression" is intentionally not pinned to a specific percentage in this spec — see Assumptions; the concrete threshold is a Plan/Review-stage decision informed by actual before/after measurements.
- **SC-004**: `knowledge_status` or an equivalent telemetry surface exposes (a) whether the `NameIndex` is currently considered trusted, and (b) a count of fallback scans performed, so index desync is diagnosable without reading source or reproducing the bug.

## Assumptions

- #219's performance motivation is still valid; this issue is about correctness of the miss path, not about reverting the index.
- **On SC-003's "agreed margin"**: no numeric regression threshold is fixed at spec time. The scan fallback is only exercised on a miss (FR-002 bounds it to unresolved names), so the common, already-benchmarked case — a coherent index with no misses — is expected to see no added cost. The Plan stage should decide the concrete acceptable-regression threshold (if any) once an implementation approach is chosen, informed by actual `dedup_overlap_check` numbers; Review should confirm the measured delta stays within whatever threshold Plan set.
- **On the `merge_entities`/`Merged`-tombstone edge case**: this spec assumes a scan fallback should reproduce the same resolution behavior the index currently has — including resolving to a `Merged`-labelled tombstone where the index would. Research/Plan should confirm this is still the intended behavior once a concrete fallback mechanism is chosen; it is called out explicitly rather than silently decided because it affects what "resolves" means for an entity that has been merged away.
- The choice between "restore a scan fallback" and "make the index authoritative by construction" (FR-001, FR-003, FR-004) is left open deliberately — it is an implementation/architecture decision for the Plan stage, not a product-level requirement of this spec.

## Out of Scope

- The endpoint-drift defect from #202 (a separate, already-filed issue) — that one drops edges for endpoints which never existed at all, which is a different failure mode from this issue's "endpoint exists but the index doesn't know it."
- Normalizing `group_id` handling (case/whitespace) across the index and any fallback scan. Both must behave consistently with each other, but changing that behavior itself is not in scope here.

## Source References

- Issue #209 / PR #218 (`04aacec`) — original persisted-entity fallback for edge-endpoint validation.
- Issue #219 (`f0a8ed3`) — `NameIndex` introduction that removed the scan fallback.
- ADR-0038 (`docs/adr/0038-in-process-name-index.md`) — accepts index-miss degradation for the dedup path; does not address the endpoint-fallback path this issue is about.
- `crates/core/src/db.rs:1125-1152` — `get_entity_by_name_ci`.
- `crates/core/src/name_index.rs:71-79` — `NameIndex::lookup`.
- `crates/core/src/handlers.rs:630-652` — `handle_query_cypher`.
- `crates/core/src/handlers.rs:1499-1503`, `:1771-1775` — non-fatal `rebuild_name_index()` failure handling after WAL replay.
- `crates/core/src/episode.rs:245-268` — existing deduplicated `missing_names` set that a bounded scan fallback should reuse.
- `crates/core/tests/name_index_coherence.rs:158-166`, `:200-232`, `:255-289` — existing coverage of merge-tombstone aliasing, verify-on-hit failure, and post-replay index blindness.
- Issue #202 — the investigation that surfaced this issue; explicitly not the same defect.
