# ADR-0020: IPC Collection Response Envelope Contract

**Status:** Accepted  
**Date:** 2026-05-26  
**Relates to:** issue #96 (entity-search UI bug), ADR-0002 (reader-writer split), ADR-0006 (embedder HTTP contract)

## Context

liminis-context-graph's IPC protocol returns collection data from many methods. Prior to this ADR,
these methods used inconsistent response shapes:

- `knowledge_list_entities` returned `{nodes: [...], count: N}` — an object envelope
- `knowledge_find_entities` returned `[entity, ...]` — a bare JSON array

The inconsistency was an accident of implementation order, not an intentional design. It
caused a silent bug in liminis-app's UI entity search: `parseMcpResult(result, 'nodes')`
expects every collection response to be an object with a named key. When `find_entities`
returned a bare array, `'nodes' in []` evaluated to `false`, and the UI returned
`resultCount: 0` for queries that had real results in the graph.

Bare-array responses also make forward compatibility harder: adding a field (scores,
pagination, warnings) to a bare-array response is always a breaking change. Object
envelopes accept new fields without breaking consumers.

## Decision

Every IPC method that returns a collection of records MUST return a JSON object of the form:

```json
{
  "<collection_key>": [...],
  "count": N
}
```

Where:

- **`<collection_key>`** is the natural plural noun for the records in that response
- **`count`** equals `len(<collection_key>)` after all server-side filtering is applied — it
  is the size of the returned set, not a pre-filter total
- Additional metadata fields (`center_uuid`, `source`, `node_count`, `edge_count`, etc.)
  may appear alongside the required pair, but must not replace them

### Naming conventions by record type

| Record type | Collection key |
|-------------|----------------|
| Entity nodes | `nodes` |
| Relationship/edge records (semantic search, list) | `edges` |
| Edge records (structural fetches by group or UUID) | `edges` |
| Episode records | `episodes` |
| Passage records | `passages` |

### Multi-collection responses

When a method returns two parallel arrays (e.g., `knowledge_get_entity_neighbors` returns
both `nodes` and `edges`), the canonical `count` field tracks the **primary** collection
(nodes, in that case). Secondary counts use named fields (`edge_count`). Both arrays must
be present in the envelope.

### Pre-filter totals

If a future method needs to expose a pre-filter or pre-limit total (e.g., for pagination
UI), it goes in a field named `total_before_limit`. The `count` field always means "what
I'm giving you right now."

### Empty results

An empty collection returns `{"<key>": [], "count": 0}`. Never return `{}`, `null`, or
omit the collection key.

## Audit table (as of 2026-05-26; `facts`→`edges` rename per issue #524, 2026-09-01)

| Method | Collection key | Shape |
|--------|----------------|-------|
| `knowledge_find_entities` | `nodes` | `{nodes, count}` |
| `knowledge_find_relationships` | `edges` | `{edges, count}` |
| `knowledge_get_episodes` | `episodes` | `{episodes, count}` |
| `knowledge_get_nodes_by_group` | `nodes` | `{nodes, count}` |
| `knowledge_get_edges_by_group` | `edges` | `{edges, count}` |
| `knowledge_get_edges_by_uuids` | `edges` | `{edges, count}` |
| `knowledge_search_passages` | `passages` | `{passages, count}` |
| `knowledge_list_entities` | `nodes` | `{nodes, count}` |
| `knowledge_list_relationships` | `edges` | `{edges, count}` |
| `knowledge_get_entity_neighbors` | `nodes` (primary) | `{center_uuid, nodes, edges, count, node_count, edge_count}` |
| `knowledge_get_entities_by_source` | `nodes` | `{source, nodes, count}` |

**Issue #524 (2026-09-01):** `knowledge_find_relationships` and `knowledge_list_relationships`
previously returned the relationship list under `facts`, while `knowledge_get_edges_by_group`
already used `edges` — an inconsistency that let a client parsing the wrong key silently get an
empty list instead of an error. Rather than add a second alias key (which would have permanently
enshrined the inconsistency and doubled response payload size), both methods were renamed to
return `edges`, matching the one-key-per-record-type convention above. This is a breaking change,
released in the same 0.14.0 release as the storage-format migration (lbug 0.17.0 → 0.20.1,
storage v41 → v47) specifically because that release already carries a one-way break — see the
CHANGELOG entry for the client-facing migration note.

Single-record and scalar responses are out of scope: `knowledge_status`, `health_check`,
`knowledge_close`, and single-entity getters return records directly.

## Rationale

**Consistency eliminates client special-cases.** A single parser handles all collection
responses. No method requires caller-side branching on shape.

**Forward compatibility.** Adding scores, pagination cursors, or filter telemetry to an
envelope response is non-breaking — consumers reading `result.nodes` are unaffected by a
new `result.scores` sibling. Adding a field to a bare array is always breaking.

**Empty-vs-error clarity.** `{nodes: [], count: 0}` unambiguously signals "call succeeded,
zero results." A bare `[]` carries the same information with less structural confidence for
parsers that may conflate empty-array with missing key.

**Conformance enforcement.** `crates/core/tests/ipc_response_shapes.rs` calls every
collection-returning method and asserts `is_object() && "count" in keys && key.is_array()`
for each. New collection methods that skip the envelope will fail this test automatically.

## Consequences

- All 6 previously bare-array handlers now return envelopes (fixed in issue #96).
- `knowledge_get_entities_by_source` uses `count` (previously `node_count`). No in-tree
  consumer read `node_count` directly; `parseMcpResult` only extracts the `nodes` array.
- **New collection-returning methods that do not follow this contract MUST be rejected at
  review.** The conformance test will catch them at CI time, but the review gate is the
  primary enforcement point.
- Future issues for new methods (`knowledge_list_sources`, `knowledge_preview_chunks`,
  `knowledge_suggest_duplicates`, `knowledge_entity_edge_analysis`) must reference this ADR
  and produce conformant envelope responses from the start.
