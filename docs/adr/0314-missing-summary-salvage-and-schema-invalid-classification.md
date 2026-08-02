# ADR-0314: Missing-Summary Salvage and `schema_invalid` Classification

**Status**: Accepted
**Date**: 2026-08-02
**Issues**: #314

## Context

`ExtractedEntity.summary` had no `#[serde(default)]`, unlike `ExtractedEdge`'s
`relation_type`/`valid_at`/`invalid_at` immediately below it. On the OAI-compatible path
(ADR-0041), which uses `response_format: {"type": "json_object"}` plus a text instruction
(`ENTITY_JSON_INSTRUCTION`) with no structural enforcement, a model could return otherwise
valid JSON — every entity present with `name` and `entity_type` — but omit `summary`.
Deserialization failed on the whole payload, and `parse_oai_entity_response` discarded **every
entity in the chunk**, then reported it as `structured_output.malformed` — the same bucket as
genuinely unparseable content.

This was found live during the #306 failure-sidecar re-capture of `qwen3.6-35b-a3b`
(2026-08-02): two chunks (`Apollo 11`, 29 entities; `Astronaut`, 18 entities) were valid JSON
missing only `summary`, and both were fully discarded and misreported as malformed alongside a
genuinely broken chunk (`History of the Earth`, `Expecting ',' delimiter`). The Anthropic path
(`AnthropicExtractor`) is structurally immune: its `tool_use` JSON Schema declares
`"required": ["name", "entity_type", "summary"]`, so the hosted model cannot omit the field.
The bug therefore penalized only local models, for a field they were never told is optional and
the hosted model cannot fail to emit — corrupting every local-vs-hosted comparison in
`docs/history/extraction-eval-2026-07.md`.

[ADR-0310](0310-strict-mode-reclassifies-not-drops.md) established the precedent this issue's
core complaint directly parallels: filtering *deletes* information, whereas salvaging what can
be salvaged preserves it. Strict-mode relation-type filtering was fixed to reclassify rather
than drop; this issue applies the same principle to the OAI parse path — a missing `summary` is
recoverable (default to empty), unlike a missing `name`/`entity_type`, which has no fallback and
must still fail.

[ADR-0306](0306-extraction-failure-sidecar-and-truncation-visibility.md) established the
`<cassette>.failures.jsonl` sidecar and its `classification` vocabulary — exactly three values:
`"http_error"`, `"truncation"`, `"malformed"`. This defect's second half is that `"malformed"`
conflated two causes needing opposite responses: content that never parsed as JSON at all (a
genuine model failure) versus content that parsed cleanly but failed schema validation (our
schema was stricter than necessary). FR-003 required splitting these so a report reader can
tell "the model broke" apart from "we were strict."

## Decision

### 1. `ExtractedEntity.summary` defaults to `""` on absence or `null`

A `deserialize_with` helper (not a bare `#[serde(default)]`, which does not cover explicit
`null` for a non-`Option` field) makes `summary` tolerate both an absent key and an explicit
`null`, collapsing both to `""` — the same as an explicit empty string, which was already legal.
The field's public type stays `String`, matching `episode.rs`'s existing no-presence-check
consumption; no downstream code needed to change. `name` and `entity_type` remain genuinely
required — an entity without an identity is not salvageable — and still fail deserialization if
missing, per the spec's Edge Cases.

**Scope, per FR-002's audit**: only `summary` needed this fix. The three single-field wrapper
structs in `extractor.rs`'s OAI path (`EntityPayload { entities }`, `EdgePayload { edges }`,
`TypesPayload { types }`) were explicitly considered and excluded — a wrapper's field being
absent means the model didn't return the expected top-level shape at all, not that one field
within a populated array is missing. Folding that in would silently convert "wrong shape" into
"zero found," a different and less honest failure mode than this issue addresses.

### 2. A fourth classification value, `schema_invalid`, mechanically derived from `serde_json::Error::classify()`

A shared `classify_parse_failure(&Error) -> &'static str` helper returns `"schema_invalid"` when
the error is `Error::Json` with `Category::Data` (valid JSON, but a required field was missing
or mistyped), and `"malformed"` for everything else — syntax/EOF/IO errors, and the `Error::Ipc`
cases with no JSON to even attempt validating (a missing `tool_use` block, a `null` tool input,
a missing message content field). Applied uniformly at all four `ParseError` emit sites
(Anthropic entities/edges, OAI entities/edges) to both `TelemetryEvent::StructuredOutputParse
.outcome` and `TelemetryEvent::ExtractionFailure.classification`, so the sidecar and the
lightweight counting event never disagree.

The Anthropic path gets this too, for mechanism-uniformity, even though its surface is narrow:
its `tool_use` schema means only the `Error::Ipc` "missing block"/"null input" cases can occur
there, and those fall through to `"malformed"` by construction (no JSON exists to classify
against). This does not change Anthropic-path behavior in practice — SC-004 holds — but keeps
one classification mechanism instead of two divergent ones.

### 3. Missing-summary visibility via a dedicated telemetry event, not a repair pass

`TelemetryEvent::EntitiesMissingSummary { ts_ms, model, chunk_key, entities_extracted,
missing_summary }` is emitted from `OaiExtractor::do_extract_entities`'s success arm only —
Anthropic never emits it (SC-004), since only `OaiExtractor` calls this emit site (not because
its tool_use schema forbids an empty `summary` string; `"required"` only guarantees the key is
present, not that its value is non-empty). Computed post-parse via
`entities.iter().filter(|e| e.summary.is_empty()).count()`. This mirrors the existing
`ExtractionTruncated` pattern — a dedicated, lightweight event rather than a field bolted onto
`StructuredOutputParse` (whose `outcome` is a closed pass/fail vocabulary, not a place for a
degraded-but-successful signal) — and gives production visibility for free through the existing
`StderrSink` JSONL stream. The eval harness's `CountingSink` gained a sibling
`MissingSummaryCounts` tally, and `crates/eval/src/report.rs` gained a parallel
`MissingSummaryReport`, following the same "aggregate-only, never folded into another metric"
convention as `TruncationReport`/`VocabularyComplianceReport`.

**FR-005's actual decision: no repair pass.** The spec required this issue to decide, and
record here, whether the OAI path should add a second round-trip request that re-asks the model
for just the missing summaries, versus accepting empty summaries as the interim degraded state.
**We chose to accept empty summaries and add no repair pass.**

Why: a second round-trip costs latency and tokens on every chunk where a local model omits
`summary` — plausibly frequent for some models (§ Context: `qwen3.6-35b-a3b` hit this on at
least 2 of 228 chunks in one capture) — and that cost is paid before there is any evidence the
degradation actually matters downstream. An empty summary is a real degradation (summaries feed
embeddings and dedup), but it is a *bounded* one: the entity, its type, and its edges all
survive, which is the disproportionate loss this issue exists to fix. FR-004's new
`EntitiesMissingSummary` visibility is what would surface if the degradation turns out to matter
in practice — at which point a repair pass is the documented next step, added in a follow-up
issue with actual rate data to justify its cost. Building the repair pass now, speculatively,
would be optimizing for a cost that has not been measured yet against a benefit that has not
been measured either. This is also the spec's own default (Assumptions) and the option every
Success Criterion is consistent with — none require a repair pass.

## Consequences

- A local model omitting `summary` no longer loses any entities or their edges — the core
  defect (SC-001, SC-002).
- The eval report and the failure sidecar can now distinguish "the model emitted broken JSON"
  from "the model omitted an unenforced field" (SC-003), so a report reader gets the right
  signal for which side needs fixing.
- The Anthropic path is untouched in practice (SC-004); the `open`/`freeform` rendered prompt
  text is untouched (SC-005) — this issue only changed parsing and telemetry classification,
  never prompt content.
- `docs/history/extraction-eval-2026-07.md`'s qwen3.6-35b-a3b section now carries an explicit
  correction: its recorded error rates and "excluded chunks" framing predate this fix and are
  upper bounds, not accurate measurements of that model's reliability (FR-006). The eval was not
  re-run — only annotated, per the spec's Out of Scope.
- Entities with an empty summary now reach embedding/dedup exactly as before this fix accepted
  them (episode.rs already had no presence check), but at a higher rate than when they were
  silently dropped. `EntitiesMissingSummary` telemetry is the mechanism for noticing if that
  rate becomes a real quality problem.
- The `<cassette>.failures.jsonl` sidecar's `classification` field now has four legal values
  instead of three. No cassette or sidecar consumer in this repo pattern-matches the vocabulary
  exhaustively except `lcg_eval::runner::CountingSink`, which was updated in the same change as
  the emit sites — see the Risks section of the Plan-stage output for why that pairing matters
  (a silent-drop match arm on an unrecognized value is exactly the "misreported" failure shape
  this issue is about, one layer up).
- A pre-fix cassette (recorded before this change, containing a chunk that failed as
  `"malformed"` due to a missing `summary`) replays identically after this fix.
  `RecordingExtractor::extract` only ever wrote a cassette record on success, so that chunk has
  no entry in the main cassette either before or after — only in the sidecar, which replay never
  reads. Nothing about which keys exist in the cassette changes retroactively. No special-case
  code was needed for this edge case.

## Alternatives Considered

- **`Option<String>` for `summary`** instead of a `deserialize_with` default-to-`""` helper:
  rejected because it would change the field's public type, and `episode.rs`'s existing
  consumption already treats an empty string as the "no summary" sentinel — introducing a second
  sentinel (`None`) for the same condition would create an unnecessary two-value encoding of one
  concept, contradicting the spec's own Edge Cases ("an explicit empty string and a
  defaulted-from-absent empty string are indistinguishable... they represent the same condition").
- **Extending FR-001's fix to the wrapper structs** (`EntityPayload`/`EdgePayload`/
  `TypesPayload`): considered and rejected — see Decision §1. A missing top-level key is a
  different failure shape than a missing field within a populated array, and conflating them
  would silently misreport "wrong response shape" as "the model found nothing."
- **A repair pass on the OAI path** (FR-005's alternative): rejected for now — see Decision §3.
  Left as the documented next step if `EntitiesMissingSummary` telemetry shows the missing-rate
  is a real problem.
- **Re-running the eval to get corrected numbers** for `docs/history/extraction-eval-2026-07.md`:
  explicitly out of scope per the spec (FR-006 requires annotation only) — a re-run is nontrivial
  effort and this issue's fix is validated by its own new tests, not by re-deriving the historical
  eval's numbers.

## References

- Issue #314
- [ADR-0041](0041-local-openai-compatible-extraction-adapter.md) — the OAI path's
  text-instructed, unenforced schema this issue's defect lived inside
- [ADR-0306](0306-extraction-failure-sidecar-and-truncation-visibility.md) — the
  `classification` vocabulary this issue extends from three values to four
- [ADR-0310](0310-strict-mode-reclassifies-not-drops.md) — the salvage-over-drop precedent this
  issue's FR-001 fix follows
- [ADR-0051](0051-edge-endpoint-salvage-and-deferred-drop.md) — why a dropped entity also costs
  its edges, the motivating harm this issue's data-loss fix avoids
- `docs/history/extraction-eval-2026-07.md` — annotated per FR-006
