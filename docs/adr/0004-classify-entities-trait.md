# ADR-0004: Add `classify_entities` to the `Extractor` trait

## Status

Accepted

## Context

The `knowledge_reprocess_entity_types` IPC method needs to call an LLM to assign specific type
labels (e.g. `Person`, `Organization`) to entities that were extracted with only the generic
`Entity` label. The existing `Extractor::extract` method takes an episode body and returns a full
`ExtractionResult` (entities + edges). It is driven by a prompt designed for knowledge-graph
extraction, which is not suited for per-entity type classification.

Three implementation options were considered:

1. **Standalone function**: a free function in `corrections.rs` that calls the Anthropic API
   directly using `reqwest`, bypassing the `Extractor` trait entirely.
2. **Synthetic episode**: reuse `Extractor::extract` with a crafted episode body that contains
   each entity's name and summary, then parse `entity_type` from the extraction result.
3. **New trait method**: add `classify_entities(&[(name, summary)]) -> Vec<String>` to the
   `Extractor` trait, implemented by `AnthropicExtractor`, `MockExtractor`, and `LlmRouter`.

## Decision

Add a new `classify_entities` method to the `Extractor` trait (option 3).

## Rationale

**Routing unification**: `LlmRouter` implements primary/fallback routing with telemetry and an
`AtomicBool` latch. Adding `classify_entities` to the trait means classification calls go through
the same routing path as extraction calls, getting the same fallback behaviour and telemetry
automatically. A standalone function would duplicate all of this.

**Testability**: tests use `MockExtractor`. If classification went through a standalone function,
tests couldn't easily control the classification output. With the trait method, `MockExtractor`
returns `vec![""; N]` (empty string = no reclassification), which is a well-defined and
predictable test behaviour.

**Prompt separation**: using a synthetic episode (option 2) is fragile — the extraction prompt
is designed to extract *new* entities and edges, not to re-classify a pre-existing entity list.
The classification prompt asks for exactly one type label per entity, which is a different task
requiring a different prompt structure. A separate method makes the intent explicit.

**Breaking change is contained**: all `Extractor` implementors (`AnthropicExtractor`,
`MockExtractor`, `LlmRouter`) are in this repository and are updated together. There are no
external implementors to break.

## Consequences

- Any future implementor of the `Extractor` trait (e.g. an OpenAI adapter) must implement
  `classify_entities` in addition to `extract`. The method is documented with a clear contract
  (same-length output, empty string = unknown type).
- `AnthropicExtractor` makes a separate API call with a distinct classification prompt. This
  incurs an extra LLM round-trip per `reprocess_entity_types` batch, but is necessary for
  prompt correctness.
- `MockExtractor::classify_entities` returns empty strings for all entities. Tests that call
  `knowledge_reprocess_entity_types` will see `reclassified_count: 0`, which is the expected
  no-op for a mock extractor.
