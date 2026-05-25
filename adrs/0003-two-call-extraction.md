# ADR 0003 — Two-Call Extraction Pipeline

**Status:** Accepted
**Date:** 2026-05-25
**Issue:** #82

## Context

The original extraction pipeline made a single LLM call that asked for both entities and edges in one response. The edge system prompt included an `ENTITIES` placeholder to anchor edge endpoints to known entity names, but the placeholder was never populated with the actual extracted entities — it was always empty. This caused the LLM to hallucinate entity names in edge endpoints, produce self-referential edges, and omit edge relation types.

The graphiti reference implementation (MIT-licensed Python, which liminis-graph ports) uses two separate LLM calls: an entity extraction call followed by an edge extraction call that injects the previously-extracted entity names into the prompt.

## Decision

Replace the single-call pipeline with a two-call pipeline in `AnthropicExtractor::do_extract()`:

1. **Call 1 — entity extraction**: sends `entity_system_prompt(source_type)` + `entity_user_prompt(body, custom_instructions)`. Returns `Vec<ExtractedEntity>`.
2. **Call 2 — edge extraction**: sends `edge_system_prompt()` + `edge_user_prompt(&entity_names, reference_time, body, custom_instructions)`. Returns `Vec<ExtractedEdge>`.

Entity names extracted in Call 1 are injected into the `ENTITIES` section of the edge user prompt before Call 2 fires. This gives the LLM the complete entity set as a closed list to constrain edge endpoint names.

Post-extraction filters applied after Call 2:
- Drop edges where `source_name == target_name` (self-referential).
- Drop edges where either endpoint name is not in the extracted entity set.
- Normalize `relation_type` to SCREAMING_SNAKE_CASE.

## Consequences

**Positive:**
- Edge endpoint names are anchored to real extracted entities, eliminating the hallucinated-endpoint class of defect.
- SCREAMING_SNAKE_CASE normalization produces consistent relation type labels for dedup and search.
- The `ENTITIES` injection is explicit in the prompt text, making it auditable and testable without live LLM calls.

**Negative:**
- Two LLM calls per episode instead of one. Latency increases by one round-trip. For the target use case (background document ingestion), this is acceptable.
- If the entity call returns zero entities, the edge call still fires but will produce no edges (all endpoints would fail the entity-name filter). This is correct behavior but wastes one call. A future optimization could short-circuit the edge call when the entity set is empty.
