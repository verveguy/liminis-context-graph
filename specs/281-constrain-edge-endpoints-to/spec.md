# Feature Specification: Constrain edge endpoints to the extracted entity set, and stop banning the concepts edges hub on

**Feature Branch**: `fabrik/issue-281`
**Created**: 2026-07-29
**Status**: Draft
**Input**: User description: "Constrain edge endpoints to the extracted entity set, and stop banning the concepts edges hub on" — reported by @totalslacker in #202 (follow-up comment, 2026-07-28), after the #209 fix in PR #218 landed.

## Background

PR #218 fixed #209: a batch-local edge endpoint that *exists in the graph* now resolves correctly. This issue is a different, larger defect: edge endpoints that exist **nowhere in the graph**, because the entity-extraction pass was instructed not to extract them in the first place.

Extraction is two independent LLM calls (`crates/core/src/extractor.rs:139` and `:256`). Call 1 extracts entities; call 2 is handed those entity names and asked for facts "between the given ENTITIES". Two things make the second call emit endpoints the first call never produced:

1. **The prompts contradict each other.** `crates/core/src/prompts/extract_text.txt:2` instructs "NEVER extract vague or standalone abstract concepts," while `extract_edges.txt:6` asks for all factual relationships in the text. For most expository prose, the natural hub of the facts *is* an abstract concept. The entity pass is forbidden from producing it; the edge pass depends on it.

   Worse, the same prompt's closed ontology (`prompts/mod.rs:11-29`) offers `Concept: An abstract idea, principle, or theoretical framework`. On the measured page the model used 10 of the 16 available entity types and **never once used `Concept`** — the header ban wins over the type list. The system advertises a type it forbids populating.

2. **The schema does not enforce the constraint the prompt claims.** `extract_edges.txt:15-16` states "CRITICAL: Using names not in the list will cause the edge to be rejected," but `extractor.rs:286-288` declares `source_name` / `target_name` as a bare `{"type": "string"}`. The rejection is real; the prevention is advisory only — nothing stops the model from naming an off-list endpoint.

Edges whose endpoint resolves nowhere are dropped at `crates/core/src/episode.rs:291-313`, before the write lock is taken, using `retain`. This means the correct, lock-held resolution logic at `episode.rs:548-591` (which can also match against previously-persisted entities) never gets a chance to rescue them.

### Measured impact

Replay of the production prompts (`claude-haiku-4-5-20251001`, the `LlmRouter::from_env` default) over pages from @totalslacker's 4,374-page corpus export:

| page | chars | entities | edges | dropped |
|---|---:|---:|---:|---:|
| `wikipedia.org/wiki/LEMON_(C++_library)` | 4,797 | 10 | 9 | 0 (0%) |
| `wikipedia.org/wiki/Capacitive_Micromachined_Ultrasonic_Transducers` | 12,785 | 22 | 18 | 1 (5.6%) |
| `wikipedia.org/wiki/Global_warming` | 257,061 | 54 | 46 | **45 (97.8%)** |

On the third page, 45 of 46 edges are discarded across 24 distinct off-list endpoint names — `climate change` (9 edges), `carbon dioxide` (5), `sea level rise` (4), `global warming` (3), `ocean acidification`, `desertification`, `extreme weather`, and others. Every one is a concept the entity prompt bans. Neither call truncated (`stop_reason=tool_use`, 2,822 and 4,383 output tokens against an 8,192 cap), so this is not budget exhaustion — the extraction "succeeded" and then threw away the graph.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Ingesting expository prose retains its facts (Priority: P1)

A user ingests a document whose subject is an abstract concept (e.g. an encyclopedia article on a scientific or social phenomenon rather than a person or organization). The relationships stated in that document appear in the graph, rather than being silently discarded because the subject was never allowed to become an entity.

**Why this priority**: This is the core defect. Without it, entire categories of documents (anything with an abstract subject) lose nearly all of their extracted relationships, making the knowledge graph unreliable for exactly the content it's meant to capture.

**Independent Test**: Ingest the `Global_warming` fixture page and confirm the edge drop rate falls from 97.8% to within the success-criteria bound, and that the subject concept appears as an entity.

**Acceptance Scenarios**:

1. **Given** a document about an abstract subject, **When** it is ingested, **Then** the subject is present as an entity and edges to it are retained.
2. **Given** the 257KB `Global_warming` fixture, **When** it is ingested, **Then** ≤5% of extracted edges are dropped as unresolvable.

---

### User Story 2 - The edge model cannot name an endpoint that does not exist (Priority: P1)

The tool schema handed to the edge-extraction LLM call constrains the set of valid endpoint names, rather than relying on prompt text the model may ignore.

**Why this priority**: This is the structural fix that prevents the defect from recurring under prompt drift, model changes, or edge cases the entity pass didn't anticipate — a schema-level constraint is enforced regardless of how well the model follows instructions.

**Independent Test**: Inspect the `extract_edges` tool schema sent for a given batch and confirm `source_name`/`target_name` are constrained to that batch's entity names.

**Acceptance Scenarios**:

1. **Given** an entity list, **When** edges are extracted, **Then** every returned `source_name`/`target_name` is a member of that list, enforced by the tool schema rather than by prompt text.

---

### User Story 3 - A residual off-list endpoint is salvaged, not silently discarded (Priority: P2)

Even with schema-level constraints, an off-list endpoint may still occur (e.g. under the OpenAI-compatible path, where local models may not honor an `enum` constraint the way Anthropic's tool-use does). When one occurs, the system attempts to resolve it against the batch's known entities by similarity before dropping it, and reports what it drops.

**Why this priority**: This is a safety net and observability improvement, not the primary fix — it reduces residual data loss and makes remaining loss visible, but User Stories 1 and 2 address the bulk of the defect.

**Independent Test**: Feed a validation pass an edge naming an off-list endpoint that is semantically equivalent to a batch entity (e.g. a synonym or near-duplicate name) and confirm it resolves rather than drops; separately, confirm a genuinely unmatched endpoint is counted in the `process_chunk` result.

**Acceptance Scenarios**:

1. **Given** an off-list endpoint semantically equivalent to a batch entity, **When** validation runs, **Then** it resolves to that entity instead of being dropped.
2. **Given** an off-list endpoint that matches nothing, **When** it is dropped, **Then** the count is surfaced in the `process_chunk` result — not only on stderr.

---

### Edge Cases

- Entity list large enough that an `enum` bloats the request — measure; 54 names is trivial, but a pathological chunk could produce hundreds.
- An `enum` value containing characters that break schema validation (control characters are already stripped at `prompts/mod.rs:167`).
- Empty entity list — the edge-extraction call should be skipped rather than sent with an empty `enum`.
- Fuzzy matching must not collapse genuinely distinct entities (e.g. `carbon dioxide` vs `carbon monoxide`); the similarity threshold used for salvage needs verification on adversarial pairs.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `extract_edges` tool schema MUST constrain `source_name` and `target_name` to an `enum` of the entity names passed in that call.
- **FR-002**: The entity prompt MUST NOT forbid the entity types its own ontology defines. Either permit `Concept`-typed entities for subjects that pass the existing "could this have its own Wikipedia article" test, or remove `Concept` from the ontology and stop asking the edge pass to hub on concepts.
- **FR-003**: An off-list endpoint MUST be matched against the batch's entities by name-embedding similarity before being dropped, reusing the existing name-embedding and similarity-threshold machinery already used for deduplication.
- **FR-004**: `knowledge_process_chunk` MUST report the number of edges dropped for unresolvable endpoints in its result payload.
- **FR-005**: Edge validation MUST NOT permanently discard an edge before the write lock is taken when the lock-held resolution (which can also match against previously-persisted entities) could resolve it. Either defer the drop decision to the lock-held phase or make the pre-lock pass advisory only.
- **FR-006**: Behavior MUST hold for both the Anthropic path and the OpenAI-compatible path (ADR-0041) — an `enum` in the tool schema is honored differently by local models, so a post-hoc filter is still required regardless of schema enforcement.

### Key Entities *(if the feature involves data)*

- **Entity**: A named node extracted from source text in the first extraction call, typed against a closed ontology (Person, Organization, Concept, etc.).
- **Edge**: A factual relationship extracted in the second extraction call, naming a source and target entity by name; must resolve to two distinct, existing entities before being written to the graph.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The `Global_warming` fixture drops ≤5% of extracted edges, down from 97.8%.
- **SC-002**: Across the three fixture pages (`LEMON_(C++_library)`, `Capacitive_Micromachined_Ultrasonic_Transducers`, `Global_warming`), no page drops more than 5% of its edges.
- **SC-003**: A regression test asserts that an edge naming an off-list endpoint that is semantically equivalent to a batch entity is retained and attached to that entity.
- **SC-004**: Entity extraction on the `Global_warming` fixture yields at least one `Concept`-typed entity (currently zero of 54).

## Assumptions

- The corpus replay used the default `claude-haiku-4-5-20251001`; the drop rate on a Sonnet primary is unmeasured and may differ.
- Chunk-size-driven recall collapse (a related but distinct defect) is filed separately — this issue is about the endpoint contract holding at any chunk size.

## Out of Scope *(optional)*

- Input size guards for `process_chunk` (separate issue).
- The `NameIndex` scan-fallback regression from #219 (separate issue).

## Source References *(optional)*

- `crates/core/src/extractor.rs:139` — entity extraction call.
- `crates/core/src/extractor.rs:256` — edge extraction call.
- `crates/core/src/extractor.rs:286-288` — edge tool schema (`source_name`/`target_name` currently bare strings).
- `crates/core/src/prompts/extract_text.txt:2` — entity-prompt concept ban.
- `crates/core/src/prompts/extract_edges.txt:6,15-16` — edge-prompt hub dependency and unenforced name-list claim.
- `crates/core/src/prompts/mod.rs:11-29` — closed entity-type ontology, including `Concept`.
- `crates/core/src/prompts/mod.rs:167` — existing control-character stripping.
- `crates/core/src/episode.rs:291-313` — pre-lock `retain`-based edge drop.
- `crates/core/src/episode.rs:548-591` — lock-held endpoint resolution (including fallback to persisted entities).
- Prior work: #202 (original report), #209 / PR #218 (batch-local endpoint fix).
- ADR-0041 (OpenAI-compatible path).
