# ADR-0023: Legacy-WAL Translation Layer — Cypher-text/Param-shape vs. Param-value Module Split

**Status**: Accepted
**Date**: 2026-06-15
**Issue**: #133

## Context

FalkorDB-era WAL files (written under the original Graphiti+FalkorDB integration — the Liminis
app's ADR-035 — before the FalkorDB→Kuzu/lbug migration, app ADR-052) contain
dialect constructs that lbug cannot execute:

- **`vecf32(...)`**: FalkorDB's float32-vector constructor. lbug stores embeddings in `FLOAT[N]`
  columns that accept a bare list literal directly — the wrapper is FalkorDB-only.
- **`SET n = $props`** (bulk property-set): FalkorDB/Neo4j allow setting all properties from a
  single map param. lbug requires individual `SET n.k = $v` assignments.

These are *Cypher-text* or *param-shape* transforms — they modify the Cypher string or the
shape of the params object before interpolation. They are distinct from *param-value* transforms
(e.g., backslash string escaping, RFC-3339 timestamp literals in #128/#130) which only change
how a specific param value is rendered as a Cypher literal.

Before this ADR, all translation logic lived in `replay.rs::json_to_cypher_literal` (a
param-value function), making it an awkward home for Cypher-text rewrites. Future contributors
would face ambiguity about where to put the next compatibility fix.

## Decision

The WAL replay pipeline applies two classes of transforms in order:

```
raw wal_line.cypher
  → strip_vecf32          (Cypher-text only, no params)           ─┐ legacy_wal.rs
  → expand_bulk_property_set (Cypher-text + params shape)         ─┘
  → interpolate_params    (param-value → Cypher literal)          ─── replay.rs
  → conn.raw_query
```

**Module ownership rule:**

| Layer | What changes | Module |
|---|---|---|
| Cypher-text and/or param map shape | Rewrites the Cypher string or adds/removes keys in the params object | `legacy_wal.rs` |
| Param value → Cypher literal | Changes how a single param value is formatted (string escaping, typed constructors like `timestamp()`) | `replay.rs::json_to_cypher_literal` |

A new `crates/core/src/legacy_wal.rs` module houses:
- `strip_vecf32(cypher: &str) -> String` — balanced-parenthesis case-insensitive scan
- `expand_bulk_property_set(cypher: &str, params: &Value) -> (String, Value)` — regex-based bulk-SET expansion, with `param_key` prefixing to avoid name collisions

## Rationale

1. **Pipeline ordering constraint**: `strip_vecf32` must precede `interpolate_params` so that
   `vecf32($emb)` is stripped to `$emb` before `interpolate_params` substitutes it with the
   array literal. Similarly, `expand_bulk_property_set` must precede `interpolate_params`
   because it mutates the params map, adding flattened top-level keys that `interpolate_params`
   then resolves. Swapping the order breaks both transforms.

2. **Module boundary prevents misclassification**: without a named boundary, the next FalkorDB
   compat fix would likely land in `json_to_cypher_literal` (the only existing translation
   function), even if it is a Cypher-text rewrite. The named module + doc comment makes the
   right home unambiguous.

3. **Dependency management**: `legacy_wal.rs` requires `regex` in `[dependencies]` (not only
   `[dev-dependencies]`) for the `expand_bulk_property_set` implementation. The `strip_vecf32`
   function uses a pure balanced-paren scan and adds no new dependency.

## Consequences

- Future FalkorDB→lbug dialect fixes that rewrite the Cypher string or reshape the params map
  MUST go in `legacy_wal.rs`, not in `json_to_cypher_literal`.
- Future fixes that only change how a param *value* is formatted (e.g., a new typed Cypher
  constructor for a different lbug type) MUST go in `json_to_cypher_literal`.
- If lbug ever gains a parameterized-query API (see ADR-0015), both modules become unnecessary
  and should be removed — the dialect gap is eliminated at source.
- The `regex` crate is now a production dependency of `lcg-core`. It is already
  declared in `[workspace.dependencies]` and carries no additional version-management burden.
