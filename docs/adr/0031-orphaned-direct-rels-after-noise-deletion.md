# ADR-0031: Orphaned Direct RELATES_TO Rels After Noise Edge Deletion

**Status**: Superseded by ADR-0033  
**Date**: 2026-06-22  
**Context**: Issue #163 — Relation canonicalization noise deletion  
**Superseded by**: ADR-0033 (noise edges are reclassified, not deleted — no orphaning occurs)

## Context

Every `RELATES_TO` relationship in this graph has a dual representation:

1. **Shadow node**: A `RelatesToNode_` node carrying all edge properties (`uuid`, `name`,
   `fact`, `relation_type`, etc.), connected via two structural hop rels:
   - `(src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)`
   - `(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity)`

2. **Direct rel**: In Rust-initialized workspaces, a direct `(src:Entity)-[:RELATES_TO]->(dst:Entity)`
   rel is also created alongside the two-hop structure. This direct rel has no `uuid` field and
   is only used for traversal compatibility with graph algorithms that expect a simple 1-hop edge.

All read queries in this service use the **two-hop path** through `RelatesToNode_` — not the
direct rel. The direct rel is structurally present but semantically redundant; it has no
uuid-keyed lookup and carries no properties.

## Decision

When the canonicalization pass deletes a noise `RelatesToNode_` node via
`MATCH (n:RelatesToNode_ {uuid: $uuid}) DETACH DELETE n`, the DETACH DELETE removes:
- The `RelatesToNode_` shadow node
- Both structural hop rels (`src→rn` and `rn→dst`)

It does **NOT** remove the direct `(src:Entity)-[:RELATES_TO]->(dst:Entity)` rel, because that
rel has no relationship to the shadow node — it connects `Entity` directly.

**v1 accepts this orphaned direct rel** for the following reasons:

1. All read queries use the two-hop path; orphaned direct rels are never returned to callers.
2. There is no uuid-keyed lookup path for direct rels, making targeted deletion impractical
   without a full graph scan.
3. The orphaned rels are semantically inert — they carry no properties and are invisible to
   all service operations.

## Cleanup path (future)

When orphaned direct rels need to be cleaned up (e.g., before a schema migration that relies
on the invariant), a future pass can execute:

```cypher
MATCH (a:Entity)-[r:RELATES_TO]->(b:Entity)
WHERE NOT EXISTS {
  (a)-[:RELATES_TO]->(:RelatesToNode_)-[:RELATES_TO]->(b)
}
DELETE r
```

This scan is safe to run at any time because orphaned direct rels are read-invisible and have
no application-level meaning after their shadow node is gone.

## Alternatives considered

- **Delete direct rels immediately in the noise deletion pass**: Requires a matching query by
  `(src_uuid, dst_uuid)` pair for each deleted noise edge. Since multiple direct rels may exist
  between the same entity pair (from different episodes), this risks deleting too many rels.
  Deferred until a proper inverse-lookup mechanism is in place.
- **Prevent creation of direct rels**: Would require changes to the episode ingestion path and
  is out of scope for this issue.
