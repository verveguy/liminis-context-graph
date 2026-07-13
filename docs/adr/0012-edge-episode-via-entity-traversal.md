# ADR-0012: Edge-to-Episode Associations via Either-Endpoint Entity Traversal

**Status:** Accepted  
**Date:** 2026-05-25  
**Issue:** [#32](https://github.com/verveguy/liminis-context-graph/issues/32)

## Context

When enriching `RelatesToEdge` responses with `episode_uuids` and `source_descriptions`,
we need to answer: which episodic nodes are associated with a given edge?

Two approaches were considered:

**Option A — Both-endpoint Cypher cycle (strict):**
Match episodes that have `MENTIONS` relationships to _both_ the source and target entity
of the edge:
```
MATCH (ep:Episodic)-[:MENTIONS]->(src)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst)<-[:MENTIONS]-(ep)
WHERE rn.uuid IN [...]
```
This would attribute an edge only to episodes that explicitly mentioned both endpoints.

**Option B — Either-endpoint entity lookup (permissive):**
Collect all source and target entity UUIDs from returned edges, run the same
`MATCH (ep:Episodic)-[:MENTIONS]->(n:Entity) WHERE n.uuid IN [...]` query used for entity
enrichment, then attribute an edge to any episode that mentions either its source _or_
target entity — deduplicating by episode UUID.

## Decision

We chose **Option B**.

## Rationale

1. **No direct Episodic→RelatesToNode_ relationship exists.** The schema
   (`schema.rs:59-92`) defines `MENTIONS` only as `FROM Episodic TO Entity`. There is no
   relationship connecting episodic nodes to `RelatesToNode_` nodes in the graph.

2. **lbug cycle support is unverified.** Option A requires a single Cypher MATCH path
   where the same node (`ep`) appears at both ends. Whether lbug 0.16.1 supports this
   circular pattern was not confirmed during research.

3. **Code reuse.** Option B reuses `get_episode_info_for_entities` unchanged — the same
   batch lookup that enriches entity responses also drives edge enrichment with no new
   Cypher patterns.

4. **Graceful degradation.** If the enrichment query fails, `unwrap_or_default()` returns
   empty arrays for that response — the main result is unaffected.

## Consequences

- An edge's `episode_uuids` may include episodes that mentioned only one of its two
  endpoints. This is semantically permissive but is consistent with the Python
  `graphiti_service.py` behavior, which derives edge episodes from the entity objects
  returned by the graph traversal rather than a direct edge-to-episode query.

- The either-endpoint deduplication logic in `enrich_edge_from_entity_ep_info`
  (`handlers.rs`) ensures each episode UUID appears exactly once in the result even when
  both endpoints are mentioned by the same episode.

- If a future lbug version confirms cycle support, Option A could be revisited for
  stricter attribution. Until then, Option B is the production behavior.
