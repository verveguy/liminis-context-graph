# Feature Specification: Optional Ontology Support — Workspace-Scoped Entity and Edge Vocabularies that Guide Extraction

**Feature Branch**: `fabrik/issue-83`
**Created**: 2026-05-25
**Status**: Draft
**Input**: Live extraction quality audit 2026-05-25 against demo-notebook surfaced that even with strong prompt engineering (filed as a sibling issue), the entity-type and relation-type vocabularies remain free-form per-call. A workspace whose users care about a specific domain (e.g., "scientific papers + experiments + concepts" or "people + meetings + decisions") should be able to declare that domain once and have extraction conform to it.

## Background

The graphiti pipeline supports per-call `entity_types` and `edge_types` parameters that, when present, inject taxonomy sections into the system prompt (`graphiti_core/prompts/extract_nodes.py` `_entity_types_section`, `extract_edges.py` `FACT_TYPES` block). With these set, extraction is narrowed to the declared types; without them, the LLM derives types ad-hoc.

liminis-graph's extractor accepts no such input today. Even after the sibling prompts-port issue ports graphiti's prompts verbatim, the entity-types and edge-types placeholders remain unfilled, and the system falls back to the LLM's free-form classification — leading to the 40+ overlapping types observed in the live audit (`Concept`/`Topic`, `Strategy`/`Initiative`/`Project`, etc.).

An **ontology** is the declared, persistent vocabulary for a workspace. It is **optional**: workspaces without one keep today's free-form behavior. Workspaces that declare one get consistent, queryable types.

A graph whose types are stable over time is queryable; one whose types drift per ingestion is just a dump. Multi-domain knowledge work (research lab vs. consulting firm vs. personal journal) wants different vocabularies — hard-coding a single taxonomy in the binary would be wrong; per-workspace declaration is the right scope. This is the natural complement to the sibling prompts-port issue; together they close the extraction-quality gap against graphiti. Ontology also lays the foundation for downstream features (curated entity reviews, schema-aware semantic search, type-aware dedup).

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Declared Entity Types Are Honored During Extraction (Priority: P1)

When a workspace declares an ontology that includes specific entity types (e.g., `Person`, `Organization`, `Paper`, `Experiment`), the extractor MUST assign one of those types to every extracted entity — or refuse to extract entities that fit none of them — depending on the ontology's strictness mode (see FR-006).

**Why this priority**: This is the user-visible outcome. Without it, the ontology has no effect on the graph.

**Independent Test**: Configure a workspace with an ontology containing `{Person, Organization, Paper}`. Ingest a text chunk mentioning a person at a company writing a paper. Assert all three extracted entities have one of those three type labels — not `Researcher`, `Company`, `Document` etc.

**Acceptance Scenarios**:

1. **Given** an ontology declaring entity types `{Person, Organization, Paper}`, **When** a chunk is ingested, **Then** all extracted entities have a label drawn from those three types.
2. **Given** an ontology declaring entity types `{Person, Organization, Paper}` in *strict* mode, **When** a chunk contains a candidate entity (e.g., a piece of equipment) that fits none of those types, **Then** the entity is not extracted.
3. **Given** an ontology declaring entity types `{Person, Organization, Paper}` in *open* mode, **When** a chunk contains a candidate entity that fits none of the declared types, **Then** the entity is extracted with a free-form type derived from the LLM output, but the ontology types remain preferred when they apply.

---

### User Story 2 — Declared Relation Types Are Honored During Edge Extraction (Priority: P1)

When the ontology declares relation types (e.g., `AUTHORED`, `AFFILIATED_WITH`, `CITES`, `CONDUCTED`), extracted edges MUST use those relation types whenever the relationship matches, in preference to deriving an ad-hoc SCREAMING_SNAKE_CASE label.

**Acceptance Scenarios**:

1. **Given** an ontology declaring relation types `{AUTHORED, AFFILIATED_WITH, CITES, CONDUCTED}`, **When** a sentence "Alice wrote the paper" is extracted, **Then** the edge `Alice --[AUTHORED]--> Paper` uses the declared `AUTHORED` rather than `WROTE`.
2. **Given** an ontology with relation types declaring `(Person, AFFILIATED_WITH, Organization)` signature, **When** "Alice works at Acme" is extracted, **Then** the resulting edge respects the signature: source type `Person`, target type `Organization`.
3. **Given** an ontology in *open* mode where the source sentence describes a relationship not in the declared vocabulary, **Then** the extractor falls back to the SCREAMING_SNAKE_CASE derivation from the prompts-port issue.
4. **Given** an ontology in *strict* mode where a candidate edge has no matching relation type, **Then** the edge is not extracted.

---

### User Story 3 — No Ontology = Today's Behavior (Priority: P1)

When a workspace declares no ontology, extraction MUST behave exactly as it does with the prompts-port issue alone — free-form types, SCREAMING_SNAKE_CASE-normalized relations, no degradation.

**Why this priority**: Ontology is opt-in. Workspaces today must not be forced into a declared vocabulary; the lift-and-shift cutover from the Python service depends on this.

**Acceptance Scenarios**:

1. **Given** a workspace with no ontology file, **When** ingestion runs, **Then** extracted entities have free-form types and edges use derived SCREAMING_SNAKE_CASE labels (per the prompts-port issue).
2. **Given** a workspace with an empty ontology file (no types declared), **When** ingestion runs, **Then** behavior is identical to no-ontology mode.
3. **Given** a workspace switching from no ontology to a declared one mid-corpus, **When** ingestion resumes, **Then** new chunks honor the ontology; pre-existing entities/edges are not retroactively re-typed (ontology migration is a separate issue, out of scope).

---

### User Story 4 — Ontology Definition Is Workspace-Scoped, Human-Readable, and Version-Trackable (Priority: P2)

The ontology lives in a file inside the workspace (`.lcg/ontology.yaml`), in a format a user can read, edit, and commit to git.

**Acceptance Scenarios**:

1. **Given** an ontology file in the workspace, **When** the user edits it and restarts the service, **Then** subsequent ingestions use the updated ontology.
2. **Given** an ontology file in a workspace under git, **When** the user reviews their commit history, **Then** ontology changes are visible diffs.
3. **Given** a malformed ontology file, **When** the service starts, **Then** it logs a clear error pointing at the syntax problem and continues without an ontology (degrades gracefully to no-ontology mode rather than crashing).

---

### User Story 5 — Ontology Includes Type Descriptions That Reach the LLM (Priority: P3)

Each entity type and relation type in the ontology MAY include a short description / instruction that gets injected into the prompt, helping the LLM disambiguate similar types.

**Acceptance Scenarios**:

1. **Given** an ontology declaring `Paper` with description "A peer-reviewed scientific publication, not a blog post or preprint", **When** the prompt is built, **Then** the description appears in the entity-types section visible to the LLM.
2. **Given** an ontology where two types share keywords (e.g., `Meeting` vs `Event`), **When** their descriptions differentiate them, **Then** the LLM uses the more specific type when descriptions match.

---

### Edge Cases

- **Ontology file edited while service is running.** v1: requires restart to take effect; the service logs this on startup. v1.5: file-watch + reload (see FR-007).
- **Ontology declares no entity types but declares relation types** (or vice versa). Each axis is treated independently; a missing section means free-form for that axis.
- **Strict mode + a chunk that produces zero in-vocabulary entities.** Result: no entities extracted, no edges extracted (edges need endpoints). Logged at debug level.
- **Relation type with signature `(Person, KNOWS, Person)` and a candidate edge `(Person, KNOWS, Organization)`.** In strict mode: edge is dropped. In open mode: extracted but the signature mismatch is logged at debug level.
- **An entity matches multiple declared types** (e.g., a person who is also an organization). The LLM's one-best-fit choice is accepted; the prompt phrasing encourages single-type assignment. Multi-typing is out of scope for v1.
- **Legacy `entity_types` env var** (from graphiti): not exposed in liminis-graph; if present in workspace `.env`, log a one-time hint pointing at the new ontology file.
- **Ontology file uses unsupported features** (e.g., file includes, external references). Rejected at parse time with a clear error; service degrades to no-ontology mode.
- **Entity type or relation type names with whitespace, mixed case, or punctuation.** Normalized at load time: entity types to PascalCase, relation types to SCREAMING_SNAKE_CASE. Any normalization that changes a value is logged.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: liminis-graph MUST read an optional ontology file from the workspace on startup. Default path: `<workspace>/.lcg/ontology.yaml` (with `.graphiti/ontology.yaml` fallback for workspaces still using the old directory name during the rename transition).
- **FR-002**: The ontology file format MUST support: declaring entity types (name + optional description), declaring relation types (name + optional description + optional `(source_type, target_type)` signature constraint), and declaring a global strictness mode (`open` | `strict`).
- **FR-003**: If the ontology file is missing, empty, or malformed, the service MUST fall back to no-ontology behavior and log a clear message. Parsing errors MUST NOT prevent the service from starting.
- **FR-004**: When an ontology is loaded, the entity-extraction system prompt MUST inject an `<ENTITY_TYPES>` section listing the declared types (with descriptions if present) — filling the placeholder the prompts-port issue already established.
- **FR-005**: When an ontology is loaded, the edge-extraction system prompt MUST inject a `<FACT_TYPES>` section listing the declared relation types (with descriptions and signatures if present) — filling the placeholder the prompts-port issue already established.
- **FR-006**: *Strict mode*: entities and edges that don't fit a declared type MUST be filtered out post-extraction. The prompt instructs the LLM to avoid out-of-vocabulary types, but server-side validation is the enforcement gate. *Open mode*: declared types are preferred by the prompt; free-form fallback is permitted when no declared type fits.
- **FR-007**: v1 requires a service restart to pick up ontology file changes; the startup log message MUST note this. v1.5 SHOULD add hot-reload via an IPC method or file-watch trigger that reloads the ontology without a service restart.
- **FR-008**: `knowledge_status` MUST include an `ontology` summary field — present/absent, mode, count of declared entity types, count of declared relation types — so the renderer can show ontology status to the user.
- **FR-009**: Existing tests pass unchanged. New tests cover: ontology-honored extraction, no-ontology-unchanged behavior, strict-mode filtering, malformed-ontology graceful-degrade, and signature-respected edge validation.

### Key Entities

- **Ontology**: The workspace-scoped vocabulary declaration. Loaded once at startup and held as `Option<Ontology>` in `AppState`. Nil when no file is present, empty, or malformed.
- **EntityType**: A declared node-classification label (PascalCase-normalized), with an optional disambiguating description.
- **RelationType**: A declared edge-label (SCREAMING_SNAKE_CASE-normalized), with an optional description and an optional `(source_entity_type, target_entity_type)` signature constraint.
- **Strictness mode**: Either `open` (free-form fallback allowed) or `strict` (out-of-vocabulary entities/edges are dropped post-extraction). A single mode applies to both axes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With an ontology declaring N entity types in strict mode, ingestion against demo-notebook produces zero entities with types outside the vocabulary. In open mode, ≤ 5% of entities fall outside the vocabulary.
- **SC-002**: With an ontology declaring M relation types in strict mode, 100% of extracted edges use a declared relation type. In open mode, ≥ 80% of edges use a declared relation type.
- **SC-003**: With no ontology file present, extraction behavior is identical to the prompts-port issue's free-form baseline — verified by running the same test fixture with and without an ontology file.
- **SC-004**: A malformed ontology file (intentional YAML syntax error) does not prevent the service from starting; the error message clearly identifies the line and reason.
- **SC-005**: The `knowledge_status` response always includes ontology summary fields (present/absent, mode, counts), regardless of whether an ontology is loaded.
- **SC-006**: An example ontology lives at `docs/examples/ontology.example.yaml` and is referenced from the README.
- **SC-007**: New tests pass; existing tests pass unchanged.

## Assumptions

- **A1.** The prompts-port sibling issue lands first, or both issues land in coordinated PRs. Without graphiti's prompt structure, there is nowhere clean for the ontology sections to inject.
- **A2.** YAML is the right format for v1. Future formats (TOML, JSON Schema, custom DSL) can be added without disrupting the runtime contract.
- **A3.** Per-workspace scope is correct for v1. Cross-workspace shared ontologies require identity and versioning thinking that is premature now.
- **A4.** `open` and `strict` are sufficient modes for v1. A future `warn` mode (emit free-form but report drift in telemetry) might be worth adding later.
- **A5.** Storing the ontology in `.lcg/ontology.yaml` is acceptable; `.lcg/` is already the canonical workspace state directory.
- **A6.** The indexing queue in liminis-app retains chunks across service restarts, so requiring a restart to reload the ontology does not create a data-loss window.

## Out of Scope

- Ontology editor UI (renderer-side feature, separate issue).
- Retroactive re-typing of pre-existing entities/edges when the ontology changes (separate "ontology migration" issue).
- Ontology inheritance, composition, or inclusion of other ontologies (v2; v1 = single flat file per workspace).
- Cross-workspace shared ontologies (v2; v1 = per-workspace only).
- Ontology-aware dedup tuning (separate optimization once ontology is in place).
- Live A/B measurement against an ontology-less baseline; the no-ontology test path (SC-003) is the regression guard.
- Per-call ontology override via IPC parameters (graphiti's design; liminis-graph uses workspace-level declaration instead).

## Source References

- **Graphiti's design**: `entity_types` and `edge_types` as per-call parameters on `add_episode`. liminis-graph adopts the same concept but moves declaration to a workspace-scoped config file, because per-call is an awkward seam in the IPC contract and most workspaces have a stable ontology.
- **Sibling prompts-port issue**: establishes the prompt placeholders (`<ENTITY_TYPES>`, `<FACT_TYPES>`) that this issue fills. They should ship in coordinated PRs.
- **Future downstream features** that benefit from a stable ontology: curated entity review UIs, schema-aware semantic search, type-aware dedup, ontology-driven workspace onboarding.
- `graphiti_core/prompts/extract_nodes.py` — `_entity_types_section` in the upstream graphiti library
- `graphiti_core/prompts/extract_edges.py` — `FACT_TYPES` block in the upstream graphiti library
