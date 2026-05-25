# Feature Specification: Port Graphiti's Extraction Prompts to liminis-graph for Quality Parity

**Feature Branch**: `fabrik/issue-82`
**Created**: 2026-05-25
**Status**: Draft
**Input**: Live extraction quality audit 2026-05-25 against demo-notebook (196 entities, 225 relationships extracted). Empirical evidence of the predicted quality gaps from running ~5-line system prompts vs graphiti's ~70-line tuned prompts.

## Background

The liminis-graph extractor (`liminis-graph-core/src/extractor.rs:113-160` for entities+edges; `:247-251` for classification) uses minimal system prompts:

- ~5 lines of generic "extract entities and relationships, return JSON" instruction
- No examples
- No forbidden-extraction list
- No entity type taxonomy
- No edge relation conventions
- No source-type adaptation (message vs JSON vs text)
- No temporal grounding rules
- No entity-name validation between extraction and edge passes

The graphiti Python service (`graphiti_core/prompts/extract_nodes.py` `extract_text`, `extract_message`, `extract_json`, `classify_nodes`; `graphiti_core/prompts/extract_edges.py` `edge`) has substantial, battle-tested prompts:

- Explicit "NEVER extract" list (pronouns, abstract concepts, generic nouns, bare relational/kinship terms, ambiguous bare nouns, sentence fragments)
- "Could this have its own Wikipedia article?" specificity heuristic
- Configurable entity-types section injected from context
- 8 numbered extraction guidelines (full names, specificity, possessor qualifiers, etc.)
- 2+ worked good/bad examples
- Separate prompts per source type
- SCREAMING_SNAKE_CASE relation vocabulary rule, with FACT_TYPES override
- Entity-name validation: edges must use names from the supplied ENTITIES list
- Distinct-entities rule: source ≠ target enforced
- Single-entity-state rule: "Alice feels happy" → BAD; find the second entity
- ISO 8601 temporal grounding with `valid_at` / `invalid_at` and REFERENCE_TIME for relative refs

The Rust port lost all of this. Same model (Sonnet), dramatically thinner prompting.

## Evidence from a Live Run

Audited demo-notebook ingestion 2026-05-25. Concrete defects matching the missing prompt rules:

| Defect | Examples | Graphiti rule that would have caught it |
|---|---|---|
| Generic bare nouns extracted | `City`, `Body`, `Interview`, `Phone Call`, `Doctor's Appointment`, `Meeting Prep` | "NEVER extract: generic common nouns or bare object words" |
| Dates as entities | `Date` type with values `Wednesday, March 11`, `March 20` | "Do not create nodes for temporal information like dates, times or years" |
| Pseudo-duplicate persons | `Alex Rodriguez-Chen` vs `Alexandra Rodriguez-Chen` | "Always use the most specific form from the text" + entity-name validation downstream |
| Free-form entity types | 40+ unique types for 196 entities — `Concept`/`Topic`, `Strategy`/`Initiative`/`Project`, `Activity`/`Process`/`Task`/`Event`/`Meeting` all overlap | classify_nodes prompt with configurable entity types |
| Free-form relation labels | `produced by`, `married to`, `is followed by`, `won`, `Won`, `Set in` (case + tense vary) | "Derive a relation_type in SCREAMING_SNAKE_CASE" |
| Run-on facts smushed into edge label | `Q2 prioritisation session confirmation is a priority item slot on 09:30 all-hands` | "The fact should closely paraphrase the original source sentence(s)" |
| Attributes leaking into edges | `Alex Rodriguez-Chen --[has email address]--> a.rodriguez-chen@nexusdyn.io` | Edges are relationships; attributes belong on nodes (handled by separate `extract_attributes` prompt) |
| Self-referential edges | `Tar Pits --[contains tar...]--> Tar` | "source_entity_name and target_entity_name NEVER refer to the same entity" |
| Misclassified entities | `Maric Jack` labeled `Character State` (it's a name) | Examples + entity types narrowing |

Rough estimate: **10–15% noise** in node set + **~5× search-quality loss** from inconsistent relation vocabulary (queries like "all WROTE relations" fail when the verb varies between `wrote`, `writes genre`, `is assigned to update with URL path versioning decision`).

## Why This Matters

- These prompts are the difference between "passable LLM dump" and "queryable knowledge graph." Without them, the eventual cutover to liminis-graph in production (Stage 3 of `ideas/cutover-plan.md`) will degrade extraction quality compared to today's Python-driven workspaces.
- The prompts already exist, were tuned over years, are MIT-licensed in the graphiti fork we already maintain, and are essentially structural copies (substitute Rust `format!` for Python `f"..."`).
- This is a prerequisite for cutover, not a polish item.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Ingestion Stops Producing Generic-Noun Entities (Priority: P1)

When the user ingests text containing common references like "the meeting", "her phone", "the interview", the extractor MUST NOT create entity nodes for those bare nouns.

**Why this priority**: this is the most visible noise source. Generic-noun entities clutter the graph, defeat semantic-search relevance, and erode user trust in graph contents.

**Independent Test**: Ingest a chunk containing a sentence like "Alex picked up some supplies before the meeting and called his mother on her phone." Assert no entity nodes are created with names matching `supplies`, `meeting`, `mother`, `phone` (bare forms). `Alex` (Person) is the only acceptable entity.

**Acceptance Scenarios**:

1. **Given** a text chunk containing generic bare nouns mixed with named entities, **When** ingested, **Then** entity nodes are created only for the named/specific entities; bare generic nouns are excluded.
2. **Given** a text chunk containing pronouns ("he said she would"), **When** ingested, **Then** no entity nodes named after pronouns are created.
3. **Given** a text chunk with dates and times ("last Wednesday at 3pm"), **When** ingested, **Then** no entity nodes are created for the temporal expressions; dates land in edge `valid_at` / `invalid_at` fields instead.

---

### User Story 2 — Edge Relation Labels Use SCREAMING_SNAKE_CASE Vocabulary (Priority: P1)

When the extractor produces edges, the relation label (`fact` field's primary verb or a separate `relation_type` field) MUST be a normalized SCREAMING_SNAKE_CASE term derivable from the source predicate.

**Why this priority**: free-form relation labels make the graph un-queryable for any non-trivial pattern. The current behavior — `wrote`, `produced by`, `married to`, `won`, `Won`, `is followed by`, `Set in` — means a user querying "all WROTE relationships" cannot find them all.

**Independent Test**: Ingest 10 chunks asserting various authorship relationships ("Adrian Tchaikovsky wrote Children of Time", "George RR Martin authored A Game of Thrones", "Asimov penned Foundation"). All resulting edges should use the same `WROTE` / `AUTHORED` SCREAMING_SNAKE_CASE label (consistent across the 10 chunks).

**Acceptance Scenarios**:

1. **Given** multiple sentences expressing the same relationship in different phrasings, **When** extracted, **Then** they produce edges with the same SCREAMING_SNAKE_CASE relation label.
2. **Given** a sentence with no clear relation predicate, **When** extracted, **Then** the extractor derives a sensible SCREAMING_SNAKE_CASE label rather than emitting prose.

---

### User Story 3 — Edge Endpoints Are Validated Against the Entity List (Priority: P1)

When the extractor produces edges, both `source_entity_name` and `target_entity_name` MUST be names that appeared in the entity-extraction pass for the same episode. Edges referencing entities not in the list are dropped.

**Why this priority**: orphaned-entity edges (where the endpoint isn't in any episode's entity list) are silent corruption — the graph claims a relationship to an entity that was never extracted.

**Acceptance Scenarios**:

1. **Given** an entity-extraction pass that produced entities {A, B, C}, **When** the edge pass produces an edge `A --[REL]--> D`, **Then** the edge is dropped and logged at warn level.
2. **Given** an entity-extraction pass produced entities {Adrian Tchaikovsky, Children of Time}, **When** the edge pass attempts `Tchaikovsky --[WROTE]--> Children of Time`, **Then** the edge is dropped (last-name-only doesn't match `Adrian Tchaikovsky`).

---

### User Story 4 — Source-Type-Specific Prompts (Priority: P2)

The extractor MUST use distinct prompts for unstructured text vs. conversational messages vs. JSON data, matching graphiti's `extract_text` / `extract_message` / `extract_json` separation.

**Why this priority**: today's prompt is one-size-fits-all. Text-style rules ("don't extract pronouns") apply differently to chat ("you" / "I" in messages may legitimately refer to speakers worth modeling) and to JSON ("don't extract field values as entities").

**Acceptance Scenarios**:

1. **Given** an episode tagged as conversational message, **When** extracted, **Then** the message-flavored prompt is used.
2. **Given** an episode tagged as JSON, **When** extracted, **Then** the JSON-flavored prompt is used.
3. **Given** an episode tagged as plain text (default), **When** extracted, **Then** the text-flavored prompt is used.

---

### User Story 5 — Distinct-Entity and No-Self-Reference Edges (Priority: P2)

The extractor MUST NOT produce edges where `source_entity_name == target_entity_name`, nor edges whose `fact` is just an attribute restatement of a single entity ("Alice feels happy").

**Acceptance Scenarios**:

1. **Given** a sentence about a single entity's state ("Alice was nervous"), **When** the edge pass runs, **Then** no self-edge is produced; if a second entity is implied ("Alice was nervous about Bob"), an edge `Alice --[NERVOUS_ABOUT]--> Bob` is produced instead.
2. **Given** the case observed in the audit ("Tar Pits contains tar"), **When** extracted, **Then** the edge is rejected as self-referential.

## Requirements *(mandatory)*

- **FR-001.** The Rust extractor's entity-extraction system prompt MUST be a structural port of graphiti's `extract_text` system prompt: explicit "NEVER extract" list, 8 numbered guidelines, at least 2 worked good/bad examples, possessor-qualification rule.
- **FR-002.** The Rust extractor's edge-extraction system prompt MUST be a structural port of graphiti's `edge` system prompt: entity-name validation rule, distinct-entities rule, single-entity-state rule, paraphrase rule, SCREAMING_SNAKE_CASE relation type rule, ISO 8601 `valid_at` / `invalid_at` rule.
- **FR-003.** The Rust extractor's classifier prompt MUST be a structural port of graphiti's `classify_nodes`, accepting a (post-issue) optional entity-types vocabulary and falling back to derive-a-type behavior when absent.
- **FR-004.** The Rust extractor MUST distinguish source types — text, message, JSON — and apply the appropriate prompt variant. Source type comes from `process_chunk`'s input.
- **FR-005.** Entity-name validation MUST drop edges whose endpoints aren't in the entity list produced for the same episode. Dropped edges are logged at warn level with the offending name(s).
- **FR-006.** Self-referential edges (source==target by name) MUST be dropped at extraction time, not later in dedup.
- **FR-007.** The edge `fact` field MUST be a paraphrase of the source sentence(s) rather than verbatim quoting; the relation predicate (a separate concept) MUST be a SCREAMING_SNAKE_CASE label.
- **FR-008.** A new test fixture (`liminis-graph-core/tests/extraction_quality.rs`) MUST exercise the audited defect patterns from the Evidence section and assert the new prompts eliminate them. Tests use recorded LLM responses, not live API calls.
- **FR-009.** Prompt content MUST live in a maintainable form (e.g., `liminis-graph-core/src/prompts/`) rather than inline `let system_text = "..."` blobs.

## Edge Cases

- **Source type unknown / unset.** Default to `extract_text` prompt; log a debug-level note that the source type was missing.
- **An episode contains a mix (markdown with JSON code fences).** Treat as text by default; do not try to split. The extractor sees the whole chunk.
- **The LLM ignores SCREAMING_SNAKE_CASE and returns prose anyway.** Post-process: uppercase, replace non-alphanumeric with underscore, collapse runs. Log when post-processing changes the label.
- **The LLM returns an entity name with trailing/leading whitespace or different casing than the source.** Normalize on extraction; validation matches on normalized form.
- **An entity is referenced in an edge by an alias not in the entity list.** No fuzzy matching in v1; drop the edge. Future enhancement is its own issue.
- **A chunk has zero entities extracted.** Skip edge pass; emit a single `{"entities": [], "edges": []}` result.
- **The user supplies `custom_extraction_instructions` in `process_chunk`.** Append to the user prompt as graphiti does, after the dynamic content. Do not let user content override system rules.

## Success Criteria *(mandatory)*

- **SC-001.** Re-running the demo-notebook ingestion on the new prompts produces zero entities matching the audit-flagged generic-noun patterns (`City`, `Body`, `Interview`, etc. as bare names) and zero entities of type `Date`.
- **SC-002.** Re-running the demo-notebook ingestion produces edges whose relation labels draw from a set of ≤ 30 distinct normalized SCREAMING_SNAKE_CASE labels (down from 200+ free-form variants).
- **SC-003.** Re-running the demo-notebook ingestion produces zero self-referential edges (source==target) and zero edges whose endpoints aren't in the same episode's entity list.
- **SC-004.** New test fixtures pass in CI; existing tests pass unchanged.
- **SC-005.** Source-type dispatch works: a JSON document and a text document run through different prompts (verified by test assertion on the captured system message).
- **SC-006.** Prompt files live in `liminis-graph-core/src/prompts/` and are maintainable independent of `extractor.rs` orchestration.

## Assumptions

- **A1.** Graphiti's prompts as they exist on the `liminis` branch today are good. They're the reference we copy from. Future re-tuning is its own work.
- **A2.** Sonnet 4.x reliably follows the structured-instructions style these prompts use. Verified empirically by graphiti's production use today; no model change needed.
- **A3.** Prompt content can be embedded in the Rust binary at compile time via `include_str!` if we choose to keep templates as `.txt` files. Either inline `format!` calls or external `.txt` work.
- **A4.** Source type is determinable from the calling context — `knowledge_process_chunk` knows whether it's processing a `.md` text body or a `.json` document. If not, A4 needs to be addressed first.
- **A5.** Test fixtures with recorded LLM responses are sufficient. Live API calls in tests would be flaky and expensive.

## Out of Scope

- Configurable entity-types / edge-types vocabulary (the "Ontology" feature, tracked separately as a sibling issue).
- Cache-padding / cache-friendly restructuring beyond what graphiti already does in its prompts.
- Live A/B against the Python service — replay-based comparison is sufficient.
- Changing the model. Same Sonnet 4.x as today.
- Re-extraction of existing workspaces. Improvements apply to new ingestions; backfill is a separate decision.

## Source References

- **graphiti prompts on the `liminis` branch:** `graphiti_core/prompts/extract_nodes.py` (extract_text/message/json, classify_nodes), `graphiti_core/prompts/extract_edges.py` (edge, extract_attributes). MIT-licensed; we already vendor the fork.
- **Live audit:** demo-notebook 2026-05-25 ingestion, 196 entities + 225 relationships. Defect patterns listed in Evidence section.
- **Related cutover dependency:** `ideas/cutover-plan.md` Stage 3 requires extraction quality to not regress against the Python service. This issue closes that risk.
- **Sibling issue:** Ontology support (filed separately) builds on this work by adding optional entity-types / edge-types vocabularies that fit into the placeholders graphiti's prompts already define.
