# ADR-0029: Name-First Entity Resolution in add_episode Phase B

**Status**: Accepted
**Date**: 2026-06-22
**Issue**: #164 — Cross-episode entity resolution: fix identical-name duplication at ingest

## Context

`add_episode`'s Phase B deduplicates extracted entities against the persisted graph using
embedding-based cosine similarity (`brute_force_similar_entity` or
`hybrid_dedup_similar_entity`). Because `MockEmbedder` returns zero vectors and
`cosine_similarity` returns 0.0 for zero norms, this path was effectively disabled in all
existing tests and also unreliable when the embedding service returns the same vector for
many names. The observable consequence was 53 "Brett" nodes in production — every episode
that mentioned "Brett" minted a fresh node instead of reusing the existing one.

The root cause: Phase B had no name-based lookup. A name is the highest-confidence signal
that two extracted entities refer to the same real-world thing.

## Decision

Insert a case-insensitive, whitespace-normalised **name lookup** as the first resolution step
in Phase B, before any embedding-based check. The new step calls `Conn::get_entity_by_name_ci`
(added to `db.rs`), which uses `lower(e.name) = $lower_name` in Kuzu (lbug) with the
parameter pre-lowercased in Rust. If a match is found, the entity is immediately decided as
`DedupDecision::Merge` and the embedding lookup and dedup-adapter check are **skipped**.

The ordering is: name lookup → embedding lookup (only if name lookup misses) → dedup adapter
(only if embedding lookup finds a candidate).

## Consequences

### Positive

- Identical-name entities across episodes now converge to one node (SC-001, SC-003).
- Case variants ("brett" vs "Brett") resolve correctly (FR-001 edge case).
- Resolution is durable across service restarts because it reads from the persisted lbug graph,
  not from in-process cache (FR-003, SC-003).
- Total cost per entity remains ≤ 1 name query + 1 embedding query (FR-009).
- Empty-name entities (whitespace-only after trim) are dropped via `retain` before Phase B and
  are never inserted into the graph, per the spec edge case.
- When historical duplicates exist, `get_entity_by_name_ci` uses `ORDER BY e.created_at ASC,
  e.uuid ASC` to deterministically pick the oldest node, making successive calls stable.

### Negative / Residual risks

- **Full-table scan on name lookup**: Kuzu (and therefore lbug) does not support
  function-based indexes such as `CREATE INDEX ON Entity(lower(name))`. The
  `lower(e.name) = $lower_name` WHERE clause therefore causes a full scan over all Entity
  nodes in the group. For groups below `hybrid_threshold` (default 1 000) the brute-force
  embedding path already does an in-Rust full scan, so this is not a regression. For very
  large groups the name-lookup cost is bounded by the group's Entity count (same as a property
  scan). A `name_lower` stored column with a standard Kuzu property index is the path to O(1)
  name lookups; that schema migration is deferred to a follow-up issue.
- **TOCTOU race**: Two concurrent `add_episode` calls for the same entity name will both see
  zero existing matches (Phase B runs without the write lock), and both will commit an insert
  in Phase C. This creates duplicates under concurrent ingest. Strict serializability under
  concurrency is out of scope for v1 (documented in the spec).
- **`lower()` in lbug**: Kuzu (and therefore lbug) supports `lower()` as of Kuzu 0.5, and
  lbug is pinned to Kuzu 0.17. The implementation was validated by the new integration tests
  (`cross_episode_dedup.rs`) which call `get_entity_by_name_ci` against a real lbug instance.
  If a future lbug downgrade removes `lower()`, the symptom is a query error on every
  `add_episode` call — easy to diagnose.

## Alternatives Considered

### Store a separate `name_lower` column

Rejected: requires a schema migration and changes to every `insert_entity` call site. The
`lower()` Cypher approach avoids both.

### Add a `case_insensitive: bool` flag to `get_entity_by_name`

Rejected: `get_entity_by_name` is called from `corrections.rs` where exact-case semantics
are expected. Adding a flag would require every caller to be updated and risks confusion.
A separate `get_entity_by_name_ci` function is clearer and preserves the existing contract.

### Make name lookup the *only* dedup path (drop embedding dedup)

Rejected: FR-002 requires embedding-based resolution for name variants ("Brett Adamson" vs
"Brett A."). Both paths are needed; name lookup is a fast pre-filter, embedding lookup is the
fallback for near-matches.

## Implementation Notes

- `PhaseBResult` enum in `episode.rs` carries the outcome of the per-entity Phase B lookup:
  `NameMatch { existing: EntityRow }` or `EmbeddingCandidate { candidate: Option<EntityRow> }`.
- The async decisions loop skips `dedup.is_duplicate` for `NameMatch` variants (it would be
  redundant given an exact name match).
- A `warn!` (via `eprintln!`) is emitted when a name-matched entity's existing labels conflict
  with the extracted type, per the spec edge case.
- `count_entities_by_name_ci` was added to `Conn` for test assertions only; it is not called
  from production code paths.
