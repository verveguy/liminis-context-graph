# ADR 0044: Two-Hop RELATES_TO Traversal as Canonical Read Pattern

**Date**: 2026-05-22
**Status**: Accepted

## Context

liminis-graph operates against workspaces that may have been initialised by either the Python graphiti service or by the Rust binary itself. The two runtimes encode RELATES_TO edges differently:

**Python schema** (LadybugDB driver in `graphiti_core/driver/ladybug_driver.py`):
```sql
CREATE REL TABLE IF NOT EXISTS RELATES_TO(
    FROM Entity TO RelatesToNode_,
    FROM RelatesToNode_ TO Entity
    -- no properties on the rel itself
)
```
Write pattern: `CREATE (src)-[:RELATES_TO]->(rn:RelatesToNode_), (rn)-[:RELATES_TO]->(dst)`

**Rust schema** (before this ADR, `schema.rs`):
```sql
CREATE REL TABLE IF NOT EXISTS RELATES_TO (
    FROM Entity TO Entity,
    uuid STRING, name STRING, group_id STRING, ...
)
```
Write pattern: `CREATE (src)-[:RELATES_TO {uuid: '...', ...}]->(dst)`

The root cause of the bugs fixed in issue #38: when the Rust binary starts against a Python-populated workspace, `CREATE REL TABLE IF NOT EXISTS` is a no-op (table already exists). The existing RELATES_TO table has no properties. Any Cypher query that binds `[r:RELATES_TO]` and then reads `r.uuid` fails with a compile-time binder error before executing.

Nine read methods in `db.rs` assumed the Rust direct-rel pattern and all failed against Python-written data.

## Decision

**All RELATES_TO edge reads use the two-hop traversal pattern:**

```cypher
MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity)
RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, ...
```

All edge properties are read from the `RelatesToNode_` shadow node (`rn`). The `[:RELATES_TO]` relationship itself is treated as a navigation hop only — no properties are read from it.

**The Rust write path is extended** to also create two-hop links in addition to the existing direct `Entity→Entity` rel:
```cypher
-- existing (kept for schema compatibility)
CREATE (src)-[:RELATES_TO {uuid: '...', ...}]->(dst)
-- new: two-hop links for reads
CREATE (src)-[:RELATES_TO]->(rn:RelatesToNode_ {uuid: '...'})
CREATE (rn)-[:RELATES_TO]->(dst)
```

**The RELATES_TO rel table** is updated to declare all three FROM-TO pairs:
```sql
CREATE REL TABLE IF NOT EXISTS RELATES_TO (
    FROM Entity TO Entity,          -- Rust write path (carries all properties)
    FROM Entity TO RelatesToNode_,  -- two-hop hop 1 (property-free navigation)
    FROM RelatesToNode_ TO Entity,  -- two-hop hop 2 (property-free navigation)
    uuid STRING, name STRING, ...
)
```
Because `IF NOT EXISTS` is a no-op on existing databases, Python-populated workspaces retain their original schema (no Entity→Entity pair, no properties). Two-hop reads work correctly on both schemas.

## Consequences

### Benefits
- Reads work against both Python-populated and Rust-populated workspaces with a single query form.
- `RelatesToNode_` is the single source of truth for edge properties. No property is split across the shadow node and the rel.
- Future additions to edge data (e.g., new columns) only require changes to the `RelatesToNode_` node schema, not to the rel table.

### Constraints
- **Every new Cypher query that returns edge data MUST use the two-hop traversal.** Reading `r.property` from `[r:RELATES_TO]` is prohibited. This constraint must be applied to all future contributors.
- **Old Rust-only databases** (created before this fix) have `Entity→Entity` direct RELATES_TO rels but no two-hop links. After this fix, reads against those old databases return empty results for those edges. Old databases should be rebuilt.
- The write path creates three separate SQL statements per edge insert (shadow node + direct rel + two-hop links). This is a minor write amplification but is acceptable at current scale.

### Rationale for not using schema-detection
A startup-time schema probe was considered and rejected:
- The binder error is compile-time; a single static query must work for both schemas
- Schema detection adds `AppState` coupling and ongoing maintenance burden
- Extending the write path is simpler: all future DBs share the two-hop layout

## References

- Issue #38: Tier 1b bug — `list_relationships` and `get_entity_neighbors` use wrong edge schema
- `liminis-graph-core/src/db.rs`: `insert_relates_to_edge`, all read methods
- `liminis-graph-core/src/schema.rs`: `create_edge_tables`
- Python driver: `graphiti_core/driver/ladybug/hnsw_safe_writes.py`
