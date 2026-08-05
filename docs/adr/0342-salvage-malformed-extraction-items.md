# ADR-0342: Per-Item Salvage of Malformed Extraction Items

**Status**: Accepted
**Date**: 2026-08-04
**Issues**: #342 (community report #340)

## Context

`knowledge_process_chunk` deserializes the extraction LLM's entity/edge response as a single
`Vec<T>` at all four parse sites (`parse_entity_response`/`parse_edge_response` for
`AnthropicExtractor`, `parse_oai_entity_response`/`parse_oai_edge_response` for `OaiExtractor`).
If any one element in that array fails to deserialize — most commonly a missing `name` — the
entire array fails, and the whole chunk is rejected with a hard `-32000` error, even though every
other item in the response was well-formed.

Community report #340 hit this in production: a ~40-chunk document was lost in full because chunk
13's extraction response contained a single entity or edge without a `name`. The client treated
the chunk-level RPC error as fatal and abandoned the whole ingest rather than skipping one chunk.

This behavior was already internally inconsistent, which is the strongest argument that it was a
defect: `episode.rs`'s post-parse `extraction.entities.retain(|e| !e.name.trim().is_empty())`
already drops an entity with an explicit empty-string `name` without complaint, and the chunk
succeeds. A response with the *key entirely absent* — carrying the identical amount of usable
information — instead failed deserialization before that retain ever ran, and killed the whole
chunk. Missing, `null`, and empty-string `name` produced three different outcomes for what is, to
a human reading the model's intent, the same failure: "this one item has no identity."

[ADR-0314](0314-missing-summary-salvage-and-schema-invalid-classification.md) established the
direct precedent and the boundary this issue extends past. It added `deserialize_summary_or_default`
to `ExtractedEntity.summary` (a genuinely defaultable field) and explicitly *rejected* extending
salvage to the wrapper structs (`EntityPayload`/`EdgePayload`) — reasoning that a missing top-level
key (wrong response shape) is a different failure than a missing field inside a populated array.
This issue is the one that reaches *inside* the array: `name` cannot be defaulted the way `summary`
can (a nameless entity has no identity and no downstream meaning), so instead of defaulting the
field, the item itself is dropped — the same remedy `episode.rs:218` already applied to the
empty-string case, now applied uniformly and made observable.

[ADR-0051](0051-edge-endpoint-salvage-and-deferred-drop.md) established that `edges_dropped_unresolvable`
is authoritative only at Phase C (commit time), specifically to avoid two independently-counted,
easily-desynced drop passes. A missing `source_name`/`target_name` never reaches Phase C's
resolution logic today and still doesn't after this change — it's dropped at the new parse-time
salvage layer instead, a structurally distinct reason from an unresolvable (but present) endpoint
name. The two counters (`edges_dropped_malformed`, new here, and `edges_dropped_unresolvable`)
never overlap.

## Decision

### 1. Per-item salvage at all four parse sites, via a shared `salvage_items<T>()` helper

The two wrapper structs (`EntityPayload { entities: Vec<T> }`, `EdgePayload { edges: Vec<T> }`,
and their OAI-path equivalents) now hold `Vec<serde_json::Value>` instead of
`Vec<ExtractedEntity>`/`Vec<ExtractedEdge>`. A new helper,

```rust
fn salvage_items<T: DeserializeOwned>(raw: Vec<Value>) -> (Vec<T>, usize)
```

attempts to deserialize each element independently, collecting successes into the output `Vec<T>`
and counting failures into `dropped`. The wrapper-level check — is the `entities`/`edges` key
present at all, is its value an array — is unchanged and still hard-fails via the outer
`serde_json::from_value`/`from_str` call: switching the field's element type from `ExtractedEntity`
to `Value` does not change when that *outer* deserialize fails, only what happens to each element
once the array itself is known to exist. This is exactly FR-005's boundary (User Story 2): a
non-JSON body or a missing `entities`/`edges` key still fails loudly, unchanged from before this
issue.

`name`/`entity_type` (entity) and `source_name`/`target_name`/`fact` (edge) remain bare `String`
with no `#[serde(default)]` — a missing/`null` value still fails *that item's* deserialization,
which `salvage_items` then catches and counts rather than defaulting.

### 2. A new `ExtractionOutcome` wrapper, not new fields on `ExtractionResult`

`Extractor::extract()`'s return type changes from `Result<ExtractionResult, Error>` to
`Result<ExtractionOutcome, Error>`, where:

```rust
pub struct ExtractionOutcome {
    #[serde(flatten)]
    pub result: ExtractionResult,
    #[serde(default)]
    pub entities_dropped_malformed: usize,
    #[serde(default)]
    pub edges_dropped_malformed: usize,
}
```

`ExtractionResult { entities, edges }` is exhaustively field-literal-constructed at 56+ sites
across 13 files in this repo (test fixtures, `ConfigurableExtractor::new(vec![...])` queues, eval
crate scoring/pairwise code) — none of them go through `Extractor::extract()` directly.
`ConfigurableExtractor::extract()` is the single place that pops one `ExtractionResult` off its
queue and returns it; wrapping it into an `ExtractionOutcome` (via `.into()`, using
`entities_dropped_malformed: 0`/`edges_dropped_malformed: 0`) there confines this change's blast
radius to the `Extractor` trait's ~9 real implementors, leaving every one of those 56+ fixture
sites untouched. Adding the two counters directly to `ExtractionResult` would have forced every
one of those sites to be edited for no behavioral reason.

`#[serde(flatten)]` on `result` plus `#[serde(default)]` on the two counters keeps the JSON wire
shape backward compatible: a cassette record written before this change
(`{"entities":[...],"edges":[...]}`, no counter keys at all) still deserializes into
`ExtractionOutcome` through `ReplayingExtractor`, with both counts defaulting to `0`. This is
verified directly by `types.rs`'s
`extraction_outcome_deserializes_pre_342_cassette_shape_with_zero_drops` test — no golden-corpus
cassette ships in this repo (per ADR-0044), so this property needed an explicit unit test rather
than resting on inspection of the derive macros alone.

### 3. The `episode.rs:218` empty-name `retain` stays where it is, and now feeds the same counter

Rather than moving the empty-string check into the new parse-time `salvage_items` loop, it stays
exactly where it was, still running before the strict-mode reclassify loop (ordering that is
already load-bearing — see ADR-0310/ADR-0312 and the inline comment at that call site). It's
defense-in-depth: it protects every `Extractor` implementor, not just the two that go through
`salvage_items` (`MockExtractor`, `ConfigurableExtractor`, and any other test double can still
hand `episode.rs` an entity with an explicit empty-string name). Moving it would silently narrow
that protection to only the real providers.

What changes is that its drop count (`before.len() - after.len()`) is now folded into the same
`entities_dropped_malformed` counter that arrived from parse-time salvage via `ExtractionOutcome`,
rather than being silently discarded (FR-007). The two layers are provably disjoint and therefore
never double-count: parse-time salvage only ever removes items that failed to *deserialize*
(missing/`null` `name`), which by definition never reach `episode.rs` as an `ExtractedEntity` at
all; the `retain` only removes items that deserialized successfully but carry an empty-string
`name`. Missing, `null`, and empty-string `name` now all produce the same observable outcome: item
dropped, drop counted once, chunk succeeds — closing the inconsistency described in Context.

The two new counters — `entities_dropped_malformed`, `edges_dropped_malformed` — are added to
`AddEpisodeResult` and surfaced in `knowledge_process_chunk`'s IPC response, following the shape
of the existing `edges_dropped_unresolvable` counter (FR-003's decision, made in the spec, not
re-litigated here).

### 4. FR-004's "all malformed" case falls out of the existing empty-extraction short-circuit — no new branch

Both providers' `do_extract()` already special-cased `entities.is_empty()` after entity
extraction, short-circuiting to an empty `Ok(...)` without attempting edge extraction — this was
already the shape for "the model genuinely found zero entities." Once `do_extract_entities`
returns `(Vec<ExtractedEntity>, usize)` (items, dropped count) instead of just `Vec<ExtractedEntity>`,
a response where *every* item was malformed reaches this exact branch: `entities` is empty (all
dropped), so it takes the same short-circuit, but now carries a non-zero `entities_dropped_malformed`
count instead of the `0` a genuinely empty response would carry. No error path is triggered, and no
new branch was needed — this is FR-004's decision (an all-malformed response is a success,
quantitatively but not qualitatively different from a some-malformed response) implemented for
free by an existing code shape.

### 5. A new `StructuredOutputParse` outcome, `"salvaged"`, taking precedence over `"recovered"`

`TelemetryEvent::StructuredOutputParse.outcome` gains a fifth value, `"salvaged"`, emitted whenever
a call's `dropped > 0`, regardless of provider. Per FR-006, this must be distinguishable from
`"clean"` (nothing dropped, no defensive re-parse needed), `"recovered"` (OAI-only: the whole body
needed `extract_json_block` fence-stripping to parse, but every item was well-formed), and
`"malformed"`/`"schema_invalid"` (the whole call failed, nothing was salvageable at all — a
different, harder failure this issue does not touch).

**Precedence rule**: a response that is *both* defensively re-parsed (OAI fence-stripping needed)
*and* has one or more dropped items reports `"salvaged"`, not `"recovered"`. These are orthogonal
axes — one whole-body re-parse, one per-item defect — and a single `outcome` string cannot
represent both without a compound value (e.g. `"recovered_partial"`). We chose not to introduce a
compound vocabulary: item-level data loss is judged the more actionable signal for a report reader
(a dropped entity/edge is data that's gone; a defensively-reparsed-but-otherwise-clean body is not),
so it wins precedence. This is a deliberate simplification, not an oversight — a future issue could
split `outcome` into two orthogonal fields if both signals ever need to be visible simultaneously,
but no current consumer needs that.

**`AnthropicExtractor` now emits `StructuredOutputParse` on its success path for the first time.**
Before this issue, only `OaiExtractor`'s success arms called `self.sink.emit(TelemetryEvent::StructuredOutputParse{...})`
— `AnthropicExtractor`'s `EntityOutcome::Success`/`EdgeOutcome::Success` arms returned directly,
emitting nothing (confirmed in Research; ADR-0314's Consequences section noted "the Anthropic path
is untouched in practice" for its own change). Leaving this silent here would make the very
telemetry signal this issue exists to add invisible on the primary production provider — the exact
path #340's report came through. This is net-new emission volume on the Anthropic path (`"clean"`
or `"salvaged"`), not a change to any existing emission; call this out explicitly rather than
letting it look like unrelated scope creep in a diff.

### 6. Eval crate parity: `CountingSink`/`StructuredOutputCounts`/`StructuredOutputReliability` gain `salvaged`

`crates/eval/src/runner.rs`'s `CountingSink::emit` pattern-matches `outcome.as_str()` with a
`_ => {}` catch-all — exactly the silent-drop failure mode ADR-0314's Consequences section warned
about when it added `schema_invalid`. Left unmodified, every eval report generated after this
change would silently omit the new `"salvaged"` outcome from its tallies. `StructuredOutputCounts`
gains a `salvaged: u64` field (`total()` now sums five values, plus a new `salvaged_rate()`), and
`crates/eval/src/report.rs`'s `StructuredOutputReliability` / rendered human-readable summary /
golden-JSON test carry the same addition through `scoring.rs`. `run_backend()`'s production call
site unwraps `ExtractionOutcome` to its `.result: ExtractionResult` immediately
(`.map(|outcome| outcome.result)`) so `ChunkResult`'s existing `Result<ExtractionResult, String>`
shape, and every downstream `scoring.rs`/`pairwise.rs` consumer of it, is untouched — per-chunk
drop visibility for eval runs comes from the `StructuredOutputCounts.salvaged` aggregate tally
(fed by telemetry), not from threading raw counts through `ChunkResult`.

## Consequences

- A single malformed entity or edge no longer fails an entire chunk, and by extension no longer
  fails an entire multi-chunk document ingest when a client treats a chunk-level RPC error as
  fatal — the exact failure mode #340 reported (SC-001–SC-004, regression-tested end-to-end via
  `ipc_parity.rs`'s `test_knowledge_process_chunk_multi_chunk_document_survives_one_malformed_chunk`).
- `knowledge_process_chunk`'s IPC response gains two additive fields,
  `entities_dropped_malformed`/`edges_dropped_malformed` — existing clients unaffected, following
  the same additive-field convention as `edges_dropped_unresolvable` before it.
- A non-JSON extraction response, or one missing its `entities`/`edges` key entirely, still fails
  exactly as before — this issue narrows per-item tolerance only, and does not widen the parser to
  accept structurally broken responses (SC-005, User Story 2).
- `AnthropicExtractor`'s telemetry volume increases: it now emits `StructuredOutputParse` on every
  successful entity/edge extraction call, where before it emitted nothing on success.
- The eval harness's `StructuredOutputCounts`/`StructuredOutputReliability` report a fifth
  dimension (`salvaged`/`salvaged_rate`), so a future eval run can distinguish "the model degrades
  by omitting occasional item fields" from "the model is broken" — a rate worth watching the same
  way `EntitiesMissingSummary`/`schema_invalid` already are per ADR-0314.
- Old cassette recordings (pre-#342, no drop-counter keys) continue to replay identically —
  verified by a dedicated unit test rather than left as an unverified assumption about serde's
  `#[serde(flatten)]`/`#[serde(default)]` interaction.
- `crates/core/src/extractor.rs`'s `do_extract_entities`/`do_extract_edges` (both providers) now
  return `(Vec<T>, usize)` instead of `Vec<T>` — an internal signature change with no external
  surface, but touching every call site and every test that previously destructured a bare `Vec`.

## Alternatives Considered

- **Add `entities_dropped_malformed`/`edges_dropped_malformed` directly to `ExtractionResult`**
  instead of a new `ExtractionOutcome` wrapper: rejected — see Decision §2. Would have forced
  edits at 56+ exhaustive-literal construction sites across 13 files for a change that only 9
  `Extractor` implementors actually need to carry.
- **Move the `episode.rs:218` empty-name `retain` into the parse-time `salvage_items` loop**,
  unifying all three drop causes (missing/`null`/empty-string `name`) into one code path: rejected
  — see Decision §3. The `retain` is defense-in-depth for every `Extractor` implementor, including
  test doubles that never go through `salvage_items`; moving it would silently narrow that
  protection to only the two real providers.
- **A compound telemetry outcome** (e.g. `"recovered_partial"`) to represent "both defensively
  re-parsed and had dropped items" without a precedence rule: rejected — see Decision §5. Adds a
  second axis to what has so far been a flat, closed vocabulary, for a combination no current
  consumer needs to distinguish from plain `"salvaged"`.
- **Retrying the extraction call when a malformed item is detected**, rather than salvaging what
  parsed: rejected per the spec's own Assumptions — a retry costs a second API call, and neither
  provider sets `temperature`, so a retry is not guaranteed to produce a different (better) result
  even though #340's specific report happened to look deterministic.
- **Defaulting `name` to a sentinel value** (e.g. `"Unknown"`) instead of dropping the item:
  rejected per the spec's own root-cause analysis — a nameless entity has no identity and no
  downstream meaning; inventing one would create a node with no basis for later deduplication or
  resolution, a worse outcome than not creating it at all.

## References

- Issue #342, community report #340
- [ADR-0314](0314-missing-summary-salvage-and-schema-invalid-classification.md) — the direct
  precedent this issue extends: field-level defaulting for a genuinely defaultable field, and the
  explicit rejection of wrapper-level salvage that this issue's item-level salvage does not
  contradict (it salvages *inside* the array, not the wrapper key)
- [ADR-0051](0051-edge-endpoint-salvage-and-deferred-drop.md) — `edges_dropped_unresolvable`'s
  Phase-C-only authority, and why the new `edges_dropped_malformed` is a structurally distinct,
  non-overlapping counter
- [ADR-0310](0310-strict-mode-reclassifies-not-drops.md), [ADR-0312](0312-entity-strict-mode-reclassifies-not-drops.md) —
  the reclassify-not-drop precedent and the `episode.rs` ordering discipline this issue's counter
  threading had to preserve
- [ADR-0306](0306-extraction-failure-sidecar-and-truncation-visibility.md) — the
  `ExtractionFailure`/sidecar classification vocabulary this issue's `"salvaged"` outcome does
  *not* touch, since a salvaged-but-successful response is not a failure and never reaches the
  sidecar
- [ADR-0044](0044-llm-cassette-record-replay-seam.md) — the cassette record/replay seam whose
  backward compatibility this issue's `ExtractionOutcome` wrapper preserves
