# ADR-0379: Direct Assertion API Conventions

**Status**: Accepted
**Date**: 2026-08-12
**Issue**: #379

## Context

`knowledge_assert_entity` / `knowledge_assert_relationship` add a direct write path so an agent
that already knows a fact can record it without a prose round-trip through episode extraction.
This issue supersedes #179 (closed unimplemented on 2026-06-24), whose edge-upsert match ignored
`group_id` — exactly the shape ADR-0368 identified as unsafe — and whose relationship-endpoint
resolution left cross-group refusal as an implicit consequence of the algorithm rather than a
stated requirement.

Several decisions were made during Plan and Implement that are not obvious from the spec's FRs
alone, plus two the engine forced during implementation that the spec did not anticipate. Both
kinds are recorded here so a future reader doesn't have to reconstruct them from `git blame`.

## Decisions

### 1. Resolution logic lives in a new `assert.rs`, sharing only the forward-walk with `cross_group.rs`

`cross_group::resolve_endpoint`'s `Unbound` state conflates two outcomes FR-009/FR-010 require to
be distinguishable: *no entity with this name exists at all* (assert should create one) and *a
`Merged` tombstone was found but its forward walk dead-ends* (assert must hard-error, since the
name resolves to *something* and guessing would silently duplicate it). `resolve_endpoint`'s own
caller, `rebind_pointers`, can afford to conflate them — a pointer that comes back `Unbound` is
simply retried on the next refresh pass. An assert call has no such retry.

Rather than extending `resolve_endpoint`'s return type (which would ripple into
`rebind_pointers`), the cycle-guarded `merged_into` forward walk itself was extracted into
`cross_group::follow_merged_into_chain(conn, group_id, start) -> Result<Option<EntityRow>, Error>`.
`resolve_endpoint` calls it and maps `None` to `Unbound`; `assert::resolve_entity_by_name` /
`resolve_entity_by_uuid` call the same function and map `None` to a hard `Error::Ipc`. One
implementation of the walk, two miss-handling policies at the two call sites.

### 2. `valid_at` is scoped to `knowledge_assert_relationship` only

The spec's FR-022 as originally written asked both tools to accept `valid_at`. `Entity` has no
`valid_at` column (`schema.rs`: `uuid, name, group_id, labels, created_at, name_embedding,
summary, attributes`) — only `RelatesToNode_` carries `valid_at`/`invalid_at`/`expired_at`.
Mapping it onto `created_at` would silently give it a different meaning (and one `merge_entities`
forwarding already mutates); burying it in `attributes` would put it somewhere nothing reads it
back. Neither is an acceptable stand-in for "the timestamp this fact became true." The spec was
corrected before Implement to scope FR-022 to `knowledge_assert_relationship` only —
`knowledge_assert_entity`'s `ToolSpec` schema has no `valid_at` property at all, and the handler
doesn't parse one.

### 3. `entity_uuid` is a strict group-scoped lookup — never a caller-chosen mint path

#179's FR-009 specified `entity_uuid` as *"if provided, used as the UUID"* — a caller could mint a
new entity at a UUID of their choosing. This issue's FR-007 deliberately narrows that: `entity_uuid`
is looked up within `group_id` and the call fails if absent, with **no create fallback under that
UUID**. This closes a path for writing an entity into a group at an attacker- or caller-chosen
identifier, and it keeps the tool's cross-group refusal (FR-014/FR-015) simple to reason about —
there is no second identity channel that could smuggle a foreign UUID past name-based resolution.

The direct consequence: idempotency for `knowledge_assert_entity` rests entirely on `(name,
group_id)` per FR-011, not on `entity_uuid`. A caller cannot pin a UUID across a rebuild or a
fresh create. This also rules out ever deriving a *deterministic* UUID for this feature (e.g.
`uuid_v5(group_id, name)`) as a way to satisfy some future "stable UUID" ask — #369 already
considered and deliberately deferred that idea, and #371 showed the benefit mostly evaporates
anyway: purge-and-rehydrate destroys the endpoint hop regardless of whether the rehydrated entity
re-mints the same UUID, so re-binding is required either way. `Uuid::new_v4()` stays the only
create path. (User Story 3's acceptance scenario originally asked for "identical entity and edge
UUIDs" across two independently-asserted graph instances — unsatisfiable given this decision — and
was corrected to structural determinism, aligning it with the spec's own SC-008, before Implement.)

### 4. `embedding_warning` is a response field, not just a log line

FR-012/FR-021 require the call to still succeed when the configured embedder is unavailable. No
existing single-write handler had this fallback (`handle_add_cross_group_edge` propagates the
embed error via `?`; `canonicalize.rs`'s warn-and-skip pattern is a batch/loop context). Both
assert handlers add an `embedding_warning: Option<String>` field to the JSON result — `null` on
success, a descriptive string when the embedder call failed — so a caller can detect and act on a
degraded embedding without scraping stderr, following `canonicalize.rs`'s existing warning-string
convention at the response-shape level.

### 5. The edge upsert match excludes invalidated edges

`find_active_relates_to_uuid`'s `WHERE rn.invalid_at IS NULL` filter (backing FR-016/FR-017's
upsert) mirrors `has_directed_edge`'s existing filter (which it now shares an implementation
with). An edge invalidated by `knowledge_merge_entities` or `rebind_pointers`'s self-loop/duplicate
handling is no longer a live assertion; re-asserting the same `(source, predicate, target,
group_id)` creates a fresh edge rather than resurrecting the invalidated one.

### 6. An update never rewrites the stored embedding — only create does

**Discovered during implementation, not anticipated by the spec.** `Entity.name_embedding` and
`RelatesToNode_.fact_embedding` sit under lbug's HNSW vector index once `create_vector_indexes`
has run (the normal state of a live service — see ADR-0025/ADR-0036). lbug's binder rejects a
plain `SET` on an indexed column outright:

```
Cannot set property name_embedding in table Entity because it is used in one or more indexes.
Try delete and then insert.
```

This surfaced as a hard failure the first time `update_entity_core`/`update_relates_to_core` ran
against a DB with indexes built (exactly `crates/core/tests/ipc_parity.rs`'s `make_db` helper,
which always calls `create_vector_indexes()`). Deleting and recreating the row to satisfy the
error message is not viable for an entity or edge that already has relationships attached — it
would require manually collecting and rebuilding every incident hop, an unimplemented and
unprecedented amount of machinery for a single-row update.

This codebase already has precedent for the correct alternative: `episode.rs`'s
`DedupDecision::Merge` path, which re-matches an extracted entity onto an existing one, writes
only `SET e.summary = $summary` — it never re-touches `name_embedding` on the existing row.
`update_entity_core`/`update_relates_to_core` follow the same precedent: the caller still calls
the embedder on every assert (create *or* update), so `embedding_warning`'s availability signal
stays consistent and observable either way, but the `SET` statement itself omits
`name_embedding`/`fact_embedding` entirely. An update leaves the entity or edge's previously
stored embedding untouched; only `insert_entity`/`insert_relates_to_edge` (before any index exists
over the row) ever writes it.

### 7. "Empty embedding" on embedder failure is a same-dimension zero vector, not a literal empty list

**Also discovered during implementation.** `Entity.name_embedding FLOAT[N]` and
`RelatesToNode_.fact_embedding FLOAT[N]` are lbug's fixed-size `ARRAY` type, not a variable-length
`LIST` — `N` is fixed at schema-creation time. A literal zero-length `Vec<f32>` fails to bind with
a `Conversion exception: Unsupported casting LIST with incorrect list entry to ARRAY. Expected: N,
Actual: 0`. The spec's Edge Cases text ("an empty embedding is stored") cannot be satisfied
literally against this schema. Both handlers instead store `vec![0.0f32; state.embedder.dim()]` —
a zero vector of the embedder's configured dimension — as the only physically valid stand-in for
"no real embedding," alongside the same `embedding_warning` response field from Decision 4. A zero
vector is functionally inert against cosine-similarity search paths (uniformly dissimilar to
everything), matching the spirit of "no signal" the spec's wording intended.

## Consequences

- `assert.rs`'s `Resolved` enum and `cross_group::follow_merged_into_chain` are the only new
  public resolution surface; `resolve_endpoint` itself is behavior-preserving (verified by
  `cross_group_pointers.rs`'s existing 28-test suite passing unchanged after the extraction).
- A caller relying on re-embedding to "refresh" a stale vector via re-assertion will not get one on
  an update — only a genuinely new entity/edge (or a direct `knowledge_merge_entities`-style
  rewrite, out of scope here) picks up a new embedding. This is a real limitation, not merely a
  documentation gap; a future issue wanting live embedding refresh on update will need to solve
  the indexed-column constraint directly (e.g. drop/rebuild the specific HNSW index around a
  batch of updates, as `handle_rebuild_from_wal` already does for full-graph rebuilds) rather than
  assuming a per-row `SET` will work.
- Every asserted entity/edge still carries a full-dimension embedding column (never a
  dimension-mismatched or genuinely empty one), so downstream vector-index rebuilds and search
  paths need no special-casing for assert-created rows.

## Alternatives Considered

### Extend `resolve_endpoint`'s `BindingState` with a finer-grained dead-end variant

Rejected: it would touch `rebind_pointers`, `create_cross_group_edge`, and every consumer of
`BindingState` for a distinction only the two assert handlers need. A new, narrower type
(`assert::Resolved`) with its own two functions keeps the blast radius to the module that needs it.

### Drop/rebuild the specific HNSW index around every assert call to allow an in-place embedding update

Rejected as disproportionate: a single-entity or single-edge assert is meant to be a cheap,
synchronous write comparable to any other structured write handler. Dropping and rebuilding a
vector index scales with the whole table's size, not with the one row being asserted — the same
reason `handle_rebuild_from_wal` only pays that cost once, around a full-graph bulk operation, not
per mutation.

### Store a caller-supplied UUID on create when `entity_uuid` doesn't resolve

Rejected — reopens exactly the path FR-007 deliberately closed (see Decision 3) and reintroduces
the same cross-group-UUID-manufacturing risk ADR-0369's pointer model was built to avoid.

## Related

- `crates/core/src/assert.rs` — `Resolved`, `resolve_entity_by_name`, `resolve_entity_by_uuid`.
- `crates/core/src/cross_group.rs` — `follow_merged_into_chain` (extracted), `resolve_endpoint`.
- `crates/core/src/db.rs` — `update_entity_core`, `update_relates_to_core`,
  `find_active_relates_to_uuid`, `validate_and_normalize_valid_at`.
- `crates/core/src/handlers.rs` — `handle_assert_entity`, `handle_assert_relationship`.
- `crates/core/tests/assert.rs`, `crates/core/tests/ipc_parity.rs` — FR-level and wire-shape
  coverage, including the embedder-unavailable zero-vector fallback and the group-scoped edge
  upsert.
- [ADR-0368](0368-group-scoped-edge-dedup-in-merge.md) — governs the group-scoped edge upsert
  match this issue's FR-016 follows.
- [ADR-0369](0369-resolvable-cross-group-pointers.md) — defines `knowledge_add_cross_group_edge`,
  the tool named in FR-015's cross-group-refusal error.
- [ADR-0371](0371-merge-never-writes-foreign-group-data.md) — the `merged_into` forwarding
  semantics this issue's FR-008/FR-009 reuse, and the "a write in group G touches only G's data"
  rule FR-013/FR-014/FR-016 follow.
- [ADR-0025](0025-auto-heal-index-build.md), [ADR-0036](0036-eager-index-build-at-startup.md) —
  why a live service's HNSW indexes are normally already built, making Decisions 6/7's constraint
  the common case rather than an edge case.
- Issue #179 — superseded; its caller-chosen-UUID mint path (Decision 3) and ungrouped edge upsert
  (Background in the spec) are the two corrected requirements this issue exists to fix.
