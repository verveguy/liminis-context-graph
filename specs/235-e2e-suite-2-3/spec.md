# Feature Specification: MCP Write/Mutation-Path E2E Suite Over the Real-Corpus Fixture

**Feature Branch**: `fabrik/issue-235`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "No test exercises the mutation surface against a realistic graph. Write-path tools are covered only by unit/integration tests over synthetic data (MockExtractor's fixed Alice + Acme Corp), or not at all end-to-end. That leaves the operations most likely to corrupt a real graph — bulk deletes, entity merges, corrections, whole-graph re-typing — unverified at scale and unverified through the MCP surface users actually call. Extend the MCP e2e suite with mutation coverage: apply write-path tools through tools/call against a graph seeded from the real-corpus fixture, then assert the resulting graph state, including that unrelated data is left intact."

## Background

Three of this cycle's community-reported defects were mutation-path bugs found only in
production use: #202 (dropped edges during ingest, because new-batch edge endpoints failed to
resolve against pre-existing entities), #203 (index loss under sustained write), and #205
(`knowledge_backfill_relation_types` polluting the relation-type space with fact-prefix
pseudo-types instead of genuine classification). Each was invisible to the existing test suite
because nothing mutates a realistic, at-scale graph end to end.

Today's coverage of the write-path tools is either synthetic-data unit/integration testing
(`MockExtractor`'s fixed two-entity `Alice`/`Acme Corp` fixture) or, for the read-path, a real
1,506-entity/2,392-relationship/228-episode corpus exercised through the actual MCP-over-stdio
transport (#234, `crates/service/tests/mcp_real_corpus_e2e.rs`). #234 also built and documented a
reusable seeding harness (`crates/service/tests/common/real_corpus.rs`:
`seed_real_corpus_workspace` / `SeededWorkspace`) specifically so this issue and a planned
admin/lifecycle follow-on suite would not have to re-derive "populate a temp workspace from the
committed real-corpus WAL fixture and rebuild it with zero LLM/embedder calls" from scratch.

This issue is that write-path follow-on (2 of 3 in the planned MCP e2e series: read-path #234,
write/mutation-path — this issue, admin/lifecycle-path — a further follow-on). It closes the gap
between "mutation tools are tested at MCP-transport granularity but only against a toy graph" and
"mutation tools are tested at real-corpus scale but only via direct in-process dispatch, never
through MCP" — neither of which would have caught #202, #203, or #205, since both bypass either
realistic scale or the actual client-facing transport.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Deletions remove exactly what they should, nothing else (Priority: P1)

As a maintainer, when I change `knowledge_delete_by_source`, `knowledge_delete_episode`, or
`knowledge_delete_chunk_episode`, I want a suite that deletes a known slice of a real,
1,500+-entity graph through `tools/call` and asserts the surviving entity/edge/episode counts
precisely, so that a regression that deletes too much (or too little, leaving orphans
unexpectedly reachable) is caught before merge instead of in production.

**Why this priority**: Bulk/targeted deletion is explicitly named in the issue as one of "the
operations most likely to corrupt a real graph," and is the simplest mutation category to get
subtly wrong (off-by-one scoping, missing group_id filters, cascading deletes that touch
unrelated data).

**Independent Test**: Run the deletion tests in isolation against a freshly seeded (or
snapshot-restored) copy of the real-corpus workspace; each test's before/after count deltas are
self-contained and don't depend on any other test having run first.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace with new content ingested under a distinct,
   test-owned source (see User Story 4), **When** `knowledge_delete_by_source` is called through
   `tools/call` for that source, **Then** exactly that source's episodes/entities/edges are
   removed, the response's reported counts match, and every entity/edge/episode count belonging
   to the original fixture content is unchanged from its pre-deletion value.
2. **Given** the same setup, **When** `knowledge_delete_episode` is called for a single episode
   UUID, **Then** only that episode is removed; entities extracted solely from it become orphaned
   (not deleted) per the tool's documented contract, and this orphaning is explicitly asserted
   (not merely unchecked).
3. **Given** content ingested via `knowledge_process_chunk` (which stamps a `chunk_id`),
   **When** `knowledge_delete_chunk_episode` is called for that `chunk_id`, **Then** exactly the
   episode(s) for that chunk are removed and orphan behavior is asserted the same way as Scenario
   2.
4. **Given** any of the above deletions, **When** the workspace's WAL is subsequently rebuilt from
   scratch (`knowledge_rebuild_from_wal`), **Then** the rebuilt graph's counts match the
   post-deletion counts exactly (WAL round-trip integrity).

---

### User Story 2 - Merges and corrections are safe, previewable, and reversible-by-preview (Priority: P1)

As a maintainer, when I change `knowledge_merge_entities`, `knowledge_apply_corrections`, or
`knowledge_validate_corrections`, I want a suite that merges two real entities (and applies a real
corrections file) against the real-corpus graph and asserts both the resulting graph shape and
that `dry_run`/`validate` previews exactly match what the real run produces, so that a regression
that drops edges during a merge, or where a preview lies about what will actually happen, is
caught before merge.

**Why this priority**: Entity merges are named explicitly in the issue as a graph-corrupting-risk
operation (dangling edge endpoints, duplicate edges after merge), and "`dry_run` results provably
match the applied result" is a standalone item in the issue's Acceptance Criteria — a preview that
diverges from reality is worse than no preview at all, since it actively misleads an operator
deciding whether to proceed.

**Why this priority**: Both operations mutate real entity/edge identity in ways a caller has no
way to safely retry if they get it wrong — a merge is not naturally reversible, and a corrections
file is meant to be trusted at face value.

**Independent Test**: Run against a real pair of alias-shaped entities and a real corrections file
targeting real fixture entities/edges; both can run against an isolated copy of the seeded
workspace independent of the deletion or reprocessing tests.

**Acceptance Scenarios**:

1. **Given** two real entities in the fixture graph that are genuine aliases of each other (or a
   deliberately-constructed alias pair, if the fixture's real content doesn't already contain one
   — a Research-stage determination), **When** `knowledge_merge_entities` is called with
   `dry_run: true` and then without it, **Then** the dry-run's reported merge plan (edges to be
   rewritten/deduplicated) matches exactly what the real merge produces, edges originally incident
   on either entity are preserved (rewritten to the canonical UUID, not dropped), no dangling edge
   endpoints reference the removed alias UUID afterward, and a traversal from the canonical entity
   afterward reflects the merged edge set.
2. **Given** a `knowledge-corrections.yaml` file written into the seeded workspace targeting real
   fixture entities/edges, **When** `knowledge_validate_corrections` is called and then
   `knowledge_apply_corrections` is called with `dry_run: true` and then without it, **Then** all
   three calls agree on what would change / did change, and the graph state after the real apply
   matches what was previewed.
3. **Given** either mutation above, **When** the workspace's WAL is rebuilt from scratch, **Then**
   the rebuilt graph reproduces the post-mutation state exactly.

---

### User Story 3 - Retyping is correctly scoped, idempotent, and abstains honestly (Priority: P1)

As a maintainer, when I change `knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`,
or `knowledge_canonicalize_relations`, I want a suite that runs each of them against the real
graph with a deterministic stub/replay extractor (no live LLM) and asserts scope filtering,
dry-run/applied parity, idempotency on re-run, and honest `UNCLASSIFIED` abstention, so that a
regression like #205 (a retyping tool polluting the relation-type space instead of classifying
genuinely) is caught before merge.

**Why this priority**: Whole-graph re-typing is explicitly named in the issue as a
graph-corrupting-risk operation, is directly implicated by #205's known defect class, and
"idempotency asserted where the tool claims it" is a standalone item in the issue's Acceptance
Criteria.

**Independent Test**: Runs entirely against a stub/replay extraction endpoint — no
`ANTHROPIC_API_KEY`, no network access beyond the suite's own loopback stub extractor and
embedder — so it can execute in CI with no live-LLM dependency and no per-run extraction-output
variance.

**Acceptance Scenarios**:

1. **Given** the seeded real-corpus workspace (with an ontology declaring entity/relation types
   available to it — a Research-stage determination of how that ontology is supplied), **When**
   `knowledge_reprocess_entity_types` and `knowledge_reprocess_relation_types` are each called with
   `scope: "untyped"`, `"off_ontology"`, and `"all"` in turn, **Then** each scope selects exactly
   the rows the tool's documented scope semantics describe (verified against the real graph's
   actual untyped/off-ontology/all row counts, not merely "some rows changed").
2. **Given** any of the above calls, **When** `dry_run: true` is compared against the same call
   without it, **Then** the dry run's reported plan (counts, per-row old/new type breakdown)
   matches exactly what the real run applies.
3. **Given** a completed (non-dry-run) reclassification, **When** the same call is issued again
   with the same scope, **Then** it is a no-op — no further writes, no further WAL entries,
   consistent with the tools' documented idempotency contract.
4. **Given** the stub extractor is configured to abstain on at least one real edge/entity,
   **When** reclassification runs, **Then** the corresponding row is written as `UNCLASSIFIED`
   (for relation types) or left unchanged (for entity types, per the tools' differing documented
   abstention contracts) — never force-assigned a nearest type.
5. **Given** the real graph's existing predicates, **When** `knowledge_canonicalize_relations` is
   run, **Then** its documented behavior holds against real data: edges already at their canonical
   type or already `UNCLASSIFIED` are left unchanged on re-run, and arrow-named "noise" edges keep
   their existing predicate — per the semantics documented in the README's relation-typing section.
6. **Given** any mutation above, **When** the workspace's WAL is rebuilt from scratch, **Then**
   the rebuilt graph reproduces the post-mutation state exactly.

---

### User Story 4 - Ingesting into the pre-populated graph resolves against pre-existing entities (Priority: P1)

As a maintainer, when I change `knowledge_process_chunk` or `knowledge_add_episode`, I want a
suite that ingests new content into the already-1,506-entity graph (using a deterministic
stub/replay extractor) and asserts that new edges whose endpoints already exist in the graph
resolve to those existing entities rather than creating duplicates or leaving dangling references,
so that the #202 regression (dropped/misresolved edges during ingest) is caught at the same
graph scale where it was originally field-reported, not just against a two-entity synthetic
fixture.

**Why this priority**: This is the #202 regression guard named explicitly as a standalone item in
the issue's Acceptance Criteria ("Ingesting into the pre-populated graph resolves edge endpoints
against pre-existing entities"), and is the one assertion in this issue that specifically requires
real scale (crossing the >1,000-entity hybrid-dedup threshold) rather than being satisfiable
against a small fixture.

**Independent Test**: Runs with a stub/replay extractor configured to emit at least one entity
name/alias that collides with a real entity already in the fixture graph, so cross-batch
resolution is genuinely exercised, not just assumed by graph size.

**Acceptance Scenarios**:

1. **Given** the seeded real-corpus workspace and a stub/replay extractor configured to extract an
   entity that names or aliases a real, pre-existing fixture entity, **When**
   `knowledge_process_chunk` or `knowledge_add_episode` is called through `tools/call` with content
   that would produce an edge to that entity, **Then** the resulting edge's endpoint resolves to
   the pre-existing entity's UUID (not a newly-created duplicate), verified by entity count and by
   inspecting the new edge's endpoint UUID directly.
2. **Given** the same setup, **When** the ingest completes, **Then** the operation is confirmed to
   have exercised the hybrid-dedup code path used above the 1,000-entity threshold (satisfied by
   construction, since the pre-populated graph already exceeds 1,000 entities before the new
   content is added).
3. **Given** the ingested content, **When** the workspace's WAL is rebuilt from scratch, **Then**
   the rebuilt graph reproduces the post-ingest state exactly, including the resolved (not
   duplicated) edge endpoint.

---

### User Story 5 - Full graph reset requires confirmation and leaves a rebuildable graph (Priority: P2)

As a maintainer, when I change `knowledge_clear_all`, I want a suite that confirms it refuses to
run without explicit confirmation and, once confirmed, leaves the workspace in a coherent state
that can be rebuilt from a preserved WAL, so that this irreversible operation can't silently
destroy data on a missing/defaulted confirmation flag.

**Why this priority**: `knowledge_clear_all` is the most destructive tool in the write surface and
is explicitly called out in the issue with its own bullet ("requires confirmation; leaves a
coherent empty graph that can be rebuilt again"), but unlike User Stories 1–4 it isn't implicated
by any of the three cited production defects, so it's ranked below the regression-guard stories.

**Independent Test**: Run last against a workspace copy that's about to be discarded anyway (since
this is the one mutation in the suite that intentionally destroys the entire graph), independent
of the other stories' assertions.

**Acceptance Scenarios**:

1. **Given** the seeded real-corpus workspace, **When** `knowledge_clear_all` is called through
   `tools/call` without `confirm: true` (omitted or `false`), **Then** the call is rejected and no
   data is removed (counts unchanged).
2. **Given** the same workspace, **When** `knowledge_clear_all` is called with `confirm: true` and
   `preserve_wal: true`, **Then** the graph is empty (zero entities/edges/episodes) but reports a
   coherent, valid empty state (not an error or degraded status), and a subsequent
   `knowledge_rebuild_from_wal` against the preserved WAL reproduces the original pre-clear graph.

---

### User Story 6 - Mutations are invisible and rejected under read scope (Priority: P1)

As a maintainer, I want every mutation tool covered by this suite to be confirmed absent from
`tools/list` and rejected if called anyway when the server is started with `--scope=read`, so that
a scope-gating regression that accidentally exposes a write tool to a read-only client is caught
against real data, mirroring the read-path suite's existing scope-gating coverage (#234) but for
the write surface.

**Why this priority**: Named as a standalone cross-cutting requirement in the issue ("under
`--scope=read` every one of these is absent and rejected"), and scope-gating bugs are a distinct
failure class from the mutation-correctness bugs the other stories cover — a tool being correct
when called doesn't imply it's correctly hidden when it shouldn't be callable at all.

**Independent Test**: Runs against a `--scope=read` server spawned from a seeded (but not
necessarily mutated) copy of the workspace; doesn't depend on any mutation from the other stories
having actually run.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace, **When** the server is spawned with `--scope=read`
   and `tools/list` is called, **Then** none of `knowledge_delete_by_source`,
   `knowledge_delete_episode`, `knowledge_delete_chunk_episode`, `knowledge_merge_entities`,
   `knowledge_apply_corrections`, `knowledge_reprocess_entity_types`,
   `knowledge_reprocess_relation_types`, `knowledge_canonicalize_relations`,
   `knowledge_process_chunk`, `knowledge_add_episode`, or `knowledge_clear_all` appear in the list.
2. **Given** the same server, **When** any one of those tools is called anyway through
   `tools/call`, **Then** the call is rejected as an unlisted tool (protocol-level error,
   consistent with #234's and `mcp_stdio.rs`'s existing scope-rejection precedent), and the
   underlying graph is verifiably unchanged.

---

### Edge Cases

- **Test isolation cost**: seeding the fixture's ~71 MB WAL and rebuilding indices takes roughly
  60–140s (per #217/#234's measured figures). Since every mutating test needs its own isolated
  starting state (one test's deletion must not affect another test's merge assertions), the suite
  must not pay a full from-scratch reseed per mutating test — see FR-013.
- **Deletion targets on fixture-native vs. suite-ingested content**: the fixture's originally
  captured episodes were ingested via `knowledge_add_episode`, not `knowledge_process_chunk`, so
  they may not carry the `chunk_id`/`source_file`-shaped `source_description` metadata that
  `knowledge_delete_by_source` / `knowledge_delete_chunk_episode` key off of. Deletion assertions
  may need to target content this suite ingests itself (via User Story 4's ingest calls) rather
  than the fixture's original episodes — see Assumptions.
- **`knowledge_reprocess_relation_types` requires a declared ontology**: unlike
  `knowledge_reprocess_entity_types`'s `untyped` scope (which works with no ontology via
  open-ended classification), every scope value of `knowledge_reprocess_relation_types` fails with
  a structured `{success: false, ...}` error if the workspace's ontology declares no relation
  types (ADR-0037). The suite must ensure the seeded workspace has a relation-type-bearing
  ontology available, or explicitly assert the structured-failure path where no ontology is
  configured — not silently skip the assertion.
- **Corrections file lifecycle**: `knowledge_apply_corrections` / `knowledge_validate_corrections`
  operate on a `knowledge-corrections.yaml` file expected to already exist in the workspace, not on
  parameters passed through `tools/call`. The suite must write that file into the seeded
  workspace's directory before calling either tool.
- **`knowledge_canonicalize_relations` is only partly idempotent by design**: per the README, a
  re-run skips an edge only when it's already at its target type or already `UNCLASSIFIED`; an
  edge whose classification *changes* on re-evaluation is overwritten. The idempotency assertion
  for this tool must therefore check "stable inputs produce a stable second run," not "any second
  run is always a no-op regardless of input drift."
- **`knowledge_clear_all` is the one genuinely destructive operation in this suite**: it must run
  against a workspace copy this suite is done with (last, or in total isolation), never against a
  shared seeded workspace another test in the same run still depends on.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The suite MUST reuse the seeding harness built by #234
  (`crates/service/tests/common/real_corpus.rs`: `seed_real_corpus_workspace` /
  `SeededWorkspace`) unmodified, per that issue's documented reusability guarantee.
- **FR-002**: The suite MUST isolate each mutating test's starting graph state from every other
  mutating test's, without re-paying a full from-scratch WAL replay + index rebuild
  (~60–140s) per test — e.g. via snapshot/restore of an already-seeded workspace's files, or
  another isolation mechanism a Research/Plan stage selects.
- **FR-003**: The suite MUST introduce a deterministic stub/replay extraction endpoint (no live
  LLM call, no `ANTHROPIC_API_KEY` required) that the seeded server is configured to call via
  `--extractor-http`, used by every acceptance scenario that requires entity/relation
  classification or extraction (`knowledge_process_chunk`, `knowledge_add_episode`,
  `knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`).
- **FR-004**: The suite MUST assert, for `knowledge_delete_by_source`, that deleting one source
  removes exactly that source's entities/edges/episodes and leaves every other count in the graph
  unchanged (User Story 1, Scenario 1).
- **FR-005**: The suite MUST assert, for `knowledge_delete_episode` and
  `knowledge_delete_chunk_episode`, targeted removal with explicit assertion of the tools'
  documented orphan behavior (entities extracted solely from the deleted episode remain in the
  graph, orphaned rather than cascade-deleted) (User Story 1, Scenarios 2–3).
- **FR-006**: The suite MUST assert, for `knowledge_merge_entities`, that merging two real
  entities preserves edges originally incident on either entity (rewritten to the canonical UUID,
  not dropped or duplicated), leaves no dangling edge endpoints referencing the removed alias
  UUID, and that a post-merge traversal reflects the merged edge set (User Story 2, Scenario 1).
- **FR-007**: The suite MUST assert, for `knowledge_apply_corrections` /
  `knowledge_validate_corrections`, a validate → dry-run apply → real apply round trip where all
  three agree on what changes, using a real corrections file targeting real fixture
  entities/edges (User Story 2, Scenario 2).
- **FR-008**: The suite MUST assert, for `knowledge_reprocess_relation_types` and
  `knowledge_reprocess_entity_types`, that each of the `untyped` / `off_ontology` / `all` scope
  values selects exactly the rows the tool's documented scope semantics describe, verified
  against the real graph's actual row counts per scope (User Story 3, Scenario 1).
- **FR-009**: The suite MUST assert, for both reprocess tools, that `dry_run: true` output
  (counts, per-row breakdown) exactly matches what the corresponding real (non-dry-run) call
  applies (User Story 3, Scenario 2).
- **FR-010**: The suite MUST assert idempotency: re-running a completed (non-dry-run)
  reclassification with the same scope and stable stub-extractor output is a no-op — no further
  writes, no further WAL entries (User Story 3, Scenario 3).
- **FR-011**: The suite MUST assert honest abstention: when the stub extractor is configured to
  abstain, `knowledge_reprocess_relation_types` writes the literal string `UNCLASSIFIED` to the
  edge's `relation_type`, and `knowledge_reprocess_entity_types` leaves the entity's type
  unchanged — per the tools' documented, differing abstention contracts (ADR-0037) (User Story 3,
  Scenario 4).
- **FR-012**: The suite MUST assert, for `knowledge_canonicalize_relations` against real
  predicates, that edges already at their canonical type or already `UNCLASSIFIED` are left
  unchanged on re-run, and that arrow-named "noise" edges keep their existing predicate — per the
  semantics documented in the README's relation-typing section (User Story 3, Scenario 5).
- **FR-013**: The suite MUST assert, for `knowledge_process_chunk` and `knowledge_add_episode`
  ingesting into the pre-populated (1,506-entity) graph with the stub extractor configured to
  reference a pre-existing fixture entity, that the new edge's endpoint resolves to that
  pre-existing entity's UUID rather than creating a duplicate — the #202 regression guard,
  exercised above the 1,000-entity hybrid-dedup threshold (User Story 4, Scenarios 1–2).
- **FR-014**: The suite MUST assert, for `knowledge_clear_all`, that a call omitting `confirm:
  true` is rejected with no data removed, and that a confirmed call with `preserve_wal: true`
  leaves a coherent, reportable empty graph state whose preserved WAL can be rebuilt to reproduce
  the pre-clear graph (User Story 5).
- **FR-015**: The suite MUST assert, for every mutation tool listed in FR-004 through FR-014
  above, that after the mutation a full `knowledge_rebuild_from_wal` reproduces the post-mutation
  graph state exactly (WAL round-trip integrity).
- **FR-016**: The suite MUST assert, against a `--scope=read` server spawned from a seeded
  workspace, that every mutation tool listed in FR-004 through FR-014 is absent from `tools/list`
  and rejected as an unlisted tool if called anyway, with the underlying graph verifiably
  unchanged (User Story 6).
- **FR-017**: The suite MUST make zero outbound live-LLM or real-embedder network calls end to
  end (no `ANTHROPIC_API_KEY`, no network access beyond the suite's own loopback stub extractor
  and stub embedder), verifiable independently, mirroring #217/#234's explicit call-counting
  discipline.
- **FR-018**: The suite MUST be gated the same way as #234's `mcp_real_corpus_e2e.rs` — excluded
  from the default per-PR `cargo test --release` gate, with an automatic trigger on push to
  `main` (and/or `workflow_dispatch`) so it doesn't rot unrun.
- **FR-019**: `cargo fmt --all` and the project's clippy gate (`cargo clippy --all-targets -- -D
  warnings` / `cargo clippy --release -- -D warnings`) MUST remain green with the new suite added.

### Key Entities

- **Seeded MCP workspace (isolated copy)**: A per-test (or per-test-group) isolated instance of
  the real-corpus workspace built via #234's `SeededWorkspace`/`seed_real_corpus_workspace`, safe
  for this suite to mutate without contaminating other tests.
- **Stub/replay extraction endpoint**: The new deterministic, no-live-LLM extraction stub this
  issue introduces, analogous to `spawn_stub_embedder`, driving every ingest/reclassification
  assertion in this suite.
- **MCP mutation assertion**: A before/after graph-state assertion (exact counts, exact edge
  endpoints, dry-run/applied parity, idempotency, or WAL round-trip) checked through `tools/call`
  against the isolated seeded workspace, rather than through direct in-process dispatch.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Every mutation tool named in the issue (`knowledge_delete_by_source`,
  `knowledge_delete_episode`, `knowledge_delete_chunk_episode`, `knowledge_merge_entities`,
  `knowledge_apply_corrections`, `knowledge_validate_corrections`,
  `knowledge_reprocess_relation_types`, `knowledge_reprocess_entity_types`,
  `knowledge_canonicalize_relations`, `knowledge_process_chunk`, `knowledge_add_episode`,
  `knowledge_clear_all`) has at least one assertion proving it changed exactly what it should and
  left the rest of the graph intact.
- **SC-002**: For every tool supporting `dry_run` (`knowledge_apply_corrections`,
  `knowledge_merge_entities`, `knowledge_reprocess_entity_types`,
  `knowledge_reprocess_relation_types`, `knowledge_canonicalize_relations`), the dry-run result
  provably matches the applied result.
- **SC-003**: Idempotency is asserted for every tool that claims it
  (`knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`,
  `knowledge_canonicalize_relations`).
- **SC-004**: Ingesting into the pre-populated (1,506-entity) graph via `knowledge_process_chunk`
  / `knowledge_add_episode` is shown to resolve new edge endpoints against pre-existing entities
  — the #202 regression guard — at real scale (above the 1,000-entity hybrid-dedup threshold).
- **SC-005**: For every mutation asserted in this suite, a post-mutation
  `knowledge_rebuild_from_wal` reproduces the post-mutation graph state exactly.
- **SC-006**: The suite makes zero live LLM or real-embedder network calls end to end,
  independently verifiable (no API key, no network beyond the suite's own loopback stubs).
- **SC-007**: `cargo fmt --all` and the project's clippy gate remain green with the new suite
  added; the suite runs within its own gated CI budget (excluded from the per-PR critical path,
  per FR-018), not adding to it.

## Assumptions

- This issue reuses #234's `SeededWorkspace`/`seed_real_corpus_workspace` unmodified (FR-001);
  it does not re-derive or fork the seeding logic.
- The exact isolation mechanism for per-test mutation safety (FR-002) — snapshot/restore of the
  seeded workspace's `db_path`/`wal_dir` files between tests, a copy-on-write temp workspace, or
  another approach — is a Research/Plan-stage decision; this spec only requires that isolation
  exist and that it not force a full from-scratch reseed per mutating test.
- The exact design of the stub/replay extraction endpoint (FR-003) — a keyed lookup by input
  content, a canned fixed response, or a small scriptable stub server analogous to
  `spawn_stub_embedder` — is a Research/Plan-stage decision. It must be deterministic and must
  support at least: emitting an entity that names/aliases a real pre-existing fixture entity (User
  Story 4), and abstaining on request (User Story 3, Scenario 4).
- Whether the seeded workspace already carries an ontology with declared relation types, or one
  must be configured for this suite specifically, is a Research-stage determination — required
  because `knowledge_reprocess_relation_types` fails without one (see Edge Cases).
- Deletion-tool assertions (`knowledge_delete_by_source`, `knowledge_delete_chunk_episode`) may
  target content this suite ingests itself via User Story 4's calls, rather than the fixture's
  originally-captured episodes, since the fixture's episodes may not carry the
  `source_file`/`chunk_id`-shaped metadata these tools key off of — see Edge Cases. The exact
  target selection is a Research/Plan-stage decision.
- The `knowledge-corrections.yaml` file used for User Story 2's corrections round trip is
  authored by this suite (written into the isolated workspace before the test runs), targeting
  real fixture entities/edges to exercise a genuine round trip rather than a no-op.
- CI gating mirrors #234's established pattern (excluded from the default PR gate, run on
  push-to-main / `workflow_dispatch`) — whether this means a new job in the existing
  `real-corpus-e2e.yml` workflow or a new workflow file is a Plan-stage decision.
- This issue is test-only: no production mutation-handler code changes are in scope, except as a
  genuine bug fix if this suite's real-scale coverage surfaces one (a Plan/Implement-stage
  determination, not assumed in advance).

## Out of Scope

- Live LLM or real-embedder calls of any kind — this suite runs entirely against loopback stubs
  (the #232 cassette, if it lands, is a future enhancement, not a dependency of this issue).
- Admin/lifecycle round-trips (`knowledge_dump_wal`, `knowledge_prepare_checkpoint`,
  `knowledge_recover`/`knowledge_recover_full`, and related admin-scope tools beyond the
  post-mutation `knowledge_rebuild_from_wal` check required by FR-015) — tracked as a further
  follow-on issue (3 of 3 in the planned MCP e2e series).
- Read-path assertions — covered by #234.
- Regenerating or modifying the committed `real_corpus_wal` fixture itself (owned by #217).
- Attached mode (`--connect` to an existing Unix-socket server) — like #234, this suite only
  covers standalone `--mcp-stdio` spawning.
- Extraction quality / comparing extraction models against the corpus (#228) — this suite's stub
  extractor is deterministic-but-arbitrary, not a quality benchmark.

## Source References

- `crates/service/tests/mcp_real_corpus_e2e.rs` and `crates/service/tests/common/real_corpus.rs`
  (#234) — the MCP-over-stdio read-path suite and reusable seeding harness this issue builds on.
- `crates/core/tests/real_corpus_e2e.rs` and `crates/core/tests/fixtures/real_corpus_wal/` (#217)
  — the golden fixture and its rebuild→assert precedent (enumeration correctness, set-membership
  tolerance, zero-LLM-call discipline).
- `crates/service/src/mcp/tools.rs` — the MCP tool registry, scope buckets, and the input schemas
  for every mutation tool named in this spec.
- `crates/core/src/handlers.rs` — the mutation handlers themselves (`handle_delete_by_source`,
  `handle_merge_entities`, etc.).
- `README.md`'s relation-typing section — the documented `canonicalize_relations` /
  `reprocess_relation_types` semantics (idempotency caveats, abstention contract,
  arrow-named-noise-edge handling) this suite asserts against.
- `docs/adr/0037-relation-classification-abstention-writes-unclassified.md` (ADR-0037) — the
  abstention/ontology-required contract referenced in FR-011 and the Edge Cases.
- Issues #202 (dropped/misresolved edges during ingest — the User Story 4 regression guard), #203
  (index loss under sustained write), #205 (`backfill_relation_types` relation-type pollution —
  the motivating defect for User Story 3's genuine-classification assertions).
