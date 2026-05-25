# Feature Specification: Port Graphiti's Extraction Prompts to liminis-graph for Quality Parity (Ontology-Aware)

**Feature Branch**: `fabrik/issue-92`
**Created**: 2026-05-25
**Status**: Draft
**Input**: Re-filing of liminis-graph#82 (closed). The original PR #88 ported graphiti's prompt structure but was written before ontology landed; reconciling the two designs is real re-integration, not a rebase. This issue re-scopes the work to land cleanly on current main where ontology is the baseline.

## Background

Live extraction quality audit 2026-05-25 against demo-notebook (196 entities, 225 relationships) confirmed that the Rust extractor's ~5-line system prompts produce visibly worse output than graphiti's ~70-line tuned prompts:

| Defect | Examples | Graphiti rule that would catch it |
|---|---|---|
| Generic bare nouns | `City`, `Body`, `Interview`, `Phone Call`, `Meeting Prep` | "NEVER extract: generic common nouns" |
| Dates as entities | `Date` type with "Wednesday, March 11", "March 20" | "Do not create nodes for temporal information" |
| Pseudo-duplicates | `Alex Rodriguez-Chen` vs `Alexandra Rodriguez-Chen` | "Always use the most specific form" + name validation |
| Free-form entity types | 40+ types for 196 entities, lots of overlap | classify_nodes with configurable types |
| Free-form relation labels | `produced by`, `married to`, `is followed by`, `won`, `Set in` | "Derive relation_type in SCREAMING_SNAKE_CASE" |
| Run-on facts | `Q2 prioritisation session confirmation is a priority item slot on 09:30 all-hands` | "fact should paraphrase the original sentence" |
| Attributes leaking into edges | `Alex Rodriguez-Chen --[has email address]--> a.rodriguez-chen@nexusdyn.io` | separate `extract_attributes` prompt |
| Self-referential edges | `Tar Pits --[contains tar]--> Tar` | "source ≠ target" |

Estimated impact: **10–15% noise + ~5× search-quality loss** from inconsistent relation vocabulary.

### How this differs from #82

Issue #82 / PR #88 had to be closed because main moved while the PR was in flight:

- **#83 (ontology)** landed on main and added an `Ontology` type, an `ontology: Option<&Ontology>` parameter to the `Extractor` trait, and inline ontology-aware guidance in the extractor's system prompt. The `Extractor` trait now takes `(episode_body, group_id, ontology)`.
- **PR #88** designed a different `Extractor` trait taking `opts: ExtractOptions<'a>` and split extraction into a two-call pipeline (`do_extract_entities` + `do_extract_edges`), with prompts factored into a `prompts/` module — but had no ontology integration.

The two designs need to be reconciled coherently in a single new implementation, not via mechanical rebase.

**Critical prior art to reuse from PR #88's branch `fabrik/issue-82` (still in the repo, just closed):**

- `liminis-graph-core/src/prompts/extract_text.txt` — verbatim port of graphiti's `extract_text` system prompt (NEVER-extract list, 8 guidelines, examples)
- `liminis-graph-core/src/prompts/extract_message.txt` — graphiti's `extract_message` prompt
- `liminis-graph-core/src/prompts/extract_json.txt` — graphiti's `extract_json` prompt
- `liminis-graph-core/src/prompts/extract_edges.txt` — graphiti's `edge` prompt (entity validation, distinct rules, SCREAMING_SNAKE_CASE, temporal grounding)
- `liminis-graph-core/src/prompts/classify_nodes.txt` — graphiti's `classify_nodes` prompt
- `liminis-graph-core/src/prompts/mod.rs` — module structure with `SourceType`-keyed dispatch

These are the substantive work and should be reused verbatim. The new implementation builds the integration around them.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Ingestion Stops Producing Generic-Noun Entities (Priority: P1)

When the user ingests text containing common references like "the meeting", "her phone", "the interview", the extractor MUST NOT create entity nodes for those bare nouns.

**Independent Test**: Ingest a chunk containing "Alex picked up some supplies before the meeting and called his mother on her phone." Assert no entity nodes are created for `supplies`, `meeting`, `mother`, `phone` (bare forms). `Alex` is the only acceptable entity.

**Acceptance Scenarios**:

1. **Given** a text chunk containing generic bare nouns mixed with named entities, **When** ingested, **Then** entity nodes are created only for the named/specific entities.
2. **Given** a text chunk containing pronouns, **When** ingested, **Then** no entity nodes are named after pronouns.
3. **Given** a text chunk with dates and times, **When** ingested, **Then** no entity nodes are created for temporal expressions; dates land in edge `valid_at` / `invalid_at` fields.

---

### User Story 2 — Edge Relation Labels Use SCREAMING_SNAKE_CASE Vocabulary (Priority: P1)

Edges MUST use normalized SCREAMING_SNAKE_CASE relation labels (`fact` field or separate `relation_type`), derivable from the source predicate, consistent across calls.

**Independent Test**: Ingest 10 chunks asserting various authorship phrasings ("wrote", "authored", "penned"). All resulting edges use the same `AUTHORED` (or `WROTE`) label.

---

### User Story 3 — Edge Endpoints Validated Against Entity List (Priority: P1)

Edge endpoints (source + target names) MUST be names that appeared in the entity-extraction pass for the same episode. Mismatched edges are dropped.

---

### User Story 4 — Source-Type-Specific Prompts (Priority: P2)

The extractor MUST use distinct prompts for unstructured text vs conversational messages vs JSON data, matching graphiti's `extract_text` / `extract_message` / `extract_json` separation, dispatched by `SourceType`.

---

### User Story 5 — Distinct-Entity and No-Self-Reference Edges (Priority: P2)

The extractor MUST NOT produce edges where `source_entity_name == target_entity_name`, nor edges whose `fact` is a single-entity state ("Alice feels happy" without a second entity).

---

### User Story 6 — Ontology Guidance Flows Into the Right Prompts (Priority: P1)

When an `Ontology` is loaded for the workspace, its entity-types and relation-types MUST be injected into the appropriate ported prompts:

- Entity types → injected into `extract_text` / `extract_message` / `extract_json` prompts as the `<ENTITY_TYPES>` section that graphiti's prompts already define
- Relation types → injected into `extract_edges` prompt as the `<FACT_TYPES>` section
- Strict vs Open mode → the corresponding sentence ("Only extract entities whose type is..." vs "Prefer the listed entity types when they apply...")

This MUST replace the current inline-string assembly in `extractor.rs` — the ontology guidance lives in the prompt templates, not in Rust string concat.

**Acceptance Scenarios**:

1. **Given** an ontology with entity types `{Person, Organization, Paper}` and Strict mode, **When** the extractor is invoked, **Then** the system prompt sent to Anthropic contains an `<ENTITY_TYPES>` section listing those three types with the strict-mode instruction sentence.
2. **Given** no ontology, **When** the extractor is invoked, **Then** the system prompt sent to Anthropic has no `<ENTITY_TYPES>` section but still has all the static graphiti-ported content.
3. **Given** an ontology with relation types having `(source_type, target_type)` signatures, **When** the edge prompt is built, **Then** signatures appear in the `<FACT_TYPES>` section to guide the LLM.

---

### Edge Cases

- **Source type unknown / unset.** Default to `extract_text` prompt; log a debug note.
- **Episode mixes content types** (markdown with JSON code fences). Treat as text by default; do not split.
- **LLM ignores SCREAMING_SNAKE_CASE.** Post-process: uppercase, replace non-alphanumeric with underscore, collapse runs. Log when post-processing changes the label.
- **LLM returns entity name with different casing / whitespace.** Normalize on extraction; validation matches on normalized form.
- **Edge references entity by alias not in entity list.** No fuzzy matching v1; drop the edge.
- **Chunk has zero entities.** Skip edge pass; emit `{"entities": [], "edges": []}`.
- **`custom_extraction_instructions` supplied.** Append to the user prompt after dynamic content (as graphiti does); user content does not override system rules.
- **Ontology present but empty.** Behave as if no ontology — no `<ENTITY_TYPES>` / `<FACT_TYPES>` sections.
- **Ontology has entity types but no relation types** (or vice versa). Inject the section that has content; omit the other.

## Requirements *(mandatory)*

- **FR-001.** Adopt the `liminis-graph-core/src/prompts/` module from PR #88's closed branch `fabrik/issue-82` (the five `.txt` files + `mod.rs`) verbatim. Do not rewrite the prompt content.
- **FR-002.** Update the `prompts` module's prompt-building functions to accept an `Option<&Ontology>` parameter and inject `<ENTITY_TYPES>` / `<FACT_TYPES>` sections + mode-specific instruction sentences based on it. The placeholders graphiti's prompts already define (lines in `extract_text.txt`, `extract_edges.txt`) are the injection points.
- **FR-003.** Rewrite `extractor.rs`'s extraction pipeline to use a two-call structure: one call for entities (via `prompts::entity_system_prompt(source_type, ontology)` + per-episode user prompt), one call for edges (via `prompts::edge_system_prompt(ontology)` + the entity list + episode body).
- **FR-004.** Update the `Extractor` trait to take a single `ExtractOptions<'a>` struct argument (per PR #88's design), with fields: `episode_body`, `group_id`, `source_type`, `custom_instructions`, `reference_time`, **and `ontology: Option<&'a Ontology>`** (this is the integration #88 lacked).
- **FR-005.** Update call sites (`episode.rs`, handler dispatch, mock implementations) to construct `ExtractOptions` and pass it. Source type comes from the calling context (`knowledge_process_chunk` knows the document kind).
- **FR-006.** Entity-name validation: edges whose endpoints aren't in the same episode's entity list MUST be dropped post-extraction (warn-logged). Self-referential edges (source == target by name) MUST be dropped at the same point.
- **FR-007.** The edge `fact` field MUST be a paraphrase of the source sentence(s); the relation predicate MUST be normalized to SCREAMING_SNAKE_CASE (post-process if the LLM doesn't comply).
- **FR-008.** Remove the inline ontology-guidance string assembly currently in `extractor.rs` (introduced by #83). That logic moves into the `prompts` module per FR-002.
- **FR-009.** New test fixture `liminis-graph-core/tests/extraction_quality.rs` exercises the audit-flagged defect patterns + verifies ontology injection. Use recorded LLM responses, not live API.

## Success Criteria *(mandatory)*

- **SC-001.** Re-running demo-notebook ingestion on the new prompts produces zero entities matching the audit-flagged generic-noun patterns and zero `Date`-typed entities.
- **SC-002.** Re-running demo-notebook ingestion produces edges using ≤ 30 distinct normalized SCREAMING_SNAKE_CASE labels (down from 200+ free-form variants today).
- **SC-003.** Re-running demo-notebook ingestion produces zero self-referential edges and zero edges with endpoints not in the same episode's entity list.
- **SC-004.** With an ontology loaded, the system prompt sent to Anthropic contains an `<ENTITY_TYPES>` (or `<FACT_TYPES>`) section listing the ontology's declared types, plus the strict-mode or open-mode instruction sentence — verified by capturing the request body in a test stub.
- **SC-005.** With no ontology, the system prompt is the static ported content from `prompts/*.txt` with no ontology section — verified by request-body capture.
- **SC-006.** Source-type dispatch works: a JSON document and a text document run through different prompts (test assertion on the captured request body).
- **SC-007.** All existing tests pass; new extraction-quality tests pass.

## Assumptions

- **A1.** PR #88's `prompts/*.txt` files are good as ported (already verified against graphiti's source — A1 of issue #82).
- **A2.** Sonnet 4.x reliably follows the structured-instructions style; graphiti's production validates this.
- **A3.** Prompt content embedded at compile time via `include_str!` is fine; no need for runtime loading.
- **A4.** Source type is determinable from the calling context (handler knows whether the input is `.md` / `.json` / chat-style). If a caller can't determine, default to `Text`.
- **A5.** Test fixtures with recorded LLM responses are sufficient; live API calls in tests are flaky and expensive.
- **A6.** Reusing prompt content via cherry-pick or manual file copy from `fabrik/issue-82` is fine. The new branch doesn't need git ancestry with the old PR.

## Out of Scope

- Changing the ontology format (#83 already defined it).
- Cache-padding / cache-friendly restructuring beyond what graphiti already does.
- Live A/B against the Python service — replay-based comparison is sufficient.
- Changing the model. Same Sonnet 4.x.
- Re-extraction of existing workspaces.

## Source References

- **PR #88 (closed)** branch `fabrik/issue-82`: the prompts files + module structure. Cherry-pick or file-copy these.
- **Issue #82 (closed):** the original framing of this work; superseded by this issue.
- **Issue #83 / merged ontology PR:** the ontology that this implementation must thread through. Current `extractor.rs` already has inline ontology guidance that this work replaces with template-based injection.
- **Graphiti source:** `graphiti_core/prompts/extract_nodes.py` (`extract_text` / `extract_message` / `extract_json` / `classify_nodes`), `graphiti_core/prompts/extract_edges.py` (`edge`, `extract_attributes`) — the originals that the `.txt` files were ported from.
- **Cutover plan:** `ideas/cutover-plan.md` Stage 3 requires extraction quality to not regress against the Python service. This issue closes that risk.
