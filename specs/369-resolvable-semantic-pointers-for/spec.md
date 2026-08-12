# Feature Specification: Resolvable semantic pointers for cross-graph references

**Feature Branch**: `fabrik/issue-369`
**Created**: 2026-08-11
**Status**: Draft
**Input**: User description: "Cross-graph references must survive the source graph rebuilding or consolidating its nodes. Today they cannot, because a UUID reference is not an identity — it is a frozen cache of a past name resolution, and nothing marks it as such. In the hub topology (companion to #360), a layer graph's edges connect entities living in independently-hydrated source groups. Add a resolvable semantic pointer — carried additively alongside the existing UUID FK, in the edge's `attributes` JSON — for any edge endpoint that is foreign to the edge's own `group_id`, so that a source graph's re-extraction, merge, or purge-and-rehydrate cycle can be reconciled by re-resolving the pointer instead of silently going stale."

## Background

Today a cross-graph edge references its endpoint by UUID alone. That UUID is not an identity — it is a frozen cache of whatever name resolution happened to produce at write time — but nothing in the data model marks it as such, so nothing knows to revisit it. Four independent facts in the current implementation make that cache go stale silently rather than loudly:

- **A source graph re-extracts.** Entity UUIDs are minted with `Uuid::new_v4()` at insert (`crates/core/src/episode.rs:546`). A rebuild from source documents produces an entirely new generation of UUIDs for the same semantic nodes — the old UUID a cross-graph edge points at is not the UUID the re-extracted node now has.
- **WAL replication cannot renumber.** A WAL line is `{seq, ts, db, cypher, params}` (`crates/core/src/wal.rs:23`) — raw Cypher carrying literal UUIDs. A hub replaying a source's WAL replays exactly the UUIDs the source minted. There is no hub-side remapping seam; any fix has to happen at the source's mint site or in the referencing discipline of whatever holds the cross-graph edge.
- **Nothing deletes an `Entity`.** Only `Episodic` nodes are `DETACH DELETE`d (`db.rs:543`, `:559`, `:604`; `recovery.rs:164`, `:575`, each carrying an explicit "never `Entity` nodes" comment). Replaying a source's regenerated WAL therefore *adds* a second generation without removing the first — both generations co-reside, a cross-graph edge still addresses generation 1, and queries answer from generation 2. The failure is silent, not a dangling pointer.
- **Merge tombstones rather than removes.** `corrections::merge_entities` adds a `Merged` label to the alias row (`corrections.rs:1068`) and leaves it in place; nothing records which canonical it merged into. A reference to a merged-away alias still resolves to a row, so no integrity check fires — the reference has simply stopped meaning what it used to mean.

This matters specifically for the topology introduced alongside #360: N independent lcg instances, each a bounded context with its own `group_id`, publish WAL streams; a hub instance hydrates them all into one database and serves them over a single MCP channel; and on top of those hydrated graphs sits a **layer graph** — its own `group_id` in the same hub database — whose edges connect entities in group A to entities in group B. The layer is only as stable as its endpoints, and its endpoints live in graphs the layer has no authority over and no visibility into when they change.

**The governing rule this issue implements**: *a UUID reference is safe exactly where the referring graph is authoritative for the referred node's identity.* Within a group, everything that can invalidate a binding also gets to repair it in the same operation, under the same write lock, in one WAL seq stream — merge rewrites the edges it orphans, and endpoint resolution happens at commit (ADR-0051). Across groups that guarantee does not hold: the invalidating event happens upstream, in an instance that has never heard of the referrer. So:

- **Intra-group references keep UUID FKs, unchanged.** This is the efficiency case and the overwhelming majority of edges; the two-hop `Entity → RelatesToNode_ → Entity` read pattern must not get slower or more complex for them.
- **Cross-group references gain a resolvable semantic pointer**, carried *additively* alongside the UUID FK in the existing `attributes` JSON column (no schema migration): `source_group_id` and `endpoint_name` are the assertion ("which graph, what name"); `resolved_uuid`, `bound_at_seq`, and `binding_state` are the cache (bound / unbound / ambiguous). The UUID FK itself is demoted from identity to cache, but it stays a real FK, so traversal cost for a *resolved* cross-group edge is unchanged.

**Why this diverges from ADR-0051.** ADR-0051 made an unresolvable edge endpoint a hard drop at commit (`episode.rs:757`) — correct for ingest, where the extractor named something that plain does not exist. It is wrong here: for a cross-group pointer, unresolvable usually means *the source graph is mid-rebuild*, not that the assertion is false. Cross-group edges therefore need a third state — `unbound` — and must be exempt from ADR-0051's drop. That state must not be conflated with `invalid_at` (temporal validity — "this fact stopped being true"): "the source retracted this" and "the source is rebuilding" are different facts, and the whole reason this layer exists is to keep them distinguishable.

**Why resolution must reuse the name index, not reimplement it.** `episode.rs:748` already binds extracted edges via `name_to_uuid.get(&normalize_name(...))` with a `resolve_via_scan` fallback (ADR-0283); `db.rs:1219` documents why that scan fallback deliberately mirrors the index's winner rule, including *not* filtering `Merged` tombstones — the two must never disagree about what "resolves" means. A pointer resolver is a second caller of that same authority (`get_entity_by_name_ci_with_scan_fallback`), not a second implementation of it.

**Relationship to #361 (group-scoped purge) — the refresh cycle this issue exists to survive.** The two-hop model already gives `unbound` a natural physical representation: a layer edge is `Entity(A) -[:RELATES_TO]-> RelatesToNode_(L) -[:RELATES_TO]-> Entity(B)`, where the `RelatesToNode_` (carrying the layer's fact, its own `group_id`, and — per this issue — its pointer fields) belongs to group L, not to A or B. A group-scoped purge of A deletes `Entity(A)` and, via `DETACH DELETE`, the `Entity(A) → RelatesToNode_(L)` hop rel — but must not delete `RelatesToNode_(L)` itself, which is L's data, not A's. So the layer's assertion survives a purge of its own endpoint for free; what's lost is exactly the hop, which is exactly the binding. Re-bind is therefore: re-resolve the pointer, re-create the missing hop rel — no new schema, no resurrection of deleted rows. The intended operational cycle is *purge group A entirely → rehydrate from new WAL → re-bind every cross-group pointer into A*, which makes the transient `unbound` window a **routine part of every refresh**, not an exceptional case — FR-004/FR-005 below are load-bearing for that normal path. This sets a contract on #361 that this issue's requirements state explicitly (FR-011): purge must not delete a foreign group's `RelatesToNode_` nodes, and must leave affected cross-group edges `unbound` rather than dangling. Sequencing follows from that contract: #361 produces a state this issue defines, so this issue lands first, and #361's current invariant ("no edge crosses a group boundary, so removing a group's nodes cannot orphan another group's data") becomes conditional once this issue lands.

**Relationship to #360 (per-source applied positions).** Not a hard dependency — the pointer design is testable today in a single instance with several `group_id`s. FR-007's re-bind staleness check wants the per-source applied positions #360 introduces; against today's `'singleton'` `WalPosition` the same check is only per-database (coarser, but workable as a first cut).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Cross-group edges carry pointer fields; intra-group edges don't (Priority: P1)

When an edge is written whose endpoints are not all within the edge's own `group_id`, it carries pointer fields (`source_group_id`, `endpoint_name`, `resolved_uuid`, `bound_at_seq`, `binding_state`) for each foreign endpoint. An edge wholly within one group is completely unaffected — no new fields, no new cost.

**Why this priority**: This is the foundational data-model change everything else in this issue depends on, and it is also what keeps the promise that intra-group traversal (the overwhelming majority of edges) never gets slower.

**Independent Test**: Insert an edge with both endpoints in one group and confirm no pointer fields are added; insert an edge with an endpoint in a different group and confirm the pointer fields are present and populated for that endpoint.

**Acceptance Scenarios**:

1. **Given** an edge whose two endpoints share the edge's own `group_id`, **When** it is inserted, **Then** no pointer fields are added and its `attributes` payload and insert cost are unchanged from today.
2. **Given** an edge with one or more endpoints in a different `group_id`, **When** it is inserted with pointer fields supplied for every foreign endpoint, **Then** it is accepted and the pointer fields are persisted in `attributes`.
3. **Given** an attempt to insert a cross-group edge that omits pointer fields for a foreign endpoint, **When** the insert is attempted, **Then** it is rejected loudly and no partial edge is written.

---

### User Story 2 - Pointer resolution agrees with the name index, including on ambiguity (Priority: P1)

Resolving (or re-resolving) a cross-group pointer produces exactly the outcome the existing name-index resolution path (`get_entity_by_name_ci_with_scan_fallback`, ADR-0283) would produce for the same `(group_id, name)` — including recognizing when more than one candidate exists, rather than silently applying the index's winner rule.

**Why this priority**: The entire feature depends on there being exactly one notion of "what this name resolves to" per source group. A second implementation risks disagreeing with the name index, which is precisely the hazard `db.rs:1219` documents.

**Independent Test**: Seed a source group with zero, one, and two-or-more entities under a given normalized name; resolve a pointer against each and confirm the outcome (unbound / bound / ambiguous) matches what the name-index path independently reports for the same query.

**Acceptance Scenarios**:

1. **Given** a source group with exactly one entity matching the pointer's normalized `endpoint_name`, **When** resolution runs, **Then** `resolved_uuid` is set, `binding_state` becomes `bound`, and `bound_at_seq` is set to the source's applied position at resolution time.
2. **Given** a source group with no entity currently matching the normalized name (e.g. mid-rebuild), **When** resolution runs, **Then** `binding_state` becomes `unbound`, no `resolved_uuid` is asserted, and the edge is retained rather than dropped — diverging from ADR-0051's commit-time drop.
3. **Given** a source group with two or more entities matching the normalized name, **When** resolution runs, **Then** `binding_state` becomes `ambiguous` rather than silently taking the name index's `ORDER BY created_at ASC, uuid ASC LIMIT 1` winner.
4. **Given** a source group where the previously-resolved entity has since been merged into a canonical (tombstoned with the `Merged` label), **When** resolution runs, **Then** it resolves through the tombstone exactly as the name index does, landing on the canonical — consistent with `db.rs:1219`.

---

### User Story 3 - Refresh cycle: purge, rehydrate, re-bind (Priority: P1)

An operator or automated process fully refreshes one source group — purges its subgraph, rehydrates it from new WAL — and re-binds every cross-group pointer into that group. The result: every layer edge into that group is correctly bound, or explicitly `unbound`/`ambiguous` where the source genuinely no longer contains a match.

**Why this priority**: This is the concrete end-to-end scenario the feature exists to support. Without it, the pointer mechanism from User Stories 1–2 is inert data with no repair path.

**Independent Test**: Build a layer graph spanning two groups, fully purge and rehydrate one group with a changed entity set (a rename, a merge, a dropped entity, and a full re-extraction under new UUIDs), run the re-bind pass, and confirm every affected layer edge lands in the correct state.

**Acceptance Scenarios**:

1. **Given** a source group re-extracted with an entirely new generation of entity UUIDs for the same semantic content, **When** the re-bind pass runs after rehydration, **Then** every layer edge into that group resolves to the new generation's UUIDs.
2. **Given** a group-scoped purge that deletes `Entity(A)` and, via `DETACH DELETE`, its hop rel to a `RelatesToNode_` belonging to a different group `L`, **When** the purge completes, **Then** `RelatesToNode_(L)` itself and its pointer fields are untouched — only the hop rel is gone.
3. **Given** a re-bind pass has just completed with no unresolved changes remaining, **When** it is run again with no intervening source change, **Then** it is a no-op — no already-bound pointer is altered (idempotency).
4. **Given** a re-bind pass is triggered while a source is only partially rehydrated, **When** it runs, **Then** it does not corrupt state, and pointers whose target has not yet arrived are correctly left `unbound`.
5. **Given** re-binding would cause two previously-distinct cross-group pointers to resolve onto the same canonical entity, **When** the re-bind pass runs, **Then** it applies the same self-loop/duplicate handling the merge path already uses rather than a new policy.

---

### User Story 4 - Unbound and ambiguous edges are observable and don't break read paths (Priority: P2)

Traversal, search, and MCP responses behave predictably when a cross-group edge is `unbound` or `ambiguous`, and an operator can see how many pointers are currently in each non-bound state via `knowledge_status`.

**Why this priority**: Without this, the feature is invisible until something downstream breaks in a confusing way. The issue itself calls this the requirement "most likely to be under-scoped" because it surfaces in more places than the write path.

**Independent Test**: Create a mix of bound, unbound, and ambiguous cross-group edges; query them through normal traversal/search/MCP paths and through `knowledge_status`; confirm behavior matches the documented default and the counts are correct.

**Acceptance Scenarios**:

1. **Given** a layer edge whose foreign-endpoint pointer is `unbound` (no hop rel to that side exists), **When** a normal two-hop traversal query runs, **Then** the edge does not appear in results — consistent with today's inner-join-shaped read pattern — and the query does not error.
2. **Given** one or more `unbound` and/or `ambiguous` cross-group pointers exist, **When** `knowledge_status` is called, **Then** it reports a count for each state.
3. **Given** a pointer transitions from `unbound` to `bound` via a re-bind pass, **When** the same traversal query is repeated, **Then** the edge now appears in results.

---

### Edge Cases

- **Names are not unique, even within a group.** `get_entities_by_name_all` returns a list; the deterministic winner rule (`ORDER BY created_at ASC, uuid ASC LIMIT 1`) is exactly the mechanism by which a naive pointer would silently re-target — which is why `ambiguous` exists as its own state rather than always taking the winner.
- **Merge mutates `created_at`.** Merging sets the canonical's `created_at` to the earliest across all merged aliases (`corrections.rs:1092`) — the primary sort key of the winner rule. Name resolution is therefore stable in practice but not immutable: a merge elsewhere in the source group can change which row a name resolves to, without any name itself changing.
- **Source graph renames a node.** Neither the pointer nor any UUID scheme survives a rename on its own. The defined behavior is: the pointer becomes `unbound` (the old name no longer resolves), surfaced for a re-bind pass or human/agent resolution — not silently left pointing at a stale UUID and not silently dropped.
- **Transitive merge chains.** `merge_entities` refuses a `Merged` entity as a canonical (`corrections.rs:961`), so merge chains only grow alias-ward, not canonical-ward — a resolver following the existing name-index path inherits this guarantee and does not need its own cycle guard for merge chains specifically.
- **Self-referential and duplicate cross-group edges during re-bind.** Re-binding two pointers onto the same canonical entity can produce a self-loop or a duplicate edge; this is handled by reusing the merge path's existing handling (User Story 3, Acceptance Scenario 5), not a new policy.
- **`to_seq`-bounded or partial hydration mid-progress.** A re-bind pass triggered before a source has fully caught up must leave not-yet-resolvable pointers `unbound` rather than erroring or misbinding (User Story 3, Acceptance Scenario 4).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: An edge whose endpoints are not all in the edge's own `group_id` MUST carry pointer fields for each foreign endpoint. Intra-group edges are unchanged and MUST NOT pay any new cost.
- **FR-002**: The insert path MUST reject a cross-group edge that lacks pointer fields. Without this, the invariant silently degrades the first time an edge is written through a code path that doesn't know about pointers, and a bare cross-group UUID FK becomes indistinguishable at rest from an intra-group one.
- **FR-003**: Pointer resolution MUST use the same resolution rule and code path as the name index (`get_entity_by_name_ci_with_scan_fallback` / ADR-0283), not a reimplementation.
- **FR-004**: A cross-group edge whose pointer does not currently resolve MUST be retained in an `unbound` state, not dropped. It MUST be exempt from ADR-0051's commit-time endpoint drop.
- **FR-005**: `unbound` MUST be represented independently of `invalid_at`.
- **FR-006**: A pointer resolving to more than one candidate MUST be recorded as `ambiguous` rather than silently taking the name-index winner.
- **FR-007**: Re-binding MUST be triggerable after hydration, and MUST use the persisted applied position (per-source, once #360 lands) to skip pointers whose source has not advanced.
- **FR-008**: Read paths, search, and MCP responses MUST have defined behaviour for `unbound` and `ambiguous` cross-group edges.
- **FR-009**: Re-binding MUST be idempotent and safe to run repeatedly, including mid-hydration.
- **FR-010**: The purge → rehydrate → re-bind refresh cycle MUST be supported end to end for a single group, leaving every cross-group pointer into that group correctly re-bound, or explicitly `unbound`/`ambiguous` where the source no longer contains a match.
- **FR-011**: A group-scoped purge MUST NOT delete `RelatesToNode_` nodes belonging to another `group_id`. Removing the hop rel to a purged entity is correct; removing the foreign edge node is not. (This constrains #361's purge implementation; this issue defines the contract and the repair pass, not the purge mechanism itself — see Out of Scope.)
- **FR-012**: `knowledge_status` (or equivalent) MUST be able to report the count of currently unbound cross-group pointers, so a refresh in progress is observable.

### Key Entities *(if the feature involves data)*

- **Cross-group pointer**: additive metadata carried in a cross-group edge's `attributes` JSON, one instance per foreign endpoint. Fields: `source_group_id` (assertion — which graph the endpoint lives in), `endpoint_name`, normalized (assertion — what it is called there), `resolved_uuid` (cache — result of the last resolution), `bound_at_seq` (cache — source WAL position at bind time), `binding_state` (cache — one of `bound` / `unbound` / `ambiguous`).
- **`binding_state`**: a tri-state axis orthogonal to `invalid_at`. `bound` = pointer currently resolves to exactly one entity; `unbound` = no current match (source likely mid-rebuild or the node was renamed/removed); `ambiguous` = more than one candidate currently matches.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A source graph fully re-extracted (new generation of `uuid_v4` entity UUIDs) leaves every layer edge pointing at the correct current entity after a re-bind pass.
- **SC-002**: A merge in a source graph leaves layer edges pointing at the canonical, with no layer assertion destroyed.
- **SC-003**: A layer edge whose source graph is mid-rebuild is reported as `unbound`, is distinguishable from a retracted fact, and re-binds automatically when the source completes.
- **SC-004**: Intra-group traversal performance is unchanged — no additional resolution hop on the read path for edges wholly within one group.
- **SC-005**: Attempting to write a cross-group edge without pointer fields fails loudly.
- **SC-006**: A full purge-and-rehydrate of one source group, with a layer graph on top, restores every layer edge to a correct binding — and no layer edge is lost to the purge.

## Assumptions

- **Read-path default for non-bound edges**: an `unbound` or `ambiguous` cross-group edge is excluded from normal traversal/search/MCP query results, matching today's inner-join-shaped two-hop read pattern unchanged — rather than surfaced inline with a special marker. It remains observable in aggregate via `knowledge_status` (FR-012). This is the default the issue itself calls "probably right," made explicit here rather than left implicit; a dedicated inspection surface for listing *which* specific pointers are non-bound (beyond the aggregate counts) is left to Plan-stage discretion (see Out of Scope).
- **`knowledge_status` reports both non-bound states**: since FR-008 requires defined behavior for both `unbound` and `ambiguous`, observability (FR-012) is assumed to cover both — i.e. `knowledge_status` exposes counts for each state, not `unbound` alone, so an ambiguous backlog is equally visible.
- **Re-bind's trigger surface** (an explicit on-demand admin/MCP operation, an automatic hook into the hydration path, or both) is a Plan-stage design decision. This spec requires only that a re-bind pass be triggerable after hydration (FR-007) and cheap enough to invoke repeatedly or automatically (FR-009's idempotency plus the `applied_seq` staleness check).
- **Ambiguous pointers are re-evaluated on every re-bind pass** just like unbound ones, using the same staleness check — `ambiguous` is not treated as a terminal state requiring a separate manual-only resolution path.
- Not a hard dependency on #360: the design is fully testable in a single instance with multiple `group_id`s. Where #360 has not landed, FR-007's staleness check operates on the coarser per-database `'singleton'` `WalPosition` rather than a per-source one — a workable first cut, not a blocker.
- Transitive merge chains do not require a cycle guard in the pointer resolver specifically, because `merge_entities` already refuses a `Merged` entity as a canonical (`corrections.rs:961`), so chains only grow alias-ward and the existing name-index path already terminates.

## Out of Scope

- **Deterministic (`uuid_v5`) entity UUID minting at the source.** Considered and rejected: it does not survive purge-and-rehydrate (which destroys the `Entity → RelatesToNode_` hop regardless of the endpoint's subsequent UUID), does not address merge/rename/ambiguity, and changes identity for every existing graph.
- **A `merged_into` forwarding pointer on merged aliases.** Would make merge auditable and let a stale binding resolve forward without a name lookup, but is not required here — name resolution already resolves through `Merged` tombstones today. Worth filing separately.
- **Implementing #361's group-scoped purge mechanism itself.** This issue defines the contract purge must respect (FR-011) and the repair pass that runs after it (FR-007/FR-009/FR-010); the purge implementation lives in #361.
- **Full per-source `WalPosition` granularity.** That is #360's scope; this issue works with the coarser per-database applied position where #360 has not yet landed (see Assumptions).
- **A dedicated query/inspection surface for listing which specific pointers are unbound or ambiguous**, beyond the aggregate counts `knowledge_status` reports. May be added at Plan/implementation's discretion.

## Source References

- `crates/core/src/episode.rs:546` (UUID minting), `:748` (name-index edge binding), `:757` (ADR-0051 commit-time drop)
- `crates/core/src/wal.rs:23` (WAL line shape)
- `crates/core/src/db.rs:543`, `:559`, `:604` (`Episodic`-only `DETACH DELETE`), `:1219` (why the scan fallback must not diverge from the index's `Merged`-tombstone behavior)
- `crates/core/src/recovery.rs:164`, `:575` (same `Episodic`-only deletion discipline in recovery paths)
- `crates/core/src/corrections.rs:961` (merge refuses a `Merged` canonical), `:1068` (`Merged` label on alias), `:1092` (merge sets canonical `created_at` to earliest)
- `crates/core/src/schema.rs:88` (`WalPosition.applied_seq`)
- [ADR-0051](../../docs/adr/0051-edge-endpoint-salvage-and-deferred-drop.md) — commit-time endpoint drop, diverged from here for cross-group pointers
- [ADR-0283](../../docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md) — the name-index resolution authority this issue's pointer resolver must reuse
- [ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md) — `WalPosition.applied_seq`, the staleness-check primitive FR-007 depends on
- #360 — per-source WAL hydration (companion topology; not a hard dependency, see Assumptions)
- #361 — group-scoped purge (the mechanism this issue's refresh cycle exists to survive; FR-011 constrains it)
- #368 — prerequisite entity-merge/cross-group edge fix (closed)
