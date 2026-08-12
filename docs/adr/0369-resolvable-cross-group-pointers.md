# ADR-0369: Resolvable Semantic Pointers for Cross-Group Edges

**Status**: Accepted
**Date**: 2026-08-11
**Issue**: #369

## Context

A `RelatesToEdge`'s `group_id` need not match either endpoint entity's `group_id` — ADR-0368
already established this shape for entity merge's edge rewriting. The hub topology this issue
targets (companion to #360/#361) makes that shape deliberate rather than incidental: N
independently-hydrated source `group_id`s live in one hub database, and a **layer graph** — its
own `group_id` — has edges connecting entities across two source groups.

Today, a cross-group edge references its endpoints by UUID alone. That UUID is not an
identity — it is a frozen cache of whatever name resolution happened to produce at write time —
but nothing in the data model marks it as such:

- Entity UUIDs are minted with `Uuid::new_v4()` at insert (`episode.rs`); a source's
  re-extraction produces an entirely new generation of UUIDs for the same semantic content.
- Nothing deletes an `Entity` today (`db.rs`/`recovery.rs` `DETACH DELETE` only `Episodic`
  nodes, each with an explicit "never `Entity`" comment) — a re-hydrated source's new generation
  co-resides with the old one rather than replacing it.
- `corrections::merge_entities` tombstones an alias (`Merged` label) rather than removing it,
  and does not record which canonical it merged into.

Each of these silently strands a cross-group UUID reference: the edge still addresses a UUID
that used to mean something, and nothing revisits it. Within one group, this doesn't happen —
everything that can invalidate a binding also repairs it, in the same operation, under the same
write lock (merge rewrites the edges it orphans; extraction-time endpoint resolution happens at
commit, see ADR-0051). Across groups that guarantee does not hold: the invalidating event
happens upstream, in an instance that has never heard of the referrer.

## Decision

**Cross-group edges gain a resolvable semantic pointer, carried additively in the existing
`RelatesToNode_.attributes` JSON column — no schema migration.** Intra-group edges
(`db::insert_relates_to_edge`, the hot path used by every extraction-time insert) are left
byte-for-byte unchanged; this feature adds a second, parallel write path rather than modifying
the first.

### Data shape

A `RelatesToNode_.attributes` value may carry a `cross_group_pointers` key:

```json
{
  "cross_group_pointers": {
    "src": { "source_group_id": "...", "endpoint_name": "...", "resolved_uuid": "...", "bound_at_seq": 12, "binding_state": "bound" },
    "dst": { "source_group_id": "...", "endpoint_name": "...", "resolved_uuid": null, "bound_at_seq": 12, "binding_state": "unbound" }
  }
}
```

`src`/`dst` are each present only when that endpoint is foreign to the edge's own `group_id`; an
intra-group edge carries neither, and its `attributes` payload is untouched by this feature.
`source_group_id`/`endpoint_name` are the assertion ("which graph, what name" — normalized via
the same `normalize_name` extraction uses); `resolved_uuid`/`bound_at_seq`/`binding_state` are
the cache produced by the last resolution. The `RELATES_TO` rel's own `attributes` copy is never
written for pointer data — reads never consult it (`schema.rs`'s comment: "reads always pull
those from the RelatesToNode_ node"), so mirroring would be pure write cost for zero benefit.

`RelatesToEdge.source_node_uuid`/`target_node_uuid` keep their existing `String` type — no
`Option<String>` widening, which would force touching every constructor call site in the
codebase (the class of break this repo's `CLAUDE.md` calls out from #46/#58). An empty string is
the sentinel for "this side is not currently bound": never a valid entity UUID, so
`MATCH {uuid: ""}` naturally binds nothing.

### `binding_state`: a tri-state independent of `invalid_at`

- **`bound`** — resolves to exactly one entity.
- **`unbound`** — no entity currently matches. For a cross-group pointer this usually means the
  source is mid-rebuild, not that the assertion is false — the opposite of ADR-0051's ingest-time
  assumption, where an unresolvable name means the extractor named something that plainly
  doesn't exist. Cross-group edges are therefore exempt from ADR-0051's commit-time drop: an
  `unbound` pointer is retained, not dropped.
- **`ambiguous`** — more than one entity currently matches. The name index's own resolution path
  (`get_entity_by_name_ci_with_scan_fallback`) silently applies a deterministic winner rule
  (`ORDER BY created_at ASC, uuid ASC LIMIT 1`) for exactly this case; a pointer must not
  reproduce that silent behavior, since a wrong winner here binds a layer assertion to the wrong
  entity with no visible sign anything is off.

`unbound`/`ambiguous` are deliberately not folded into `invalid_at` — "the source retracted this
fact" and "the source is rebuilding, or this name is currently ambiguous" are different claims,
and the entire reason this pointer layer exists is to keep them distinguishable.

### Resolution reuses the name index, never reimplements it

`resolve_endpoint` (`cross_group.rs`) is a second caller of
`get_entity_by_name_ci_with_scan_fallback` (ADR-0283) — the same authority extraction-time
endpoint resolution uses — not a parallel implementation. Reusing it means a pointer can never
disagree with the index about what a name resolves to, the exact hazard `db.rs`'s scan-fallback
comment warns about.

Ambiguity detection could not simply add a `count > 1` check using the pre-existing
`count_entities_by_name_ci` (an unfiltered row count, built for dedup-regression tests) — it
counts `Merged`-tombstoned rows as separate candidates, which would report `ambiguous` for a
name that has already been correctly merged down to one canonical (contradicting the intended
behavior: a pointer must resolve *through* a `Merged` tombstone to the canonical, exactly as the
index does). A new method, `count_active_entities_by_name_ci`, excludes `Merged` rows — mirroring
how `corrections::merge_entities` itself already filters them in Rust after the fetch, rather
than a Cypher-side label-list predicate. `resolve_endpoint` calls the winner lookup first (so the
common single-match path benefits from the index's self-heal/trust behavior), then checks the
active count only when a winner exists.

A winner that is itself `Merged`-tombstoned is not automatically `bound`, either. The
tombstone-resolves-to-canonical case above only holds when the alias and canonical share the
same name — merge backdates the canonical's `created_at` to the earliest across all merged
aliases (`corrections.rs:1092`), so the canonical wins the winner-selection tie-break. When a
merge also changes the resolvable name — the common case, since merges typically consolidate
name variants rather than literal duplicates — no live row matches the alias's old name anymore,
so the winner returned by `get_entity_by_name_ci_with_scan_fallback` is the `Merged`-labelled
alias itself, and `count_active_entities_by_name_ci` correctly reports 0 active matches (not
`> 1`). Left unchecked, this would silently report `bound` to a tombstone with no active edges,
indistinguishable from a healthy binding. `resolve_endpoint` therefore checks the winner's own
labels after the ambiguity check and reports `Unbound` if the winner itself carries `Merged` —
the same outcome this issue's model already defines for a plain rename.

### Two-hop writes are split into independent, idempotent statements

`insert_relates_to_edge`'s existing shape is a single `MATCH`-both-endpoints `CREATE` per hop —
correct today, but a `MATCH` that fails to bind silently creates zero rows, so it cannot express
"create the resolved side's hop, leave the unresolved side absent." `insert_cross_group_edge` is
a new, parallel function: it creates the `RelatesToNode_` unconditionally, then creates each hop
independently, gated on that side's UUID being non-empty, using `MERGE` (not `CREATE`) —
following `dump.rs`'s existing two-hop `MERGE` precedent. `MERGE` is what makes re-running a hop
creation safe, which is what makes `rebind_pointers` idempotent.

Because every existing two-hop read (`get_full_edges_for_entity`, `get_edges_for_entity`,
`has_directed_edge`, `get_edge_by_uuid`) is an inner-join-shaped `MATCH` requiring both hops, an
edge with a missing hop is already invisible to every one of them — this is a consequence of the
existing read shape, not new code, so read-path behavior for `unbound`/`ambiguous` edges (User
Story 4) needed only regression tests, not a code change.

### Re-bind

`rebind_pointers(conn, source_group_id, ts)` re-resolves every pointer whose `source_group_id`
matches, meant to run after that source's own hydration/refresh cycle:

- **Staleness gate and idempotency are the same mechanism.** A pointer is skipped when
  `bound_at_seq >= WalPosition.applied_seq` (the DB-wide singleton from ADR-0353 — #360's
  per-source position hasn't landed, so this is coarser but workable, per the spec's own
  accepted first cut). Every pointer this call *does* touch has its `bound_at_seq` bumped to the
  current applied position, so a second call with no intervening WAL activity is a true no-op.
- **Self-loop and duplicate handling is not a new policy.** A resolution change that would make
  an edge's two sides equal, or duplicate an edge `has_directed_edge` already reports (scoped to
  the edge's own `group_id`, per ADR-0368), invalidates the whole edge via `invalidate_edge` —
  the identical pattern `corrections::merge_entities_inner` already uses when merge produces the
  same shapes.
- **A stale hop is deleted before the new one is created**, so a transition out of `bound`
  (rename, purge, now-ambiguous) doesn't leave a dangling hop pointing at a name that no longer
  matches.

### FR-011 (purge must not delete a foreign group's `RelatesToNode_`) needed no new code

No purge mechanism exists yet (#361, open); nothing in the codebase deletes a `RelatesToNode_`
today. The contract is a regression test (`crates/core/tests/cross_group_pointers.rs`) that
simulates a purge by hand — deleting only the purged group's `Entity` rows, never
`RelatesToNode_` — so #361's eventual purge implementation has a tripwire if it ever adds one.

## Consequences

### Positive

- Intra-group traversal and insert cost are provably unchanged (`insert_relates_to_edge` is
  untouched; every existing two-hop read query is untouched) — SC-004 holds by construction, not
  by new benchmarking.
- A source's re-extraction, merge, or rename is now a reconcilable event (re-run
  `knowledge_rebind_pointers`) instead of a silent staleness that nothing detects.
- `unbound`/`ambiguous` are observable in aggregate via `knowledge_status`, so an in-progress
  refresh — expected to leave a transient window of unbound pointers on every purge/rehydrate
  cycle — is visible without a dedicated inspection endpoint.
- Ambiguity detection cannot silently diverge from the name index's own resolution rule, because
  it composes the index's own methods rather than a hand-rolled query.

### Negative / Residual risks

- `count_cross_group_pointers`/`rebind_pointers`'s candidate scan is a `CONTAINS`-prefiltered
  full scan of `RelatesToNode_`. Acceptable today (admin/status-triggered, not the hot path;
  cross-group edges are expected to stay a small minority), but will need an index or dedicated
  column if that stops being true.
- FR-011's regression coverage simulates a purge by hand; #361's actual purge implementation may
  encode assumptions this hand-simulation gets wrong, and should add its own regression test
  against a real purge once it lands.
- FR-007's staleness check is per-database, not per-source, until #360 lands — a WAL-driven
  change to any group bumps the one singleton position, so a rebind pass's "nothing changed"
  no-op is coarser than it will eventually be.
- `knowledge_add_cross_group_edge` is the only creation path for this edge shape; extraction
  (`episode.rs`) is untouched and has no notion of cross-group edges. A future extractor that
  wants to emit cross-group edges directly would need new integration work, not just a config
  flag.

## Alternatives Considered

### Deterministic (`uuid_v5`) entity UUID minting at the source

Rejected: does not survive purge-and-rehydrate, which destroys the `Entity → RelatesToNode_` hop
regardless of the endpoint's subsequent UUID scheme; does not address merge, rename, or
ambiguity; and changes identity semantics for every existing graph, not just cross-group edges.

### Widen `RelatesToEdge.source_node_uuid`/`target_node_uuid` to `Option<String>`

Rejected: forces every constructor call site across the codebase to change — the exact class of
break this repo's `CLAUDE.md` documents from #46/#58 — for a benefit (type-level
non-nullability) an empty-string sentinel and a code comment cover just as well here, since a
valid entity UUID is never empty.

### A `merged_into` forwarding pointer on merged aliases

Would make merge auditable and let a stale binding resolve forward without a name lookup. Not
required: name resolution already resolves through `Merged` tombstones today via the shared
winner-selection order. Left as a candidate for a separate issue.

### Restructure `insert_relates_to_edge` itself to support partial-hop creation

Rejected: it is the hottest write path in the codebase (every extraction-time edge insert), and
any restructuring risks regressing its all-endpoints-exist, single-statement-per-hop
performance/atomicity for the overwhelming majority (intra-group) case. A new, parallel function
(`insert_cross_group_edge`) fully avoids that risk instead of mitigating it.

## Related

- `crates/core/src/pointer.rs` — `BindingState`, `EndpointSide`, `CrossGroupPointer`,
  `read_pointers`/`write_pointers`.
- `crates/core/src/cross_group.rs` — `EndpointSpec`, `resolve_endpoint`,
  `create_cross_group_edge`, `rebind_pointers`.
- `crates/core/src/db.rs` — `insert_cross_group_edge`, `create_relates_to_hop`,
  `delete_relates_to_hop`, `update_relates_to_attributes`, `count_active_entities_by_name_ci`,
  `count_cross_group_pointers`, `list_cross_group_pointer_candidates`.
- `crates/core/src/handlers.rs` — `handle_add_cross_group_edge`, `handle_rebind_pointers`,
  `handle_knowledge_status`'s `cross_group_pointers` field.
- `crates/service/src/mcp/tools.rs` — `knowledge_add_cross_group_edge` (write),
  `knowledge_rebind_pointers` (admin).
- `crates/core/tests/cross_group_pointers.rs`, `crates/core/tests/ipc_parity.rs` — coverage for
  all four user stories.
- [ADR-0051](0051-edge-endpoint-salvage-and-deferred-drop.md) — the commit-time endpoint drop
  cross-group edges are explicitly exempt from.
- [ADR-0283](0283-name-index-scan-fallback-for-endpoint-authority.md) — the resolution authority
  this issue's pointer resolver reuses.
- [ADR-0353](0353-persist-and-expose-applied-wal-seq.md) — `WalPosition.applied_seq`, the
  staleness-check primitive re-bind depends on.
- [ADR-0368](0368-group-scoped-edge-dedup-in-merge.md) — the precedent for group-scoped
  dedup/self-loop handling this issue's re-bind pass reuses, and for an edge's `group_id`
  legitimately differing from its endpoints'.
- `specs/369-resolvable-semantic-pointers-for/spec.md` — this issue's spec.
- #360 — per-source WAL hydration (companion topology; FR-007's staleness check would gain
  per-source granularity once it lands).
- #361 — group-scoped purge; this ADR's FR-011 contract constrains its eventual implementation.
