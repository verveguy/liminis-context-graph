# Feature Specification: Replace In-Process Entity-Name Lookup Map with a Secondary ART Index

**Feature Branch**: `fabrik/issue-221`
**Created**: 2026-07-25
**Status**: Specified
**Input**: User description: "Replace in-process entity-name lookup map with a secondary ART index (post lbug 0.19)"

## Background

`Conn::get_entity_by_name_ci` (`crates/core/src/db.rs`) is backed today by `NameIndex`, an in-process `HashMap<(group_id, name_lower), BTreeSet<uuid>>` accelerator introduced by ADR-0038 (issue #219), because lbug 0.17.0 offered no secondary index for non-PK columns and `lower(e.name) = $x` predicates are never push-down indexable on any lbug version. That accelerator works and is verified against the database on every hit, but it carries costs the database should be bearing instead:

- **Invalidation surface** — the map must be updated on every mutation path that can change a name→UUID mapping (insert, `merge_entities`, `delete_by_source`, `delete_episode`, `clear_all`, `apply_corrections`, `rebuild_from_wal`, recovery). A missed site risks silently corrupting dedup.
- **Startup cost** — a full `Entity` scan to rebuild the map on every service start and after every rebuild.
- **Memory** — proportional to entity count (modest today, unbounded as graphs grow).
- **Duplicated state** — an index of the database, maintained outside the database.

lbug 0.18.0 added non-PK secondary ART indexes (LadybugDB/ladybug#582): physical creation on tables that already have a PK, planner push-down via `popSecondaryARTEqualityComparison`, non-unique secondary leaves with `lookupAll`, and WAL-logged index builds. This issue was originally filed against a pending 0.18.1 upgrade; that upgrade (issue #190) was subsequently retargeted to lbug 0.19.1 to pick up 0.19.0's checkpoint lock-file and read-only-open fixes, and has since **shipped and closed** — `Cargo.toml` pins `lbug = "=0.19.1"`. This issue is therefore no longer blocked on the version upgrade; the secondary-ART-index capability it depends on is present now.

Since ADR-0038 landed, the invalidation surface `NameIndex` must track has grown, and all of the following are now merged:

- **#378** — WAL streams became per-group (multi-stream WAL), rather than a single stream.
- **#385** — fixed mutation attribution for `delete_by_group` and `rebind_pointers`, which had been writing other groups' mutations to the default WAL stream.
- **#361** — added `knowledge_delete_by_group` (group-scoped complete purge). This is documented in `db.rs` as the one *deliberate* exception to "no path deletes `Entity` nodes without invalidating `NameIndex`" (see the comment on `remove_episode` at `db.rs:830-833`, and `crate::group_purge`) — any replacement design must account for this path explicitly, not assume the four call sites are the only ones that matter.
- **ADR-0283** (issue #283) narrowed `NameIndex`'s "no scan fallback" stance for two of its four call sites, and established that name resolution must pass **through** `Merged`-labelled tombstones rather than filtering them out (`db.rs:1659`, `corrections::merge_entities`) — a lookup for a name that resolves to a merged-away alias returns the same `ORDER BY created_at ASC, uuid ASC LIMIT 1` winner whether or not that winner is a tombstone. Cross-group pointer resolution (#369) depends on this exact behavior; a secondary ART index replacement that changed it would silently change #369's meaning.

This work replaces the in-process map with a materialized `lookup_key` column on `Entity` (a composite `group_id + '\x1f' + lower(name)` key, computed host-side in Rust) plus a `CREATE ART INDEX` on that column, so the database itself serves the lookup. `get_entity_by_name_ci`'s signature and behavior at its call sites are unchanged — this is a drop-in swap behind the same API.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Entity name lookup is served by the database's own index (Priority: P1)

As the ingest pipeline resolves entity names to UUIDs, `get_entity_by_name_ci` is answered by an ART-indexed equality lookup on a materialized `lookup_key` column instead of an in-process structure the host process must keep in sync by hand.

**Why this priority**: this is the core of the issue — it removes the invalidation surface, startup scan, and duplicated state that motivate the change, while preserving the performance win the in-process map already delivered.

**Independent Test**: Populate a graph with ≥10k entities, run `EXPLAIN` on the query behind `get_entity_by_name_ci`, and confirm it shows an index-backed scan rather than a full table scan. Call the function with names that do and do not exist and confirm correct results.

**Acceptance Scenarios**:

1. **Given** an `Entity` table with a `lookup_key` column and ART index built, **When** `EXPLAIN` is run on the query behind `get_entity_by_name_ci`, **Then** the plan shows `PRIMARY_KEY_SCAN_NODE_TABLE ... Index: ART` and not a full `ScanNodeTable` + `Filter`.
2. **Given** the same setup, **When** `get_entity_by_name_ci` is called with a name that exists (case-insensitively) within a given `group_id`, **Then** the correct UUID is returned.
3. **Given** the same setup, **When** `get_entity_by_name_ci` is called with a name that does not exist in that `group_id`, **Then** no match is returned.
4. **Given** the fix is in place, **When** the four existing call sites in `crates/core/src/episode.rs` invoke `get_entity_by_name_ci`, **Then** their behavior and logic are unchanged — only the lookup's internal access path changes.

---

### User Story 2 - Existing databases migrate cleanly to the new index (Priority: P1)

As a database created before this change is opened by an upgraded service, its `Entity` rows are backfilled with `lookup_key` and the ART index is built once, without requiring a manual operator step.

**Why this priority**: without a clean migration path, every existing deployment breaks on upgrade — this gates shipping the change at all.

**Independent Test**: Open a pre-migration database (no `lookup_key` column, no ART index) with the upgraded service and confirm the column is backfilled, the index is built, and subsequent lookups are correct — as a one-shot step, since `CREATE ART INDEX` backfills by scanning and blocks (no `CONCURRENTLY`).

**Acceptance Scenarios**:

1. **Given** an existing database with `Entity` rows but no `lookup_key` column, **When** the migration step runs, **Then** every existing `Entity` row has `lookup_key` populated correctly from its current `name` and `group_id`.
2. **Given** the backfilled column, **When** the ART index build runs, **Then** it completes as a one-shot step and subsequent `get_entity_by_name_ci` calls use it.
3. **Given** `knowledge_rebuild_from_wal` is invoked (fresh rebuild or recovery), **When** `Entity` rows are replayed, **Then** each replayed row's `lookup_key` is derived and correct, without requiring a separate backfill pass afterward.

---

### User Story 3 - Lookup resolution semantics are preserved exactly, including through tombstones and the authority guarantee (Priority: P1)

As code that depends on `get_entity_by_name_ci`'s exact current resolution semantics — including ADR-0283's requirement that a lookup resolve through `Merged`-labelled tombstones rather than skip them, ADR-0283's authority guarantee at `episode.rs`'s Site 1 (the "global fallback" that answers "does this entity exist anywhere in the group"), and #361's group-purge path — that behavior is unchanged after the index swap.

**Why this priority**: #369 (cross-group pointer resolution) depends on the current tombstone-resolution behavior; silently changing it would be a correctness regression disguised as a performance change. Likewise, losing the authority guarantee at Site 1 would silently reintroduce the exact false-negative bug ADR-0283 fixed, in a way no existing happy-path test would catch.

**Independent Test**: Create an entity, merge it into another (producing a `Merged`-tombstoned row), and confirm a name lookup for the tombstoned entity's name still resolves to the same winner as it did under `NameIndex`. Separately, run `knowledge_delete_by_group` for a group and confirm no stale/incorrect lookups survive for that group's purged entities. Separately, construct a scenario where Site 1's `lookup_key`-indexed entry is stale or missing for an entity that exists, and confirm Site 1 still correctly reports existence (via whatever mechanism replaces or preserves ADR-0283's bounded scan fallback).

**Acceptance Scenarios**:

1. **Given** an entity has been merged into another via `merge_entities` (leaving a `Merged`-tombstoned row), **When** `get_entity_by_name_ci` is called for that name, **Then** it returns the same winner (`ORDER BY created_at ASC, uuid ASC LIMIT 1` among matching rows, tombstones included) as the pre-change behavior.
2. **Given** a group has been purged via `knowledge_delete_by_group`, **When** `get_entity_by_name_ci` is called for a name that only existed in the purged group, **Then** no match is returned.
3. **Given** `episode.rs`'s Site 1 (the authority/global-fallback lookup) encounters a stale or missing `lookup_key` entry for an entity that does exist, **When** the lookup runs, **Then** it does not return a false "does not exist" answer — an equivalent guarantee to ADR-0283's bounded scan fallback and trust state is preserved.

---

### User Story 4 - The in-process map and its invalidation hooks are gone (Priority: P2)

As a maintainer reading the mutation paths that touch `Entity` (insert, `merge_entities`, `delete_by_source`, `delete_episode`, `clear_all`, `apply_corrections`, `rebuild_from_wal`, recovery, `knowledge_delete_by_group`), none of them contain `NameIndex`-invalidation logic anymore, because the database is now the sole source of truth for the lookup.

**Why this priority**: this is the cleanup the rest of the work exists to enable — removing the duplicated state and the risk of a missed invalidation site. It is lower priority than correctness/migration because it's a deletion, not new behavior, and should only happen once Stories 1-3 are verified.

**Independent Test**: Grep the codebase for `NameIndex` and confirm no references remain outside of history/comments explaining the prior design; confirm no mutation path contains index-invalidation calls.

**Acceptance Scenarios**:

1. **Given** the ART-index-backed lookup is serving all four call sites correctly, **When** the `NameIndex` type and its invalidation call sites are removed, **Then** the codebase compiles, and all mutation paths that previously invalidated `NameIndex` no longer reference it.

---

### Edge Cases

- **Raw Cypher writes bypass the host-side `lookup_key` write path — resolved per call site, not uniformly.** `handle_query_cypher` (the MCP `cypher` scope's arbitrary-query escape hatch) can insert or update `Entity.name`/`Entity.group_id` directly, without going through the Rust code that computes `lookup_key`; a second process writing the same database file directly has the same exposure. Unlike `NameIndex` (process-local, rebuilt at every service start and `rebuild_from_wal`), the `lookup_key` column is **persisted** — staleness from an out-of-band write does not self-heal on restart, it persists until something recomputes it. Per ADR-0038/ADR-0283's existing doctrine (mitigate where a miss is *wrong*, not merely *slow*): for the three call sites where a miss only degrades performance (Phase B dedup, Phase C's two per-edge lookups), this is an acceptable, documented limitation with no additional mitigation. For `episode.rs`'s Site 1 (the authority/"does this entity exist anywhere in the group" lookup), an equivalent guarantee to ADR-0283's bounded scan fallback and trust state MUST be preserved — see FR-010.
- **`CREATE ART INDEX` blocks during migration** — for a large existing graph, the one-shot backfill-and-build step has non-trivial, unmeasured duration; the migration step must not be mistaken for a fast/no-op upgrade path.
- **`knowledge_delete_by_group`'s purge path** must produce the same "no longer resolvable" outcome for `lookup_key`-indexed rows as it does today for `NameIndex` entries (#361's documented exception).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST add a materialized `lookup_key` column to `Entity`, computed host-side in Rust as the composite key `group_id + '\x1f' + lower(name)`, written in the same statement that writes `name`/`group_id` on every insert and update path that touches either field.
- **FR-002**: The system MUST create a secondary index on `lookup_key` via `CREATE ART INDEX ... FOR (e:Entity) ON (e.lookup_key)` with an explicit index type (`ART`) and an explicit, non-default index name (distinct from the PK index's default name).
- **FR-003**: `get_entity_by_name_ci` MUST query via `WHERE e.lookup_key = $key` equality, where `$key` is computed in Rust before the query runs — never via `lower()` inside Cypher.
- **FR-004**: The system MUST remove the in-process `NameIndex` structure and all of its invalidation call sites once the ART-index-backed lookup is verified to serve correct results, including the `knowledge_delete_by_group` / `group_purge` path (#361) and the WAL-stream and mutation-attribution changes from #378 and #385.
- **FR-005**: The system MUST provide a one-shot migration step that backfills `lookup_key` for every existing `Entity` row and builds the ART index, for databases created before this change.
- **FR-006**: `rebuild_from_wal` MUST derive and populate `lookup_key` for every replayed `Entity` row, so a rebuilt database has a fully correct column and index without requiring a separate backfill pass.
- **FR-007**: The ART-index-backed lookup MUST preserve current resolution semantics exactly, including resolving through `Merged`-labelled tombstones (ADR-0283, `db.rs:1659`) rather than filtering them out, so that cross-group pointer resolution (#369) is unaffected.
- **FR-008**: The four existing call sites in `crates/core/src/episode.rs` MUST see no behavior change — same signature, same semantics; only the lookup's internal access path changes.
- **FR-009**: The `lookup_key` column, being a divergence from graphiti's `ladybug_driver.py` schema, MUST be recorded deliberately (per this repo's schema-parity guard) rather than left as an undocumented gap.
- **FR-010**: For `episode.rs`'s Site 1 (the authority/global-fallback lookup covered by ADR-0283), the system MUST preserve a guarantee equivalent to ADR-0283's current bounded-scan-fallback-plus-trust-state behavior, so that a stale or missing `lookup_key` entry cannot cause a false "entity does not exist" answer for that site. The specific mechanism (e.g. bounded scan fallback, a trust flag, or recomputing `lookup_key` on read) is a Plan-stage design decision — this requirement is functional, not prescriptive.
- **FR-011**: For the three call sites where a miss degrades performance rather than correctness (Phase B dedup, and Phase C's two per-edge lookups), a stale or missing `lookup_key` entry for an out-of-band write (raw Cypher via the `cypher` MCP scope, or a second process writing the database directly) is an acceptable, documented limitation — no scan-fallback or self-heal mitigation is required for these three sites.
- **FR-012**: `knowledge_status`'s `name_index_trusted` and `name_index_fallback_scans` fields MUST either remain accurate observability for the ART-index design, or be replaced by an equivalently meaningful surface — they MUST NOT silently report stale-but-plausible values carried over from the `NameIndex` design.

### Key Entities

- **`Entity` (node table)**: gains a new `lookup_key` column (string), populated host-side from `group_id` and `name`, indexed by a secondary ART index.
- **`NameIndex` (removed by this work)**: the in-process `HashMap<(group_id, name_lower), BTreeSet<uuid>>` accelerator and its invalidation hooks across all `Entity`-mutating paths.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `EXPLAIN` on the query behind `get_entity_by_name_ci` shows `PRIMARY_KEY_SCAN_NODE_TABLE ... Index: ART`, not a full `ScanNodeTable` + `Filter`.
- **SC-002**: Measured lookup performance on a ≥10k-entity graph is at least as good as the `NameIndex` map it replaces.
- **SC-003**: No references to `NameIndex` or its invalidation logic remain in any `Entity`-mutating code path (insert, `merge_entities`, `delete_by_source`, `delete_episode`, `clear_all`, `apply_corrections`, `rebuild_from_wal`, recovery, `knowledge_delete_by_group`).
- **SC-004**: An existing (pre-change) database opens, migrates (`lookup_key` backfilled, ART index built), and serves correct lookups with no manual operator step beyond starting the upgraded service.
- **SC-005**: `cross_episode_dedup.rs`, `dedup_integration.rs`, `edge_endpoint_resolution.rs`, and the schema/WAL parity tests all pass with no behavior change.
- **SC-006**: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --release` are all green.
- **SC-007**: `episode.rs`'s Site 1 authority guarantee is verified equivalent to ADR-0283's: a test constructing a stale/missing `lookup_key` entry for an existing entity confirms Site 1 does not report false non-existence.
- **SC-008**: `knowledge_status` continues to expose observability for index staleness/trust that is accurate under the ART-index design (whether via the existing `name_index_trusted`/`name_index_fallback_scans` fields or a documented replacement).

## Assumptions

- `lbug` is pinned at `0.19.1` (`Cargo.toml`), and issue #190 (the version upgrade this issue was originally blocked on) is closed — this issue is no longer blocked on a pending lbug upgrade.
- `Entity`'s existing primary key remains `uuid`; `lookup_key` is a plain, non-unique secondary column.
- This is a Rust/lbug-side-only schema change; graphiti's Python-side schema is not modified.
- If measurement (SC-002) shows the ART index is not actually faster than the in-process map on a representative graph, the correct outcome is to keep `NameIndex` and close this issue without merging the replacement — the map is not being removed on principle.
- Mitigation for out-of-band `lookup_key` staleness follows the existing ADR-0038/ADR-0283 doctrine: mitigate where a miss is wrong (the Site 1 authority lookup), not where a miss is merely slower (the other three call sites). Because `lookup_key` is persisted (unlike `NameIndex`, which self-heals on restart or rebuild), staleness at the authority site is a permanent condition until healed by some mechanism — the choice of mechanism is deferred to the Plan stage.
- The `cypher` MCP scope is a documented, supported power-user escape hatch that bypasses the Rust write path; out-of-band writes through it (or through a second process) are an expected, not hypothetical, class of input this design must tolerate.

## Out of Scope

- The lbug version upgrade itself (already delivered by #190).
- Any change to the four call sites' calling code in `episode.rs` beyond what's needed to keep behavior identical.
- Adopting `ANALYZE`/planner statistics (tracked separately).
- The specific mechanism used to preserve the Site 1 authority guarantee (FR-010) — left to the Plan stage.

## Source References

- ADR-0038 (NameIndex, issue #219)
- ADR-0283 (scan fallback and trust state for NameIndex endpoint resolution, issue #283)
- `docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md`
- `crates/core/src/db.rs` (`NameIndex` field, `get_entity_by_name_ci`, `get_entity_by_name_ci_with_scan_fallback`, `scan_entity_by_name_ci`, `remove_episode`'s `group_purge` exception comment)
- `knowledge_status`'s `name_index_trusted` and `name_index_fallback_scans` fields (existing observability surface for `NameIndex` trust state)
- LadybugDB/ladybug#582 (non-PK secondary ART indexes)
- Issues #190 (lbug 0.19.1 upgrade, closed), #361 (group-scoped purge), #369 (resolvable semantic pointers), #378 (multi-stream WAL), #385 (mutation attribution fix)
