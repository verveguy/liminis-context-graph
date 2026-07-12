# ADR-0032: Ontology `parent_edges:` segment conditionally included in content hash

**Date:** 2026-06-23
**Status:** Accepted
**Issue:** #173

## Context

Issue #173 adds a `parent:` field to the ontology YAML, forming a single-parent hierarchy of
entity types. The ontology content hash (ADR-0018) must cover parent relationships so that
drift detection (#98) fires when a hierarchy is added, removed, or changed.

A naïve implementation appends `\0{parent}` to each entity entry in the canonical form:

```
entity_types:{name}\0{description}\0{parent}
```

This changes the hash for **all existing flat ontologies** (those with no `parent` fields)
on the first restart after the #173 upgrade, because the entry goes from `"Person\0"` to
`"Person\0\0"`. Every workspace would see `ontology.drifted: true` on upgrade even though
nothing changed semantically. This is a silent UX regression.

## Decision

The `parent_edges:` segment is appended to the canonical form **only when at least one parent
relationship is declared**:

```
# Flat ontology (no parents) — format unchanged from pre-#173:
"mode:{mode}\nentity_types:{entries}\nrelation_types:{entries}"

# Ontology with at least one parent:
"mode:{mode}\nentity_types:{entries}\nrelation_types:{entries}\nparent_edges:{edges}"
```

Parent edges are formatted as `"{child}\0{parent}"` pairs, sorted by child name, joined with
`"\0\0"`.

## Consequences

- Flat ontologies produce the same hash as pre-#173 — no spurious drift on upgrade.
- Adding any parent triggers a hash change → correct drift detection.
- Removing all parents produces the same hash as a flat ontology → correct drift detection.

## Constraint for future additions

Any future field added to the canonical form must follow the same conditional-inclusion rule
if it should not change existing hashes on upgrade. Only append a new segment when the
feature's data is non-empty; otherwise existing hashes change for all workspaces that don't
use the new feature.

Failing to follow this rule causes a one-time spurious `drifted: true` on upgrade for all
workspaces that don't use the new field — a confusing UX event that users must manually
clear by running `knowledge_reprocess_entity_types` or Recreate.
