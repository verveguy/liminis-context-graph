# Feature Specification: Semantic search over Entity summaries

**Feature Branch**: `fabrik/issue-470`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "Entity summary is not semantically searchable: no summary_embedding column, so meaning-based retrieval is name-only"

## Background

This is the work issue for community report **#465** (submitted via the orac project). The report's underlying observation is real, but its diagnosis was root-caused against `origin/main` before filing and found to be narrower than reported — the correction below changes the shape of the fix.

`Entity` stores a `summary` field, but there is nowhere to put a vector representation of it. The node table defines `name_embedding FLOAT[{dim}]` and `summary STRING` — there is no `summary_embedding` column anywhere in the codebase. By contrast, `Episodic` already carries both `content STRING` and `content_embedding FLOAT[{dim}]`, so the asymmetry is specific to `Entity`, not a project-wide gap.

Consequently, entity assertion embeds only the entity's `name` into `name_embedding`; there is no missed call to wire up an embedding for `summary` today, because there is no destination column for it to write into. The only vector index on `Entity` is over `name_embedding`, so vector-based semantic retrieval is name-only by construction.

**Correction to the original report.** #465 states that asserted entities are discoverable "by name only." That is not accurate, and should not carry into this spec: `Entity` already has a full-text search (FTS) index over both `name` and `summary`, and entity retrieval is hybrid — FTS and vector results are fused by RRF (Reciprocal Rank Fusion). A query that shares vocabulary with an entity's summary already retrieves that entity today via the lexical (FTS) path.

The accurate framing, and the one this spec targets, is: **the summary is lexically searchable but not semantically searchable.** A keyword match against the summary works now; a paraphrase that shares no significant vocabulary with the summary does not, because nothing embeds the summary into a vector space. That gap is narrower than "invisible," but it still blocks the catalogue-style use case #465 describes, where callers query by meaning rather than by whatever slug-like `name` the entity happens to have been given.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Find an entity by what it means, not by its name (Priority: P1)

A caller of `knowledge_find_entities` knows roughly what an entity is *about* but not the exact name or wording used in its summary. Today, if their query shares no vocabulary with the entity's `name` or `summary`, the entity is not found — only vector similarity over `name_embedding` and lexical overlap over `name`/`summary` are available, and neither captures paraphrase-level meaning against the summary. This story delivers meaning-based retrieval: a query that paraphrases an entity's summary, using different words but the same meaning, retrieves that entity.

**Why this priority**: This is the core ask of the issue and of the originating community report (#465) — it is the capability that, once delivered, resolves the reported gap. Everything else in this spec exists to make this capability correct and complete (covering all entity-creation paths) and durable (covering databases that predate the change).

**Independent Test**: Assert an entity with a rich, natural-language `summary` and an unrelated, slug-like `name` (so that lexical overlap with a paraphrase is not possible by construction). Query `knowledge_find_entities` with a paraphrase of the summary that deliberately avoids the summary's significant vocabulary. The entity is returned.

**Acceptance Scenarios**:

1. **Given** an entity asserted directly (via the direct-assertion API) with a descriptive `summary` and a slug-like `name`, **When** `knowledge_find_entities` is queried with a paraphrase of the summary sharing no significant vocabulary with it, **Then** the entity is returned.
2. **Given** an entity created through the extraction path (not the direct-assertion API) with a descriptive `summary`, **When** `knowledge_find_entities` is queried with a paraphrase of that summary, **Then** the entity is returned, on equal footing with directly-asserted entities.

---

### User Story 2 - Existing behavior and existing databases keep working (Priority: P2)

Retrieval that already works today — exact and partial name matches, and lexical summary matches — must keep working exactly as before for callers who don't rely on the new capability. Separately, a database created before this change must continue to open and serve reads normally, and its existing entities must have a documented way to become semantically retrievable rather than remaining permanently stuck in the old, name-only-vector behavior.

**Why this priority**: This is a compatibility and rollout guarantee, not new capability. It's P2 because it gates whether User Story 1 can ship safely — regressing existing retrieval order, or breaking old databases, would be an unacceptable cost for the new capability — but it does not by itself deliver the feature's value.

**Independent Test**: Run a name-based query against an entity that was already retrievable by name before this change; confirm it's still returned, in the same relative order, when the new summary vector doesn't factor into that particular query. Separately, open a database created before this change and confirm it starts normally, then apply the documented backfill path and confirm previously-existing entities become retrievable by summary paraphrase.

**Acceptance Scenarios**:

1. **Given** a name-based query that returned a given entity, in a given order relative to other results, before this change, **When** the same query is run after this change, in a scenario where the summary vector does not contribute a match, **Then** the result and its relative order are unchanged.
2. **Given** a database created before this change, **When** the service opens it, **Then** it opens and serves existing reads normally, without requiring a rebuild.
3. **Given** an entity that existed in the database before this change, **When** the documented backfill path is applied, **Then** that entity becomes retrievable by a paraphrase of its summary, the same way a newly-created entity would be.

---

### Edge Cases

- An entity with no summary, or an empty-string summary, at creation time: no summary vector is meaningfully computable. Retrieval falls back to the existing name-based and lexical behavior for that entity, unaffected by this change.
- An entity's summary is changed by a later write (e.g., a re-assertion of an existing entity with an updated summary): the summary embedding reflects the current summary after the write, not a stale one from creation time — this follows from FR-002's requirement that summary embedding happens on every write path that carries a summary, not only at first creation.
- A backfill is only partially complete (some pre-existing entities have been reprocessed, others not yet): entities not yet backfilled retrieve via existing name/lexical behavior only, exactly as they do today; this must not be an error condition or block queries.
- A database created before this change is opened read-only or in a context where the migration step cannot run: the database must still open and serve pre-existing read paths (SC-004's "must not remain permanently unfindable" applies to the backfill path being available, not to migration being forced on every open).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `Entity` MUST gain a column to store a summary embedding vector, sized consistently with the project's other embedding columns (e.g. `name_embedding`). A database created before this change MUST continue to open and read existing rows normally, without requiring a rebuild, once the new column is introduced.
- **FR-002**: An entity's summary MUST be embedded into the new column on every path that writes an entity together with a summary — this explicitly includes both the direct-assertion path (`knowledge_assert_entity`) and the extraction-created path. A fix that covers only one path is not acceptable, since it would leave a corpus where semantic retrieval quality silently depends on how a given entity happened to be created.
- **FR-003**: A vector index MUST exist over the new summary-embedding column, and MUST be created and dropped in the same lifecycle as the existing `Entity` vector/FTS indexes (i.e. alongside `create_fts_indexes`/`drop_fts_indexes` and their HNSW/vector-index equivalents), so that bulk-load replay continues to work without a separate index-maintenance step.
- **FR-004**: `knowledge_find_entities` MUST incorporate summary-vector matches into its existing hybrid (RRF-fused) retrieval, so that User Story 1's acceptance scenarios are satisfiable through the normal query path — no new or separate query method is introduced for this. The specific fusion mechanics (e.g., whether the summary vector is an additional RRF input alongside the existing ones, or replaces one) are an implementation decision for the Research/Plan stages, not this spec — but whatever is chosen must be recorded along with the reasoning, since it changes ranking behavior for every existing caller of `knowledge_find_entities` (see User Story 2, Acceptance Scenario 1, for the regression constraint this is weighed against).
- **FR-005**: Existing entities (created before this change) MUST have a documented, working path to become semantically retrievable by summary — either through an automatic mechanism (e.g. as a byproduct of WAL replay, noting the related but separate work in #440) or through an explicit backfill operation. An entity written before this change MUST NOT be permanently and unconditionally excluded from meaning-based retrieval.
- **FR-006**: This change MUST NOT require any change to IPC or MCP tool schemas (request/response shape of existing methods, or the set of available methods). If implementation reveals that a schema change is in fact required, that MUST be surfaced explicitly during Research or Plan rather than folded silently into this work, since it would move the change out of purely-additive territory.

### Key Entities *(the feature involves data)*

- **Entity**: A node in the knowledge graph representing a named thing with a natural-language `summary` describing it. Already has a `name_embedding` vector and a `summary` string; gains a `summary_embedding` vector as part of this change.
- **Summary embedding**: A new vector representation of an `Entity`'s `summary` field, used for meaning-based (semantic) retrieval, as distinct from the existing `name_embedding` (name-based semantic retrieval) and the FTS index over `name`/`summary` (lexical retrieval).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An entity asserted with a rich `summary` and a slug-like `name` is retrieved by `knowledge_find_entities` from a paraphrase of the summary that shares no significant vocabulary with it. This is the criterion the originating report is actually asking for — a keyword-overlap match does not demonstrate it, since that already works today via FTS.
- **SC-002**: The equivalent paraphrase-retrieval query succeeds for entities created via the extraction path, not only entities created via direct assertion.
- **SC-003**: Existing name-based retrieval is not regressed: a name-based query returns the same results, in the same order, as it did before this change, in cases where the new summary vector is not a contributing match.
- **SC-004**: A database created before this change opens successfully after upgrade, and its pre-existing entities become semantically retrievable via the documented backfill path.

## Assumptions

- The embedding model/dimensionality used for `summary_embedding` is the same one already used for `name_embedding` and other existing embedding columns (per FR-001, "sized consistently with the project's other embedding columns").
- "Extraction-created path" refers to the entity-creation flow that runs during episode/fact extraction, as distinct from the direct-assertion API (`knowledge_assert_entity`) referenced in #379.
- A backfill path satisfying FR-005/SC-004 may be manual (an explicit operation an operator runs) or automatic (a byproduct of another mechanism, such as WAL replay per #440); this spec does not mandate which, only that one exists and is documented.
- No new IPC/MCP method or schema field is expected to be necessary to satisfy FR-004 (fusing the summary vector into existing hybrid retrieval); per FR-006, if that assumption turns out to be wrong, it must be raised explicitly rather than treated as in-scope by default.

## Out of Scope

- Batching the embedder (**#445**). This issue adds embedding calls, which makes batching more valuable, but does not depend on it and does not implement it.
- The embed-before-existence-check ordering issue in the assert handlers (**#444**).
- Summary/fact embeddings for edges (`RelatesToNode_`). `edge_name_and_fact` FTS already covers `fact` lexically today, and the same lexical-vs-semantic gap this issue addresses for entities likely exists there too — but that is a separate surface and should be filed as its own issue once this one lands, rather than expanding this issue's scope.

## Source References

- **#465** — the community report this issue implements, submitted via the orac project.
- **#440** — Recompute embeddings on WAL replay with a content-addressed cache; directly relevant to FR-005's backfill path, currently open/unimplemented.
- **#444** — assert handlers compute an embedding before the existence check and discard it on the update path; adjacent, currently open/unimplemented, explicitly out of scope here.
- **#445** — Embedder has no batch API; adjacent, currently open/unimplemented, explicitly out of scope here.
- **#379** — the direct-assertion API whose catalogue use case this issue unblocks.
- `crates/core/src/schema.rs` (`Entity` node table definition, FTS index `node_name_and_summary`).
- `crates/core/src/db.rs` (`entity_name_embedding_idx` vector index and its query path).
- `crates/core/src/handlers.rs` (entity assertion handler, `handle_assert_entity`).
