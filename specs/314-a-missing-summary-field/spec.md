# Feature Specification: A missing `summary` field discards the entire chunk, and is misreported as malformed JSON

**Feature Branch**: `fabrik/issue-314`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "`ExtractedEntity.summary` has no `#[serde(default)]`, so a model that returns structurally valid JSON but omits `summary` fails deserialization and the entire chunk's entities are discarded, then reported as `malformed` — the same bucket as genuinely unparseable JSON."

## Background

`ExtractedEntity.summary` is a required field with no serde default:

```rust
// crates/core/src/types.rs:94
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    pub entity_type: String,
    pub summary: String,          // <- no #[serde(default)]
}
```

Compare `ExtractedEdge` immediately below it, where `relation_type`, `valid_at` and `invalid_at` all carry `#[serde(default)]`.

When a model returns structurally correct JSON but omits `summary`, deserialization fails and **every entity in that chunk is discarded**. Observed live during the #306 failure-sidecar re-capture of `qwen3.6-35b-a3b` (2026-08-02):

| chunk | returned | outcome |
|---|---|---|
| `Apollo 11` | valid JSON, **29 entities**, `{"name", "entity_type"}` | all 29 discarded |
| `Astronaut` | valid JSON, **18 entities**, `{"name", "entity_type"}` | all 18 discarded |

Both had `finish_reason: "stop"` and used 745/4145 and 455/4096 tokens — nowhere near the budget. Nothing was truncated. The content parses cleanly as `{"entities": [{"name": "...", "entity_type": "..."}, ...]}`. The only defect is one absent field.

**This is only visible because of #306.** Before the failure sidecar, these appeared as a bare `structured_output.malformed` count with no payload, and were indistinguishable from genuinely broken output.

### Two problems, one root cause

**1. The data loss is disproportionate.** Discarding 29 good entities to avoid 29 empty summaries is plainly the wrong trade. Summaries feed embeddings and dedup, so an empty summary is a degraded entity — but a *dropped* entity is a missing one, and the edges referencing it lose an endpoint (ADR-0051's salvage path).

**2. The classification is wrong and misdirects diagnosis.** These are reported as `malformed`, which reads as "the model emitted broken JSON." It didn't. The same bucket also holds genuinely unparseable output — a third failure in the same run (`History of the Earth`, `Expecting ',' delimiter`) is real malformed JSON. Two distinct causes needing opposite responses are indistinguishable in the report.

### It biases every local-vs-hosted comparison

The Anthropic path uses `tool_use` with a JSON schema declaring `"required": ["name", "entity_type", "summary"]` (`crates/core/src/extractor.rs`), so the hosted model is **structurally prevented** from omitting `summary`. The OAI path (ADR-0041) uses `response_format: {"type": "json_object"}` plus `ENTITY_JSON_INSTRUCTION`, a text instruction with no enforcement — so a local model *can* omit it, and we then discard its whole chunk.

The result is a penalty applied only to local models, for a field the hosted model cannot fail to emit. Every local-vs-hosted number in `docs/history/extraction-eval-2026-07.md` is affected: the noise floor is unaffected (Haiku-vs-Haiku), but local error rates are inflated and local quality figures are computed over a corpus with its hardest chunks silently removed. `qwen3.6-35b-a3b`'s reported 3.07% error rate is substantially this defect rather than model failure.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A model that omits `summary` does not lose its work (Priority: P1)

A local model returns a chunk's worth of entities with `name` and `entity_type` but no `summary` field. Today, deserialization fails and the entire chunk is silently discarded. Instead, every entity should be retained with an empty summary, and the chunk should not be counted as an extraction failure.

**Why this priority**: This is the core defect. Discarding dozens of correctly-extracted entities over one missing string field is a disproportionate trade, and it directly corrupts local-vs-hosted quality comparisons (see Background).

**Independent Test**: Feed a response payload containing valid JSON entities that omit `summary` through the OAI extraction parse path and confirm the resulting `ExtractionResult` contains all of the entities, each with `summary` defaulted to an empty string.

**Acceptance Scenarios**:

1. **Given** an OAI-compatible model returns valid JSON entities with `name` and `entity_type` but no `summary`, **When** the response is parsed, **Then** all entities are retained in the result with `summary` defaulted to an empty string.
2. **Given** the same missing-summary response, **When** extraction telemetry is recorded for that chunk, **Then** the chunk is not counted in the `malformed` failure bucket.

---

### User Story 2 - Genuine malformed output is still distinguishable (Priority: P2)

A model returns output that is not valid JSON at all — a real parse failure. This must be classified differently from a response that parsed successfully but omitted a field, so an operator reading the failure report can tell "the model broke" apart from "we were strict."

**Why this priority**: The #306 failure sidecar exists specifically to make failure causes diagnosable. Conflating two failure modes with opposite fixes (fix the model's output vs. relax our schema) defeats that purpose and directly misled the `qwen3.6-35b-a3b` evaluation described in the Background.

**Independent Test**: Feed one payload that is not valid JSON and one payload that is valid JSON but fails schema validation on a genuinely required field, through the extraction parse path, and confirm the two produce different classification values in the failure telemetry/report.

**Acceptance Scenarios**:

1. **Given** a model response that is not valid JSON, **When** it is parsed, **Then** it is classified as unparseable content.
2. **Given** a model response that is valid JSON but fails validation on a genuinely required field (`name` or `entity_type` missing), **When** it is parsed, **Then** it is classified distinctly from the unparseable-content case.

---

### User Story 3 - The missing-summary rate is visible, not silent (Priority: P3)

An operator reviewing an extraction run can see how many entities arrived without a summary, since an empty summary still degrades embedding and dedup quality even though the entity itself now survives.

**Why this priority**: Fixing the data loss (User Story 1) must not trade a loud failure for a silent one. The degradation is real — dedup and embeddings both consume `summary` — so it needs its own visible signal, separate from the failure classification in User Story 2.

**Independent Test**: Run an extraction batch containing a mix of entities with and without summaries, and confirm the run's report/telemetry surfaces a count of entities that arrived without a summary, distinct from total entity count and from the failure classification counts.

**Acceptance Scenarios**:

1. **Given** a run in which N entities arrive without a summary, **When** the run's report is generated, **Then** it shows a count of N missing-summary entities, separate from total entity count and from failure counts.

---

### Edge Cases

- `summary` present but `null` rather than absent — must be handled the same as absent (default to empty string), not treated as a distinct case.
- `summary` present but empty string — already legal today and must remain legal. An explicit empty string and a defaulted-from-absent empty string are indistinguishable after parsing, so FR-004's missing-summary count covers both: they represent the same "no usable summary text" condition that matters for embedding/dedup quality.
- An entity missing `name` or `entity_type`: these are genuinely required, unsalvageable without an identity, and must still fail deserialization/validation for that entity.
- Cassette replay of a recording made before this fix: a cassette captured pre-fix, containing a chunk that was recorded as a `malformed` failure due to a missing `summary`, must still replay without introducing a discrepancy between what was recorded and what replay reports. (Whether this requires special-case handling or is naturally subsumed by the parser change is left to Research/Plan to determine from the cassette format.)

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `ExtractedEntity.summary` MUST tolerate absence in the source JSON, defaulting to an empty string, consistent with how `ExtractedEdge`'s optional fields (`relation_type`, `valid_at`, `invalid_at`) already behave. `summary: null` MUST be treated the same as an absent `summary` (default to empty string), not as a validation failure.
- **FR-002**: Every other struct on the extraction path (both Anthropic and OAI, entities and edges) MUST be audited for the same asymmetry — a field with no explicit default that a text-instructed (non-schema-enforced) model may plausibly omit — and fixed consistently. `name`, `entity_type`, and the edge fields already covered by `ExtractedEdge`'s existing `#[serde(default)]`s are exempt: `name`/`entity_type` are genuinely required (an entity without one is not salvageable per the Edge Cases above), and the edge fields are already handled.
- **FR-003**: Failure classification MUST distinguish *content that fails to parse as JSON at all* from *content that parses as JSON but fails schema/field validation*. The current single `malformed` classification value, which conflates both, MUST be split so a report reader can tell them apart.
- **FR-004**: The number of entities that arrive with no usable summary (absent, `null`, or empty string) MUST be counted per run and surfaced in the run's report/telemetry, separate from both the total entity count and the failure classification counts from FR-003.
- **FR-005**: The Plan stage MUST decide, and record the decision with rationale in an ADR numbered for this issue (`docs/adr/0314-<slug>.md`), whether the OAI path adds a repair pass — a second round-trip request that re-asks the model for just the missing summaries — versus accepting empty summaries as the interim degraded state (the default per the Assumptions below, and the option consistent with the Success Criteria, none of which require a repair pass). Whichever is chosen, the ADR must state why: cost/latency of a second round trip versus the value of a non-empty summary for embedding/dedup quality.
- **FR-006**: `docs/history/extraction-eval-2026-07.md` MUST be annotated to note that local-model error rates recorded in it are inflated by this defect, and that local quality figures in it were computed over a corpus with chunks lost to this defect silently excluded. The eval itself MUST NOT be re-run as part of this issue — only the existing numbers are annotated.

### Key Entities *(data contracts affected)*

- **`ExtractedEntity`**: the per-entity record produced by parsing a model's extraction response (`name`, `entity_type`, `summary`). Its `summary` field is the subject of FR-001.
- **Extraction failure classification**: the value recorded in telemetry/reporting to bucket why a chunk's extraction failed. Currently a single `malformed` value; FR-003 requires it to distinguish unparseable content from parsed-but-schema-invalid content.
- **Missing-summary count**: a new per-run counter (FR-004), independent of the failure classification, tracking entities that survived extraction but carry no usable summary text.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Re-running the `qwen3.6-35b-a3b` capture retains the `Apollo 11` (29) and `Astronaut` (18) entity sets that are discarded today.
- **SC-002**: That capture's error rate falls, and the residue is genuinely-broken output only.
- **SC-003**: The report distinguishes schema-shortfall (parsed but missing a field) from unparseable content.
- **SC-004**: No change to the Anthropic path, whose tool-use schema already guarantees `summary` is present.
- **SC-005**: No change to `open`/`freeform` rendered prompts — existing cassettes for those prompt modes stay valid (the same constraint #310 honoured).

## Assumptions

- An entity with an empty summary is materially better than no entity. This is the default behavior this issue delivers. If experience shows this is wrong for dedup or embedding quality, FR-005's repair pass is the documented alternative, and a follow-up issue would add it.
- This is a pre-existing defect, not a regression from #306/#307/#310 — those merely made it visible via the failure sidecar.
- The `summary` field's absence-tolerance (FR-001) and the failure-classification split (FR-003) are independent changes that both derive from the same root cause, and both are in scope for this issue.

## Out of Scope

- Re-running or updating the numeric results in `docs/history/extraction-eval-2026-07.md` — FR-006 only requires annotation of the existing numbers.
- Any change to the Anthropic extraction path or its `tool_use` schema (SC-004).
- Any change to `open`/`freeform` rendered prompt text or their cassettes (SC-005).
- Building a repair-pass mechanism, unless the Plan-stage decision required by FR-005 selects it.

## Source References

- `crates/core/src/types.rs` — `ExtractedEntity`, `ExtractedEdge` (FR-001, FR-002)
- `crates/core/src/extractor.rs` — Anthropic `tool_use` schema, OAI `ENTITY_JSON_INSTRUCTION` path, `malformed` classification sites (FR-002, FR-003)
- ADR-0041 — local OpenAI-compatible extraction adapter (why the OAI path lacks structural enforcement)
- ADR-0306 — extraction-failure sidecar and truncation visibility (the mechanism that made this defect observable)
- ADR-0310 — strict-mode reclassifies not drops (precedent for "salvage, don't discard" in this codebase)
- ADR-0051 — edge-endpoint salvage and deferred drop (why a dropped entity also costs its edges)
- `docs/history/extraction-eval-2026-07.md` — eval report requiring annotation per FR-006
