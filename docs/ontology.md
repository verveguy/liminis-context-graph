---
layout: default
title: Ontology
---

# Ontology

`liminis-context-graph` supports an **optional workspace-scoped ontology** that declares the entity types and relation types the LLM should use during extraction. Without an ontology, the LLM derives types ad-hoc (free-form behavior). With one, vocabulary is consistent and queryable across all chunks.

## File location

Place the ontology at `{workspace}/.lcg/ontology.yaml`.

**Requires a service restart to take effect.** The ontology is loaded once at startup and held in memory. Editing the file while the service runs has no effect until the next restart.

## Per-group ontologies

A single lcg instance can hold many co-resident `group_id`s (multi-group hydrate), and those
groups often want different vocabularies — a content channel's `Person`/`Organization` ontology
should not constrain an unrelated catalog group co-resident in the same workspace. Place a
group-specific ontology at:

```text
{workspace}/.lcg/ontology/<group_id>.yaml
```

using the same file format described above. A `group_id` containing characters unsafe as a
filesystem path component (anything outside ASCII alphanumerics, `_`, and `-`) is percent-encoded
using the same bijective scheme already applied to per-group WAL directory names — every byte
outside that safe set becomes `%XX` (uppercase hex). A `group_id` that's already a safe path
component (e.g. `catalog`, `content-v2`) is used as the filename unchanged.

**Known v1 limitation: no case-insensitive collision guard.** Two already-safe `group_id`s that
differ only by ASCII case (e.g. `Catalog` and `catalog`) resolve to the same filename on a
case-insensitive filesystem (the default for macOS APFS and Windows NTFS). Per-group WAL
directories guard against this exact case with an explicit, loudly-failing check
(`wal_group::check_no_case_insensitive_collision`, invoked when a group's WAL writer is first
created); per-group ontology file resolution does not yet apply the same guard, so on an
affected filesystem one group's ontology could silently load for the other. Avoid `group_id`s
that differ from another co-resident group's only by letter case until this is closed.

**Resolution and fallback.** For a given `group_id`:

1. If `{workspace}/.lcg/ontology/<group_id>.yaml` exists and parses successfully, it governs
   extraction, `mode` (including strict validation), canonicalization, and reprocessing
   (`knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`) for that group only.
2. Otherwise, the workspace-wide `{workspace}/.lcg/ontology.yaml` (described above) governs that
   group, exactly as it did before per-group ontologies existed.
3. If neither exists, that group extracts free-form, same as an ontology-less workspace today.

A malformed or unreadable per-group file is treated exactly like a missing one: resolution falls
through to step 2 (the workspace-wide ontology) if one exists, or step 3 (free-form extraction) if
it doesn't — never a startup failure or a hard error for that group. This degrades gracefully to
whatever ontology this workspace already has validated (which may be none at all), and the failure
is logged so it's observable rather than silent. Like the workspace-wide file, per-group files are
loaded once (on that group's first use in the running process) and cached — restart the service to
pick up a changed file.

**Direct-assert is unaffected.** `knowledge_assert_entity`/`knowledge_assert_relationship` accept
arbitrary `labels` regardless of any per-group or workspace ontology — per-group resolution only
governs *extraction-guided* groups (`knowledge_add_episode` and the maintenance operations above).

**`canonicalize_relations`** resolves and applies the target group's own ontology, scoped to the
`group_id` the call already requires. **`backfill_relation_types`** is ontology-independent — it
derives pseudo relation types from edge fact text, not from a declared vocabulary — so per-group
ontology resolution has nothing to change there.

**Published ontology is documentation, not policy.** When a group's stream is published (the
existing whole-directory copy described in [Operations](operations.md)), the ontology that guided
that group's extraction travels alongside it as `.wal-ontology.json` — informational only. A
consumer hydrating that stream can inspect it to see what vocabulary produced the graph, but it is
never applied to the consumer's own extraction, `mode: strict` validation, canonicalization, or
reprocessing for that group — the consumer's own local configuration (per-group file, workspace
file, or neither) is always what governs. A stream published without this file still replays and
behaves identically; only the documentation available to the consumer is degraded.

## Format

```yaml
# mode: open | strict
# open (default): declared types are preferred; free-form fallback allowed
# strict: out-of-vocabulary entities and edges are never dropped for their type alone — an
#   entity whose type doesn't match the declared vocabulary is reclassified to Unclassified,
#   with its original type preserved in the entity's attributes (see ADR-0312); a declared
#   alias on an edge is normalized to its canonical relation type, and anything else is
#   reclassified to relation_type: UNCLASSIFIED with the original label preserved in the
#   edge's attributes (see ADR-0310). Edges can still be dropped for unrelated reasons
#   (self-referential, unresolvable endpoint) — see edges_dropped_unresolvable.
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
| `strict` | An entity whose normalized type doesn't match the declared vocabulary is retained with an `Unclassified` label in place of the rejected type, and its original type preserved in `attributes` — never dropped for its type alone (ADR-0312) | The edge-extraction prompt tells the model to use only the declared vocabulary (including aliases). A declared alias is normalized to its canonical name; anything still out-of-vocabulary after normalization is retained with `relation_type: UNCLASSIFIED` and its original label preserved in `attributes` — never dropped for its type alone (ADR-0310) |

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
