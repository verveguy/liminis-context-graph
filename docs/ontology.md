---
layout: default
title: Ontology
---

# Ontology

`liminis-context-graph` supports an **optional workspace-scoped ontology** that declares the entity types and relation types the LLM should use during extraction. Without an ontology, the LLM derives types ad-hoc (free-form behavior). With one, vocabulary is consistent and queryable across all chunks.

## File location

Place the ontology at `{workspace}/.lcg/ontology.yaml`.

**Requires a service restart to take effect.** The ontology is loaded once at startup and held in memory. Editing the file while the service runs has no effect until the next restart.

## Format

```yaml
# mode: open | strict
# open (default): declared types are preferred; free-form fallback allowed
# strict: out-of-vocabulary entities are dropped post-extraction; out-of-vocabulary
#   edges are never dropped — a declared alias is normalized to its canonical relation
#   type, and anything else is reclassified to relation_type: UNCLASSIFIED with the
#   original label preserved in the edge's attributes (see ADR-0310)
mode: strict

entity_types:
  - name: Person           # normalized to PascalCase
    description: A human individual, not a role or title.
  - name: Organization
  - name: Document
  - name: Rfc
    parent: Document       # optional: Rfc is a subtype of Document
  - name: Adr
    parent: Document       # optional: Adr is also a subtype of Document
  - name: Paper

relation_types:
  - name: AUTHORED         # normalized to SCREAMING_SNAKE_CASE
    description: A person wrote a paper.
    source_type: Person    # optional signature constraint (informational in v1)
    target_type: Paper
    aliases: [WROTE, PENNED]   # optional: alternate spellings normalized to AUTHORED
    keywords: [author]         # optional: lowercase substrings used by the offline
                                # knowledge_canonicalize_relations pass (fuzzy match)
  - name: AFFILIATED_WITH
    source_type: Person
    target_type: Organization
```

`aliases` and `keywords` on a relation type have three consumers, each with different matching rules:

- **The `strict`-mode edge prompt** (`build_fact_types_section`) renders both `aliases` and `keywords` for every declared relation type, so the model can see the full set of accepted spellings and is more likely to emit the canonical name directly.
- **Ingest-time `strict`-mode filtering** (`episode.rs`) consults only `aliases`, as an exact match after the same case/separator normalization applied to every relation type name (`normalize_relation_type`) — e.g. `wrote` normalizes to `WROTE` and resolves via the alias map to `AUTHORED`. `keywords` play no role in ingest-time filtering.
- **The offline `knowledge_canonicalize_relations` maintenance pass** (see [IPC & MCP Reference](ipc-mcp-reference.md#relation-typing-canonicalize_relations-backfill_relation_types-reprocess_relation_types)) consults both: `aliases` via the same exact map, and `keywords` as lowercase substrings for its fuzzy-matching fallback.

### Entity type hierarchy

The optional `parent: <TypeName>` field on an entity type declares a single-parent (tree) subtype relationship. A node typed `Rfc` will carry labels `["Entity", "Document", "Rfc"]` — enabling both specific queries (`WHERE 'Rfc' IN e.labels`) and rollup queries (`WHERE 'Document' IN e.labels`).

- **Additive**: the specific type is never replaced by its parent; ancestor labels are added alongside it.
- **Transitive**: a 3-level chain `SubDoc → Rfc → Document` stamps all four labels.
- **Safe degrades**: an undeclared parent is cleared with a warning; cycles are detected and broken at startup (no crash).
- **Flat ontologies unaffected**: types without `parent` fields behave exactly as before — `["Entity", <SpecificType>]`.
- **Drift detection**: adding, removing, or changing a `parent` changes the ontology content hash, which triggers a `drifted: true` status in `knowledge_status`. Run `knowledge_reprocess_entity_types` to propagate new hierarchy to existing nodes.

See [`docs/examples/ontology.example.yaml`](https://github.com/verveguy/liminis-context-graph/blob/main/docs/examples/ontology.example.yaml) for a fully annotated scientific-paper-domain example.

## Modes

| Mode | Entity types | Relation types |
|------|-------------|----------------|
| `open` (default) | Preferred by the LLM; free-form fallback allowed | Same |
| `strict` | Out-of-vocabulary entities dropped post-extraction | The edge-extraction prompt tells the model to use only the declared vocabulary (including aliases). A declared alias is normalized to its canonical name; anything still out-of-vocabulary after normalization is retained with `relation_type: UNCLASSIFIED` and its original label preserved in `attributes` — never dropped (ADR-0310) |

## `knowledge_status` summary

The `knowledge_status` IPC response always includes an `ontology` field:

```json
{
  "ontology": {
    "present": true,
    "mode": "strict",
    "entity_type_count": 4,
    "relation_type_count": 4
  }
}
```

When no ontology is loaded, `present` is `false` and counts are `0`.

The response also includes an `indices_built` boolean, a `name_index_trusted` boolean, and a
`name_index_fallback_scans` integer — these describe search-index and name-lookup health rather
than ontology state. See [Operations](operations.md) for those fields.
