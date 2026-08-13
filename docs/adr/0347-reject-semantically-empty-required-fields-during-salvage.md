# ADR-0347: Reject Semantically-Empty Required Fields During Item Salvage

**Status**: Accepted
**Date**: 2026-08-13
**Issues**: #347 (finding deferred from #342/#344's review)

## Context

[ADR-0342](0342-salvage-malformed-extraction-items.md) added `salvage_items<T: DeserializeOwned>`,
a shared helper used at all four extraction-response parse sites (`parse_entity_response`/
`parse_edge_response` for `AnthropicExtractor`, `parse_oai_entity_response`/`parse_oai_edge_response`
for `OaiExtractor`), which drops and counts individual items that fail to *deserialize* rather than
failing the whole batch. That fix addresses structural validity only. An empty or whitespace-only
`String` satisfies `serde` — the field is present and typed correctly — so it passes through
`salvage_items` as a valid item even though it carries no usable content. This was raised during
#344's review and deliberately deferred: #342 was a 0.12.1 patch fixing reported data loss, and
adding semantic field validation across both providers and both item types was new logic on a path
that had just changed.

Two concrete gaps followed from this. First, a blank `fact` on an `ExtractedEdge` had no validation
anywhere in the pipeline and could reach storage — useless to any downstream consumer, unlike a
blank `source_name`/`target_name`, which at least lands in `edges_dropped_unresolvable` if it fails
to resolve. Second, `crates/eval/src/runner.rs` consumes `outcome.result` directly
(`.map(|outcome| outcome.result)`), upstream of `episode.rs`'s empty-name `retain` — the only
existing blank-name filter — so eval scored a slightly different item set than production ingest
would ever persist.

## Decision

### 1. A `RequiredFieldsPresent` trait, checked inside `salvage_items` via a trait bound

```rust
// crates/core/src/types.rs
pub(crate) trait RequiredFieldsPresent {
    fn is_well_formed(&self) -> bool;
}

impl RequiredFieldsPresent for ExtractedEntity {
    fn is_well_formed(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

impl RequiredFieldsPresent for ExtractedEdge {
    fn is_well_formed(&self) -> bool {
        !self.source_name.trim().is_empty()
            && !self.target_name.trim().is_empty()
            && !self.fact.trim().is_empty()
    }
}
```

```rust
// crates/core/src/extractor.rs
fn salvage_items<T: DeserializeOwned + RequiredFieldsPresent>(raw: Vec<Value>) -> (Vec<T>, usize) {
    let mut items = Vec::with_capacity(raw.len());
    let mut dropped = 0usize;
    for value in raw {
        match serde_json::from_value::<T>(value) {
            Ok(item) if item.is_well_formed() => items.push(item),
            Ok(_) | Err(_) => dropped += 1,
        }
    }
    (items, dropped)
}
```

A trait bound, rather than a closure parameter threaded through each call site, was chosen because
all four call sites already infer `T` as `ExtractedEntity`/`ExtractedEdge` and no other type is
ever passed to `salvage_items` — a trait bound requires editing zero call sites, while a closure
parameter would require editing all four to construct and pass a predicate.

The predicate returns a single `bool`, not a per-field tally, so an edge with multiple blank fields
(e.g. blank `fact` *and* blank `source_name`) is dropped and counted once, never once per field.
The blankness test is `str::trim().is_empty()`, matching `episode.rs`'s pre-existing empty-name
`retain` exactly — no new whitespace semantics.

A blank `source_name`/`target_name` is counted in `edges_dropped_malformed`, not
`edges_dropped_unresolvable`: the latter (ADR-0051) is a graph-state-dependent signal — an edge
whose endpoint didn't resolve against the *current* graph, but might resolve later. A blank name
can never resolve in any graph, at any time, so it is an invalid item, not an unresolved reference.
This is also the only choice compatible with the check's parse-time placement: resolution runs
later, in `episode.rs`'s Phase C, which a parse-time-rejected edge never reaches.

### 2. `episode.rs`'s empty-name `retain` stays code-identical — this is not the move ADR-0342 §3 rejected

ADR-0342's Alternatives Considered explicitly rejected moving the empty-name `retain` into
`salvage_items` and deleting it from `episode.rs`, reasoning that the `retain` is defense-in-depth
for every `Extractor` implementor, not just the two that go through `salvage_items` —
`ConfigurableExtractor` and `MockExtractor` build `ExtractionResult` directly and bypass
`salvage_items` entirely. Two existing tests
(`cross_episode_dedup.rs::test_empty_name_entity_skipped`,
`ontology_integration.rs::strict_mode_empty_name_out_of_vocab_entity_not_counted_in_tally`) rely on
exactly this bypass path and would break if the `retain` were removed.

This issue does not do that. It **adds** a parallel, earlier check inside `salvage_items` while
**leaving the `retain` fully in place, code-identical**. The rejected alternative was
relocate-and-delete; this issue's approach is add-alongside-keep — a different move, so ADR-0342
is not contradicted or made stale. For the two real providers, the `retain` becomes a no-op in
practice (their blank-name entities are now rejected earlier, at parse time), but it remains
load-bearing as defense-in-depth for test doubles that never call `salvage_items`.

The disjointness invariant `episode.rs` already documents — an item is only ever removed by one of
the two layers, never both, because parse-time salvage removes an item before it ever becomes a
`Vec<ExtractedEntity>` entry, and the `retain` only sees items that survived parse time — continues
to hold automatically. No new deduplication logic was needed to preserve it; the two layers were
already structured so that double-counting cannot occur, and adding a second reason for the parse
layer to remove an item does not change that structure.

### 3. `crates/eval/src/runner.rs` needs no code change

`runner.rs` calls `extractor.extract(opts).await.map(|outcome| outcome.result)`, consuming the
extractor's output directly and never calling `episode::add_episode` — this is why blank-name
entities reached eval scoring before this issue, bypassing the `retain` entirely. Placing the new
check inside `salvage_items` puts it upstream of *both* `episode.rs` and `runner.rs`, since both
consume the same `ExtractionOutcome`/`ExtractionResult` produced by the extractor's parse path. Eval
and ingest now see the same filtered item set by construction, with no `runner.rs` change required.
Had the check instead been added downstream in `episode.rs` (mirroring where the entity-name
`retain` already lives), eval/ingest parity would have required duplicating the same logic in
`runner.rs` — the outcome parse-time placement was chosen specifically to avoid.

## Consequences

- A blank or whitespace-only `fact`, `source_name`, or `target_name` on an `ExtractedEdge` is
  dropped before reaching storage and counted in `edges_dropped_malformed`, closing the primary gap
  this issue exists to close.
- A blank-name `ExtractedEntity` is now rejected at two layers for the two real providers
  (parse-time `salvage_items`, and — redundantly, but harmlessly — `episode.rs`'s `retain`, which
  never sees the item because it was already removed). The `entities_dropped_malformed` counter's
  behavior is unchanged: one count per blank-name entity, never two, since only one of the two
  layers ever actually removes a given item.
- `crates/eval/src/runner.rs`'s scoring now matches what `knowledge_process_chunk` would persist
  for the same extraction response, with no code change to the eval crate itself — a direct
  consequence of the check's parse-time placement, not independent work.
- `ConfigurableExtractor`/`MockExtractor`-based tests that rely on `episode.rs`'s `retain` catching
  a deliberately-constructed blank-name entity continue to pass unchanged, since the `retain` was
  not touched.
- `salvage_items`'s generic bound gains `+ RequiredFieldsPresent`; any future `T` passed to it must
  implement the trait. Today only `ExtractedEntity`/`ExtractedEdge` are ever passed.

## Alternatives Considered

- **Move the blank-name check (and the `retain`'s remaining coverage) into `episode.rs` instead of
  `salvage_items`**, keeping all semantic validation in one place: rejected. This is exactly the
  shape FR-005 (eval/ingest parity) says to avoid — `runner.rs` never calls `episode.rs`, so
  keeping validation there would require duplicating the same blank-field logic in `runner.rs` to
  achieve parity, rather than getting it for free from parse-time placement.
- **Delete `episode.rs`'s `retain` now that parse-time salvage covers the real providers**: rejected
  — this is the exact alternative ADR-0342 §3 already considered and rejected, for the same reason
  (it would silently narrow blank-name protection to only the two providers that go through
  `salvage_items`, breaking `ConfigurableExtractor`/`MockExtractor`-based tests that rely on it).
- **A closure/predicate parameter threaded through each of the four `salvage_items` call sites**,
  instead of a trait bound: viable, but requires editing all four call sites for no behavioral
  difference — the trait bound achieves the same result with zero call-site changes, since `T` is
  always `ExtractedEntity` or `ExtractedEdge` today.
- **Per-field drop counters** (e.g. distinguishing "dropped for blank fact" from "dropped for blank
  source_name"): rejected per the spec's own Edge Cases requirement — an edge with two blank fields
  simultaneously must be dropped and counted once, not once per field. A single `bool`-returning
  predicate makes double-counting structurally impossible rather than requiring a dedupe step.

## References

- Issue #347, deferred from #342's review (raised during PR #344)
- [ADR-0342](0342-salvage-malformed-extraction-items.md) — the per-item salvage mechanism this
  issue extends with semantic (not just structural) validation; its §3 and Alternatives Considered
  explain why `episode.rs`'s `retain` was kept rather than relocated, a decision this issue does not
  reverse
- [ADR-0051](0051-edge-endpoint-salvage-and-deferred-drop.md) — confirms `edges_dropped_unresolvable`
  is Phase-C-only, and why a parse-time-rejected blank-endpoint edge never touches that counter
