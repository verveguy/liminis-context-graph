# ADR-0003: Pass `Option<&Ontology>` as a call-time parameter to `Extractor::extract`

**Status**: Accepted  
**Date**: 2026-05-25  
**Issue**: #83 (Optional ontology support)

## Context

The ontology feature (issue #83) requires the entity-extraction LLM prompt to be customized per workspace based on a declared vocabulary. Three coupling strategies were considered:

1. **Call-time parameter on `Extractor::extract`** — pass `Option<&Ontology>` as a third argument
2. **Extractor-held state** — store the ontology inside `AnthropicExtractor` at construction time
3. **Wrapper/adapter pattern** — wrap the extractor in an `OntologyAwareExtractor` adapter

## Decision

Pass `Option<&Ontology>` as a call-time parameter on `Extractor::extract`.

The trait signature is extended to:

```rust
fn extract<'a>(
    &'a self,
    episode_body: &'a str,
    group_id: &'a str,
    ontology: Option<&'a Ontology>,
) -> BoxFuture<'a, Result<ExtractionResult, Error>>;
```

The call site in `episode::add_episode` passes `state.ontology.as_deref()`.

## Rationale

### Why call-time parameter

- **Consistent with ADR-0043**: `classify_entities` was added to the `Extractor` trait as a separate method when LLM call semantics differed. Adding `ontology` as a new parameter follows the same "extend the interface when input changes per-call" pattern rather than adding a new method.
- **Keeps extractors stateless**: `AnthropicExtractor` and `MockExtractor` remain pure functions of their inputs. Stateless implementations are easier to test and reason about.
- **Avoids reconstruction on ontology change**: v1.5 will add hot-reload support (FR-007). With call-time parameters, hot-reload requires only updating `AppState.ontology` (upgrading from `Option<Arc<Ontology>>` to `ArcSwapOption<Ontology>` for lock-free atomic swap) — no extractor reconstruction needed.
- **Test simplicity**: Tests pass different `Option<&Ontology>` values without needing to construct new extractors per test case.

### Why not extractor-held state (rejected)

- Requires reconstructing `AnthropicExtractor` (and thus `LlmRouter`) when the ontology changes — couples startup configuration to call-time vocabulary, making the v1.5 hot-reload path more invasive.
- Makes `MockExtractor` test setup complex: each test scenario needs its own mock instance, rather than just passing a different `Option<&Ontology>`.

### Why not wrapper/adapter pattern (rejected)

- Adds an indirection layer (`OntologyAwareExtractor<T: Extractor>`) that provides no benefit given the three known implementors. `MockExtractor`, `AnthropicExtractor`, and `LlmRouter` are the only implementations today; although `Extractor` is re-exported from `lib.rs` for testing convenience, there are no downstream implementors that would benefit from the wrapper pattern.
- Forces callers to wrap extractors at construction time, complicating `AppState::from_env`.

## `Option<Arc<Ontology>>` vs `ArcSwapOption<Ontology>` in `AppState`

`AppState.ontology` uses `Option<Arc<Ontology>>` rather than `ArcSwapOption<Ontology>` because:

- v1 requires a service restart to reload the ontology (FR-007). There is no concurrent writer, so `ArcSwap`'s compare-and-swap semantics are unnecessary.
- `ArcSwap` adds complexity that is only justified by hot-reload. v1.5 can upgrade `Option<Arc<Ontology>>` to `ArcSwapOption<Ontology>` when hot-reload is implemented without changing the call-time-parameter design.

## Consequences

- All three `Extractor` implementors (`AnthropicExtractor`, `MockExtractor`, `LlmRouter`) must be updated when the trait signature changes — contained blast radius since all are crate-internal.
- `MockExtractor` ignores the `ontology` parameter, returning its fixed result. Tests that exercise strict-mode filtering use the real filtering path in `episode::add_episode`, not the extractor.
- When issue #82 lands and refactors `AnthropicExtractor::do_extract`'s prompt structure, the ontology injection point (currently an append to `system_text`) may need to move to a different injection location in the refactored prompt builder.

### Update — Issue #92 (2026-05-25)

Issue #92 finalized the design anticipated above. Three concrete outcomes:

**1. `ontology` folded into `ExtractOptions<'a>`**

The three-argument `extract(episode_body, group_id, ontology)` signature was replaced by a single `ExtractOptions<'a>` struct:

```rust
pub struct ExtractOptions<'a> {
    pub episode_body: &'a str,
    pub group_id: &'a str,
    pub source_type: SourceType,
    pub custom_instructions: Option<&'a str>,
    pub reference_time: &'a str,
    pub ontology: Option<&'a Ontology>,
}
```

This co-locates all per-call extraction parameters in one place. `ExtractOptions` derives `Copy + Clone` (all fields are references or `Copy` enums) so `LlmRouter` can pass it to both primary and fallback without reconstruction.

**2. Placeholder-based ontology injection in `.txt` files**

The `prompts/` module uses compile-time `include_str!` for the five prompt files. Ontology types are injected via placeholder substitution at runtime:

- `{{ENTITY_TYPES_SECTION}}` in `extract_text.txt`, `extract_message.txt`, and `extract_json.txt` — replaced with the workspace entity-type list (or the default 16-type list when no ontology is present)
- `{{FACT_TYPES_SECTION}}` in `extract_edges.txt` — replaced with the workspace relation-type list (empty string when no relation types are declared)

This avoids showing two competing type vocabularies to the LLM simultaneously, which the earlier append-based approach would have caused.

**3. Two-call extraction pipeline**

`AnthropicExtractor::do_extract` was split into `do_extract_entities` (entity call) and `do_extract_edges` (edge call, skipped when the entity list is empty). Post-extraction edge validation in `episode::add_episode` drops self-referential edges and edges whose endpoints are not in the episode's entity list before they reach the DB commit phase.
