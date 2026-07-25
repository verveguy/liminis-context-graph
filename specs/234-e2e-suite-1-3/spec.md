# Feature Specification: MCP Read-Path E2E Suite Over the Real-Corpus Fixture

**Feature Branch**: `fabrik/issue-234`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "The real-corpus e2e harness added in #217 (`crates/core/tests/real_corpus_e2e.rs`) rebuilds the golden WAL fixture and asserts against it — but it calls `handlers::dispatch(...)` directly, in-process, from `crates/core/tests/`. It cannot do otherwise: the MCP transport lives in `crates/service/src/mcp/`, and `lcg-core` has no dependency on `lcg-service`. So the layer external users actually touch is untested against real data. Meanwhile `crates/service/tests/mcp_stdio.rs` does drive the real binary over stdio, but against an empty graph with a stub embedder. The result is that we have MCP-without-data and real-data-without-MCP, and nothing joins them. Establish an MCP-level e2e suite in `crates/service/tests/` that seeds a workspace from the committed real-corpus WAL fixture, spawns the real binary over MCP-over-stdio, and asserts the read tool surface against known-good results on real content. This issue also builds the shared fixture-seeding harness that the write-path and admin-path suites will reuse."

## Background

Today's MCP-over-stdio integration tests (`crates/service/tests/mcp_stdio.rs`, #195) spawn the real compiled binary and drive it over the real JSON-RPC/stdio transport — real tool schemas, real argument marshalling, real scope gating — but always against a freshly-initialized, empty graph with a stub embedder. Nothing there ever calls a real tool against real content.

Separately, #217 built a golden real-corpus WAL fixture (1,506 entities / 2,392 relationships / 228 episodes, captured from a real `AnthropicExtractor` + real embedder run over an Apollo-program Wikipedia corpus) and an e2e harness (`crates/core/tests/real_corpus_e2e.rs`) that replays it deterministically with zero LLM/embedder calls and asserts entity counts, golden search queries, traversal, and relation typing. But that harness lives in `crates/core/tests/` and calls `handlers::dispatch(...)` directly, in-process — it has no choice, since the MCP transport (`crates/service/src/mcp/`) lives in a separate crate that `lcg-core` does not depend on. So it bypasses everything that makes the MCP layer its own source of bugs: `tools/list` schema generation, `tools/call` argument marshalling and required-argument validation, error-to-`CallToolResult` mapping, and `read`/`write`/`cypher`/`admin` scope gating.

The net result: MCP-without-data (`mcp_stdio.rs`) and real-data-without-MCP (`real_corpus_e2e.rs`), and nothing joins them. A rebuilt 1,506-entity graph answering correctly via direct `handlers::dispatch` tells us nothing about whether `knowledge_find_entities` returns sane results *through* `tools/call` against that same data — which is exactly the layer external MCP clients actually touch, and exactly what shipped in v0.10.0.

This issue is the first of three planned suites over the same fixture (this one: MCP read-path; a follow-on: MCP write/mutation path; a follow-on: MCP admin/lifecycle path) and is responsible for building the shared seeding harness all three will reuse, so it is scoped generously enough to serve as that shared foundation without over-building for the other two suites' specific needs.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - MCP read-path regression coverage on real data (Priority: P1)

As a maintainer, when I change the MCP tool registry, argument marshalling, scope gating, or any read-path handler, I want a CI suite that spawns the real binary over MCP-over-stdio against a real, rebuilt 1,506-entity graph and asserts that `tools/call` returns the *right* results — not just a 200 — so that regressions in the layer external MCP clients actually use are caught before merge, instead of only being caught (or missed) by tests that bypass MCP entirely or that use an empty graph.

**Why this priority**: This is the entire point of the issue — without it, the gap described in the Background persists: MCP transport correctness is tested only against synthetic empty-graph data, and real-corpus data correctness is tested only by bypassing MCP. Closing that gap is the only story required to satisfy the acceptance criteria.

**Independent Test**: Run the new suite (via its dedicated CI workflow or `cargo test -p lcg-service --release -- --ignored`) with no LLM API key configured and no network access other than the test's own stub embedder loopback; it still passes because it only replays a committed WAL and never makes a real outbound LLM or embedder call.

**Acceptance Scenarios**:

1. **Given** a temp workspace seeded from the committed real-corpus WAL fixture and rebuilt, **When** the real binary is spawned with `--mcp-stdio` against that workspace and `knowledge_status` is called through `tools/call`, **Then** the response's `entity_count`, `relationship_count`, and `episode_count` match the fixture's recorded expected values (1,506 / 2,392 / 228) and `indices_built` is `true`.
2. **Given** the seeded server, **When** a golden `knowledge_find_entities` query from the fixture's recorded golden queries is issued through `tools/call`, **Then** the expected entities appear in the result (set-membership/overlap, not exact top-1/ordering — consistent with #217's tolerance for query-time embedding not being the real embedder).
3. **Given** the seeded server, **When** a golden `knowledge_find_relationships` and `knowledge_search_passages` query is issued, **Then** the results show the expected relation-type/content overlap recorded in the fixture.
4. **Given** the seeded server, **When** `knowledge_get_entity_neighbors` is called on the fixture's recorded hub entity, **Then** the returned neighbor set exactly matches the fixture's recorded traversal expectation (graph traversal doesn't depend on embeddings, so this is exact-set equality, not tolerant matching).
5. **Given** the seeded server, **When** `knowledge_list_entities` and `knowledge_list_relationships` are each called with `num_results` set comfortably above the fixture's total counts, **Then** each call's full result set has exactly the fixture's expected count with no duplicate UUIDs and no missing UUIDs (this is the enumeration-correctness technique already established by #217's `real_corpus_e2e.rs`, applied here through `tools/call` instead of direct dispatch).
6. **Given** the seeded server, **When** `knowledge_get_episodes`, `knowledge_get_entities_by_source`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, and `knowledge_get_edges_by_uuids` are each called with real fixture identifiers (a real source description, a real group ID, real entity/edge UUIDs), **Then** each returns the expected real content, not merely a non-error response.
7. **Given** the seeded server started with `--scope=read`, **When** `tools/list` is called, **Then** write/admin/cypher tools are absent from the list; **When** a write/admin/cypher tool is called anyway, **Then** the call is rejected as an unlisted tool (protocol-level error, consistent with `mcp_stdio.rs`'s existing scope-rejection behavior), while read tools continue to work against the real data.
8. **Given** the seeded server, **When** a `tools/call` for a tool with a required argument omits that argument, **Then** the response is a clean tool-level error (`isError: true`) that never reaches the underlying handler — consistent with `mcp_stdio.rs`'s existing argument-validation tests, now exercised against a real-data-backed server instead of an empty one.
9. **Given** the whole suite runs, **When** it completes, **Then** the extractor and any real embedder are never invoked — a call-counting or stub-embedder check makes this an explicit, asserted invariant, not an assumption.

---

### User Story 2 - Reusable seeding harness for the write-path and admin-path follow-on suites (Priority: P2)

As a maintainer implementing the two follow-on issues (MCP write/mutation-path and MCP admin/lifecycle-path suites over the same fixture), I want the fixture-seeding logic built in this issue to live in `crates/service/tests/common/` as a documented, reusable helper, so I don't have to re-derive "populate a temp workspace from the WAL fixture and rebuild it with zero LLM/embedder calls" from scratch in each follow-on suite.

**Why this priority**: Named directly in the parent issue as a deliverable of this issue ("This issue also builds the shared fixture-seeding harness that the write-path and admin-path suites will reuse"). It doesn't gate User Story 1's own read-path assertions, but its absence would force the two follow-on issues to duplicate or reverse-engineer this issue's seeding logic.

**Independent Test**: A follow-on test file in `crates/service/tests/` can call the seeding helper with no changes to it and get back a ready-to-spawn seeded workspace, without needing to read or copy `real_corpus_e2e.rs`'s seeding logic directly.

**Acceptance Scenarios**:

1. **Given** the seeding helper added to `crates/service/tests/common/`, **When** a test file other than this issue's own read-path suite calls it, **Then** it produces a temp workspace whose `.lcg/` directory has been populated from the fixture and rebuilt, ready to back a `--mcp-stdio` subprocess, with zero LLM/embedder calls during seeding.
2. **Given** the seeding helper's public API, **When** a future maintainer reads it, **Then** its inputs, outputs, and "zero LLM/embedder calls" guarantee are documented (doc comments and/or the fixture's own README) clearly enough to reuse without needing to read this issue's read-path assertions first.

---

### Edge Cases

- **Seeding cost amortization**: seeding the fixture's ~71 MB WAL and rebuilding indices takes on the order of a minute (per #217's measured figures). The suite MUST NOT re-seed once per test function — it must seed at most once per suite run (e.g., one shared seeded workspace/spawned session reused across all read-path assertions), or CI time balloons linearly with the number of assertions.
- **No true cursor/offset pagination exists today**: `knowledge_list_entities` / `knowledge_list_relationships` currently expose only a `num_results` limit (deterministically ordered by UUID) with no offset or cursor parameter — there is no way to fetch "page 2" of a result set through the current tool surface. "Pagination correctness at scale" in this issue is therefore validated via full-enumeration-in-one-call (request comfortably above the total count, assert exact count and UUID-set match with no duplicates), the same technique #217 already established for `knowledge_list_relationships` — not by adding a new pagination parameter to production code (see Out of Scope).
- **Query-time embedding is a stub, not a real vector**: the fixture's *stored* embeddings are real (captured at ingest time), but query-time embedding for this suite's `--embedder-http` stub is not — golden query assertions must therefore tolerate re-ranking drift the same way #217's in-process harness does (set-membership/overlap, not exact top-1/ordering), except where the operation (traversal, raw listing) doesn't touch embeddings at all, where exact equality is expected.
- **Scope-rejection response shape**: rejecting an unlisted (out-of-scope) tool call is a protocol-level JSON-RPC error, whereas rejecting a listed tool call with a missing required argument is a tool-level `isError: true` result — the suite's assertions must distinguish these two cases correctly (per `mcp_stdio.rs`'s existing precedent) rather than treating all failures the same way.
- **Zero-network verification**: "zero outbound LLM/embedder calls" must be independently checkable (no API key, no network access beyond the test's own loopback stub embedder), not merely true by construction because the wired components happen to be mocks — mirroring #217's explicit call-counting discipline.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A seeding helper MUST be added under `crates/service/tests/common/` that, given a temp workspace directory, populates its `.lcg/` from `crates/core/tests/fixtures/real_corpus_wal/wal/` and performs the rebuild, with zero LLM and zero embedder calls.
- **FR-002**: The seeding helper MUST be written and documented generically enough to be reused, unmodified, by the follow-on write-path and admin-path MCP suites (separate issues) — not hardcoded to this issue's specific read-path assertions.
- **FR-003**: The suite MUST spawn the real compiled binary with `--mcp-stdio` against a seeded workspace, following the existing spawn/drive pattern in `crates/service/tests/mcp_stdio.rs` and `crates/service/tests/common/mod.rs` (stub HTTP embedder via `--embedder-http`; the fixture's stored embeddings already carry real vectors, so no live embedder is required).
- **FR-004**: The suite MUST assert, via `tools/call knowledge_status`, that `entity_count`, `relationship_count`, and `episode_count` match the fixture's recorded expected values (sourced from `expected_results.json`, not hardcoded) and that `indices_built` is `true`.
- **FR-005**: The suite MUST assert, via `tools/call knowledge_find_entities` and `tools/call knowledge_find_relationships`, that the fixture's recorded golden queries return the expected entities / relation-type overlap, using the same set-membership tolerance #217 established (not exact top-1/ordering).
- **FR-006**: The suite MUST include at least one `tools/call knowledge_search_passages` golden assertion against real fixture content.
- **FR-007**: The suite MUST assert, via `tools/call knowledge_get_entity_neighbors` on the fixture's recorded hub entity, that the returned neighbor set exactly matches the fixture's recorded traversal expectation.
- **FR-008**: The suite MUST assert full-enumeration correctness for `tools/call knowledge_list_entities` and `tools/call knowledge_list_relationships`: requesting `num_results` comfortably above the fixture's total counts in a single call MUST yield exactly the expected count with no duplicate and no missing UUIDs.
- **FR-009**: The suite MUST include at least one real-content assertion each for `tools/call knowledge_get_episodes`, `knowledge_get_entities_by_source`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, and `knowledge_get_edges_by_uuids`.
- **FR-010**: The suite MUST exercise the >1,000-entity hybrid-dedup/search code path — satisfied by construction, since the fixture's 1,506 entities exceed the default `LIMINIS_DEDUP_HYBRID_THRESHOLD` (1,000) with margin, the same threshold-crossing behind the field-reported failure in #203.
- **FR-011**: The suite MUST assert, against the seeded real-data server started with `--scope=read`, that `tools/list` excludes write/admin/cypher tools and that calling one of them anyway is rejected, while read tools continue to succeed.
- **FR-012**: The suite MUST assert that a `tools/call` for a tool with a required argument, omitting that argument, against the seeded real-data server returns a clean tool-level error (`isError: true`) and never reaches the underlying handler.
- **FR-013**: The suite MUST seed the fixture workspace at most once across all of its assertions (e.g., one shared seeded workspace/session reused across test functions), not once per test function, given the fixture's measured seeding/rebuild cost (~71 MB WAL, up to roughly a minute, per #217).
- **FR-014**: The suite MUST make zero outbound LLM or real-embedder network calls end to end, verifiable independently (no API key, no network access beyond the suite's own loopback stub embedder).
- **FR-015**: The suite MUST be gated the same way as #217's `real_corpus_e2e.rs` — excluded from the default per-PR `cargo test --release` gate, with an automatic trigger elsewhere (e.g., on push to `main`, mirroring `.github/workflows/real-corpus-e2e.yml`'s existing precedent) so it doesn't rot unrun.
- **FR-016**: `cargo fmt --all` and the project's clippy gate (`cargo clippy --all-targets -- -D warnings` / `cargo clippy --release -- -D warnings`) MUST remain green with the new suite added.

### Key Entities

- **Seeded MCP workspace**: A temp workspace whose `.lcg/` directory has been populated from the committed `real_corpus_wal` fixture and rebuilt, ready to back a `--mcp-stdio` subprocess.
- **Seeding helper**: The reusable test helper function(s) added to `crates/service/tests/common/` that produce a Seeded MCP workspace; the artifact this issue is responsible for building on behalf of the two follow-on suites.
- **MCP read-path assertion**: A golden, per-tool assertion sourced from the fixture's `expected_results.json` and checked through `tools/call`, rather than through direct in-process dispatch.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test seeds a workspace from the fixture, spawns the real binary with `--mcp-stdio`, and completes with zero outbound LLM/embedder calls — verifiable by running with no API key and no network access beyond the suite's own loopback stub embedder.
- **SC-002**: Golden read assertions pass against real corpus content for every tool listed in FR-005/FR-006/FR-007/FR-009, including rank-order/overlap expectations where meaningful and exact-match expectations where the operation doesn't depend on embeddings.
- **SC-003**: Full-enumeration assertions for `knowledge_list_entities` / `knowledge_list_relationships` recover exactly the fixture's 1,506 / 2,392 counts, with no duplicate or missing UUIDs.
- **SC-004**: `--scope=read` hides and rejects non-read tools, and a missing required argument yields a clean tool-level error, both verified against the seeded real-data server (not only against an empty graph, as `mcp_stdio.rs` does today).
- **SC-005**: The seeding helper is reusable, unmodified, by the follow-on write-path and admin-path MCP suites — verifiable in this issue by the helper's documented, generically-scoped API (actual reuse happens in the follow-on issues).
- **SC-006**: `cargo fmt --all` and the project's clippy gate remain green with the new suite added; the suite runs within its own gated CI workflow, not the per-PR critical path.

## Assumptions

- The suite lives in `crates/service/tests/` (a new test file, e.g. alongside `mcp_stdio.rs`) since that is where the MCP transport and its existing integration tests already live; the exact file name and internal structure are a Plan-stage decision.
- "Pagination correctness at scale" (as named in the originating issue) is satisfied by the full-enumeration-in-one-call technique already established in #217's `real_corpus_e2e.rs` for `knowledge_list_relationships`, not by adding a new offset/cursor parameter to `knowledge_list_entities` / `knowledge_list_relationships` — the current handlers expose only a `num_results` limit, and adding real cursor-based pagination would be a production-code change outside this test-only issue's scope.
- CI gating reuses the pattern and gating decision already established by #217 (excluded from the default PR gate, run on push-to-main / `workflow_dispatch`); whether this means a new job in the existing `.github/workflows/real-corpus-e2e.yml` or a new workflow file is a Plan-stage decision.
- The exact mechanics of "seed once per suite" (a lazy-static/shared fixture, a `OnceLock`-guarded setup, or a single shared spawned session reused across `#[test]` functions) are left to the Plan/Research stage; this spec only requires that the expensive seed+rebuild step run at most once for the whole suite.
- Golden query/traversal/relation-type expectations are read from the fixture's existing `expected_results.json` (already committed by #217), not re-derived or hardcoded independently in this new suite.
- The stub HTTP embedder pattern already used by `mcp_stdio.rs` (`spawn_stub_embedder` in `crates/service/tests/common/mod.rs`) is reused as-is for any query-time embedding needed by this suite's `--mcp-stdio` subprocess.

## Out of Scope

- Mutations through MCP (`tools/call` for write-scope tools against real data) — tracked as a follow-on issue.
- Admin/lifecycle round-trips through MCP (WAL dump/checkpoint/rebuild/recover via `tools/call`) — tracked as a follow-on issue.
- Attached mode (`--connect` to an existing Unix-socket server) — this suite only covers standalone `--mcp-stdio` spawning.
- Extraction quality / comparing extraction models against the corpus (#228).
- Adding a true offset/cursor pagination parameter to `knowledge_list_entities` / `knowledge_list_relationships` production code — see Assumptions.
- Regenerating or modifying the committed `real_corpus_wal` fixture itself (owned by #217).

## Source References

- `crates/core/tests/real_corpus_e2e.rs` and `crates/core/tests/fixtures/real_corpus_wal/` — the golden fixture and its rebuild→assert harness (#217), source of the enumeration-correctness and set-membership-tolerance precedents this issue reuses.
- `crates/service/tests/mcp_stdio.rs` and `crates/service/tests/common/mod.rs` — the existing MCP-over-stdio spawn/drive pattern, scope tests, and argument-validation tests (#195) this issue extends to real data.
- `crates/service/src/mcp/tools.rs` — the MCP tool registry and scope buckets (`read`/`write`/`admin`/`cypher`).
- `.github/workflows/real-corpus-e2e.yml` — the CI-gating precedent (`#[ignore]`d from the per-PR gate, run on push-to-main / `workflow_dispatch`) this issue's suite follows.
- Issue #203 (field-reported hybrid-dedup-at-scale failure, the >1,000-entity threshold this fixture deliberately crosses), #228 (extraction-model comparison, out of scope).
