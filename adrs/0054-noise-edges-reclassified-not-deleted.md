# ADR-0054: Noise Edges Are Reclassified to UNCLASSIFIED, Not Deleted

**Status**: Accepted  
**Date**: 2026-06-23  
**Context**: Issue #175 — relation_type consistency fix  
**Supersedes**: ADR-0052 (orphaned direct rels concern is now moot — no edges are deleted)

## Context

Issue #163 introduced `canonicalize_relations`, which classified edges matching the `X → Y`
arrow-name pattern as "co-occurrence noise" and deleted them via `DETACH DELETE`. This policy
was adopted before the graph had been fully analyzed.

A post-hoc production workspace analysis (tracked in issue #175) revealed that this deletion
policy is unsafe:

- **~22,843 edges** in a representative workspace have arrow-pattern names (`"Brett → Seattle"`,
  `"Brett Adam → PSET Architecture Council"`, etc.).
- Of those, **~21,474 (94%)** carry a populated, meaningful `relation_type`
  (`LOCATED_IN`, `PARTICIPATED_IN`, `HAD_COACHING_SESSION_WITH`, …).
- These edges represent real, semantically rich relationships — not co-occurrence noise.
- The `is_noise_edge` regex (`^[A-Z][A-Z0-9 ]*\s*(→|->)\s*[A-Z][A-Z0-9 ]*$`) matches only
  ALL-CAPS patterns like `"BRETT → RAJI"`. Mixed-case extractor-produced names
  (`"Brett Adam → Seattle"`) never matched and were never at risk of deletion. However,
  the ALL-CAPS pattern can arise from entity names that happen to be all-caps; those edges
  are equally real.

Running `canonicalize_relations` on production data with the old delete path would
irreversibly destroy up to 94% of those 22,843 edges. No backup or undo path exists.

## Decision

`canonicalize_relations` **never deletes** any edge. The `EdgeClass::Noise` branch in Phase D
now applies the same treatment as `EdgeClass::Residual`: set `relation_type = 'UNCLASSIFIED'`
with an idempotency guard.

Concretely, the CQL changes from:

```cypher
-- OLD (unsafe)
MATCH (n:RelatesToNode_ {uuid: $uuid}) DETACH DELETE n
```

to:

```cypher
-- NEW (safe, idempotent)
MATCH (n:RelatesToNode_ {uuid: $uuid}) SET n.relation_type = $rt
-- where $rt = 'UNCLASSIFIED', skip if already 'UNCLASSIFIED'
```

The `noise_count` field in `CanonicalizeReport` is retained. Its semantics shift from
"edges deleted" to "edges reclassified to UNCLASSIFIED". Callers must treat this count
as informational (scope of the pass) rather than a deletion count.

## Consequences

1. **No edges are deleted** by any automated pass in this service. Edge deletion only
   occurs via explicit operator calls (`knowledge_delete_episode`,
   `knowledge_delete_by_source`, `knowledge_clear_all`).

2. **ADR-0052 is moot**: The orphaned-direct-rels concern arose from `DETACH DELETE`
   leaving dangling `(Entity)-[:RELATES_TO]->(Entity)` rels. Since no edges are deleted,
   no orphaning occurs and the cleanup query described in ADR-0052 is never needed.

3. **`UNCLASSIFIED` population**: After a canonicalize pass, all arrow-named edges that
   had no ontology keyword match carry `relation_type = 'UNCLASSIFIED'`. The additive
   backfill pass (issue #175, `knowledge_backfill_relation_types`) can subsequently
   replace `UNCLASSIFIED` values with semantically derived predicates if desired.

4. **Regression test**: `parity_canonicalize_no_deletion_of_arrow_edges` in
   `crates/core/tests/ipc_parity.rs` and `test_noise_edges_reclassified_not_deleted`
   in `crates/core/tests/canonicalize_integration.rs` enforce this invariant.

## Alternatives Considered

- **Keep the delete path, but guard it behind an explicit operator flag**: Rejected because
  no legitimate use case for bulk-deleting arrow-named edges exists now that the data
  analysis confirms they carry real semantics. A flag would invite misuse.
- **Remove the noise classification entirely**: Considered but rejected because `noise_count`
  in the dry-run report gives operators useful visibility into how many edges have
  arrow-pattern names (independent of whether they are deleted or reclassified).
- **Rename `noise_count` to `arrow_name_count`**: Considered for clarity but rejected as
  a premature interface churn; the ADR documents the semantic shift sufficiently.

## References

- Issue #175 (relation_type consistency fix) — the root cause analysis
- Issue #163 (canonicalize_relations) — introduced the original delete path
- ADR-0052 (superseded) — orphaned direct rels after noise deletion
