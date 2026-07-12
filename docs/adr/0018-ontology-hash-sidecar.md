# ADR-0018: Ontology Hash Sidecar for Drift Detection

**Date**: 2026-05-26
**Status**: Accepted
**Issue**: liminis-context-graph#98

## Context

When the user edits `ontology.yaml` and restarts liminis-context-graph, the existing entities and edges in the DB were extracted under the old ontology. The graph has mixed vocabulary. We need to detect this drift and surface it via `knowledge_status` so the UI can recommend a Recreate + re-ingest.

Four design decisions were made for this feature.

## Decision 1: Sidecar file over DB table

**Option A (chosen):** JSON sidecar at `<workspace>/.lcg/ontology-hash.json`.

**Option B (rejected):** New `OntologyHash` table in lbug with schema migration.

**Rationale:** A sidecar file requires zero migration code, is trivially inspectable and deletable by hand, and is consistent with the existing corrections-file pattern in `.liminis/`. The trade-off is that it is not atomic with the DB write — a crash between Phase C commit and the sidecar write leaves the sidecar stale. This best-effort inconsistency is explicitly accepted (spec A5): drift may be falsely reported after a crash, which is a safe direction to fail (prompts the user to Recreate rather than silently accepting a mixed-vocabulary graph).

## Decision 2: Semantic hash over byte hash

**Option A (chosen):** SHA-256 of a canonical serialization of the parsed `Ontology` struct.

**Option B (rejected):** SHA-256 of the raw YAML bytes.

**Rationale:** Users frequently add comments and reformat `ontology.yaml` during development. Byte-hashing would report drift for these cosmetic edits, causing false-positive "Recreate recommended" messages that erode trust. The canonical form is deterministic: `mode:{mode}\nentity_types:{sorted NAME\0DESC entries}\nrelation_types:{sorted NAME\0SRC\0TGT\0DESC entries}`. Name normalization is already applied by `load_ontology` (PascalCase for entities, SCREAMING_SNAKE_CASE for relations), so the hash is stable across equivalent naming variants.

## Decision 3: Sidecar path in `.lcg/`

The sidecar lives at `<workspace>/.lcg/ontology-hash.json`, co-located with the primary `ontology.yaml` at `<workspace>/.lcg/ontology.yaml`. The `.liminis/` directory is reserved for user-facing files (corrections YAML). A machine-generated, auto-updated file belongs in `.lcg/` alongside the ontology it tracks.

## Decision 4: Sentinel hash `"none"` for no-ontology state

`content_hash(None)` returns the sentinel string `"none"` (not a SHA-256 hash). This allows the sidecar to distinguish three states:

- **Sidecar absent, DB empty**: workspace has never been ingested. No drift reported (FR-007).
- **Sidecar present with hash `"none"`**: graph was ingested with no ontology. Drift is reported if an ontology is now loaded (FR-002).
- **Sidecar absent, DB has Episodic nodes**: workspace was ingested before #98 (no sidecar was ever written). Treated as "ingested with no ontology" — drift is reported if an ontology is now loaded. `has_prior_data: bool` is determined in `app_state::from_env` by calling `count_nodes("Episodic")` when no sidecar file exists. If the DB is unavailable at startup, `has_prior_data` defaults to `false` (conservative: may miss drift rather than false-positive it). This case is transient — after the first ingest post-upgrade, the sidecar is written and the DB check is never run again.

This avoids a special-case comparison branch at drift-detection time for the common path. The pre-upgrade DB-presence check is isolated to the sidecar-absent branch in `compute_drift`.

## Consequences

- A new sidecar file appears at `<workspace>/.lcg/ontology-hash.json` after the first ingest. Users should not edit it manually; it is regenerated on each successful ingest and WAL replay.
- `knowledge_status` gains `loaded`, `drifted`, and `drift_summary` fields in the `ontology` object. These fields are additive — existing clients that ignore unknown fields are unaffected.
- The `sha2 = "0.10"` crate is added to the workspace dependencies.
