# Feature Specification: Golden Real-Corpus WAL Fixture + Rebuild→Assert E2E Harness

**Feature Branch**: `fabrik/issue-217`
**Created**: 2026-07-24
**Status**: Draft
**Input**: User description: "The test harness exercises the real lbug DB (schema, HNSW/FTS indices, catalog, WAL), the full IPC/MCP dispatch, and — in the service tests — the real compiled binary as a subprocess. But the extractor is MockExtractor (a fixed Alice + Acme Corp + one WORKS_AT edge, input-independent; classify_entities is a no-op) and the embedder is mocked. So no test uses real LLM output or a realistic graph. That leaves a gap for everything whose behavior depends on real-corpus shape: edge-resolution against recurring hub entities (#209 / report #202), index health at scale past the 1000-entity hybrid-dedup threshold (#208 / report #203), relation typing over real fact sentences (#210 / report #204), and search/traversal quality. These are validated today only on synthetic/canned data — which is exactly where the field-reported bugs (#207) lived. The WAL is the source of truth, and knowledge_rebuild_from_wal reconstructs the entire graph deterministically with zero LLM calls (extraction already happened when the WAL was written). So a captured real WAL can serve as a golden fixture for real-data e2e tests — free per run."

## Background

Today's test suite is deep on *infrastructure* realism (real lbug DB, real schema, real HNSW/FTS indices, real IPC/MCP dispatch, and — for service tests — the real compiled binary as a subprocess) but shallow on *content* realism: every graph in the test suite is produced by `MockExtractor`, a fixed, input-independent stub that always emits one `Alice` entity, one `Acme Corp` entity, and one `WORKS_AT` edge (`classify_entities` is a no-op), paired with a mocked embedder.

That means nothing in CI today exercises behavior that only shows up on real-corpus shape:

- **Cross-batch edge resolution against recurring "hub" entities** (#209, originally reported as #202) — synthetic tests never have the same entity mentioned across dozens of episodes.
- **Index health at scale past the 1,000-entity hybrid-dedup threshold** (#208, originally reported as #203) — the `LIMINIS_DEDUP_HYBRID_THRESHOLD` code path (default 1,000, `crates/core/src/episode.rs`) is architecturally untested; no fixture in the repo comes close to that entity count.
- **Relation typing over real prose `fact` sentences** (#210, originally reported as #204) — `MockExtractor` never produces varied relation types to classify.
- **Search and traversal quality** against a graph with realistic density and ambiguity.

All four of these are exactly the shape of bug that has previously reached the field (#207) without being caught by CI, because CI's data never looked like real data.

Separately, `knowledge_rebuild_from_wal` already reconstructs an entire graph deterministically from a WAL with **zero LLM calls** — extraction happened once, when the WAL was originally written, and everything after that (parsing, `MERGE`/`SET`, index build) is mechanical replay. That means a WAL captured from one real ingest run can be committed as a fixture and replayed in every CI run afterward for free — no API keys, no network calls, no per-run cost or flakiness from a live model.

This issue extends the existing fixture pattern already established in `crates/core/tests/fixtures/wal/` (currently small, hand-crafted `.jsonl` files exercising WAL *format* edge cases, driven by `WalReplayer` in `crates/core/tests/wal_replay.rs`) and `crates/core/tests/fixtures/golden_queries.json` (currently an empty golden-query scaffold for the IPC parity test) up to a real corpus, large enough to cross the 1,000-entity threshold and dense enough to exercise recurring entities and varied relation types.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Real-data regression coverage in CI (Priority: P1)

As a maintainer, when I change dedup, index-build, relation-typing, search, or traversal code, I want a CI test that rebuilds a real-shaped graph (not the `MockExtractor` stub) and asserts on its shape and query behavior, so that regressions which only manifest on realistic data are caught before merge instead of being reported later as field bugs.

**Why this priority**: This is the entire point of the issue — without it, the gap described in the Background persists and #202/#203/#204-class bugs keep escaping to the field. It's also the only story required to close the acceptance criteria.

**Independent Test**: Run `cargo test --release` in CI (or locally) with no LLM API key configured and no network access; the new e2e test still passes because it never calls an extractor or embedder — it only replays a committed WAL.

**Acceptance Scenarios**:

1. **Given** the committed golden WAL fixture and a fresh, empty lbug DB, **When** the e2e test runs `knowledge_rebuild_from_wal` (or the equivalent `WalReplayer` path) against it, **Then** replay completes with zero LLM/extractor/embedder calls and the test asserts the resulting entity count, edge count, and episode count match the fixture's recorded expected values.
2. **Given** the rebuilt graph, **When** the test checks index state, **Then** it asserts `indices_built: true` and that the entity count exceeds the hybrid-dedup threshold (default 1,000, `LIMINIS_DEDUP_HYBRID_THRESHOLD`), genuinely exercising the hybrid-dedup/index code path rather than staying below it.
3. **Given** the rebuilt graph, **When** the test runs a representative set of golden search queries (entity search, relationship search) against known hub entities, **Then** results match expected entities/relationships recorded alongside the fixture.
4. **Given** the rebuilt graph, **When** the test runs at least one representative multi-hop traversal query, **Then** the result matches the recorded expected path/entities.
5. **Given** the rebuilt graph, **When** the test samples relation edges produced from real `fact` sentences, **Then** their relation types match the expected types recorded alongside the fixture.
6. **Given** the same committed fixture, **When** the e2e test is run twice in separate processes, **Then** both runs produce identical counts and query results (deterministic replay, no reliance on wall-clock, randomness, or run order).

---

### User Story 2 - Fixture is regenerable when the schema or WAL format changes (Priority: P2)

As a maintainer, when a future schema or WAL-format change makes the committed fixture stale (replay fails, or replay succeeds but produces different-than-recorded counts), I want documented, repeatable steps to regenerate the fixture from its pinned source corpus, so I'm not stuck reverse-engineering a one-off capture that only the original author remembers how to redo.

**Why this priority**: Without this, the fixture becomes a liability the first time the schema changes (per the existing schema-parity discipline in this repo) — exactly the kind of "worked once, no one can reproduce it" trap the project has been burned by before (per `crates/core/tests/fixtures/README.md`'s existing capture-procedure precedent for `ipc_corpus/`).

**Independent Test**: Following only the documented regeneration procedure (no tribal knowledge), a maintainer can reproduce a fixture from the same pinned corpus snapshot and get a WAL that replays to the same entity/edge/episode counts as the previous fixture (modulo any intentional schema change).

**Acceptance Scenarios**:

1. **Given** the documented regeneration procedure, **When** a maintainer follows it against the pinned corpus snapshot (same dump date / source commit), **Then** it reproduces a WAL fixture whose replay counts match the previously committed fixture.
2. **Given** a schema or WAL-format change that breaks compatibility with the committed fixture, **When** a maintainer re-runs the regeneration procedure, **Then** the documentation covers how to identify that the fixture is stale (e.g., replay fails, or `indices_built` assertions fail) and how to produce a new one.

---

### Edge Cases

- **Fixture size vs. repo hygiene**: a real ingest of 200–500 documents producing >1,000 entities will yield a WAL far larger than the existing hand-crafted fixtures (a few KB each). The committed fixture (and its `corpus_prose.jsonl` sibling, FR-013) MUST be size-managed (compressed and/or trimmed) so it doesn't meaningfully bloat clone/checkout size; document the actual approach taken (e.g., gzip, or trimming to the smallest corpus subset that still crosses the 1,000-entity threshold with margin).
- **Non-determinism in downstream ops**: index build order, HNSW graph construction, or dedup tie-breaking might not be 100% deterministic even on identical input. Assertions must be written to tolerate this (e.g., assert on counts and set-membership rather than exact ordering, where the underlying operation isn't guaranteed to be order-stable) — or the harness must confirm determinism explicitly and assert exact equality if so.
- **Corpus licensing drift**: if the pinned corpus snapshot is later taken down or re-licensed at the source, the *committed fixture* is unaffected (it's already captured and license-documented at capture time) — regeneration in the future may require selecting a new snapshot or corpus. Document the license and pinned source alongside the fixture so this is auditable later.
- **Threshold right at the boundary**: the graph must cross the 1,000-entity hybrid-dedup threshold with enough margin that ordinary dedup collapsing (expected on a real corpus with hub entities) doesn't accidentally drop the final count back under 1,000.
- **PII/secrets**: because the corpus is public documentation/encyclopedic content rather than user data, this risk is structurally avoided by the corpus-selection criteria rather than needing a scrubbing step — but the regeneration docs should note this constraint applies to any future re-capture too.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A golden WAL fixture MUST be captured from one real ingest run — real `Extractor` (not `MockExtractor`) and real `Embedder` (not `MockEmbedder`) — over a corpus meeting all of the following: (a) redistributable under a license compatible with this public MIT repo (public domain, CC-BY, CC-BY-SA, Apache-2.0, or MIT — no NC/private/scraped content); (b) large enough that the resulting graph exceeds 1,000 entities with margin; (c) contains dense, recurring "hub" entities cross-referenced across multiple source documents; (d) is knowledge/documentation-style prose with named entities in typed relationships (not narrative fiction); (e) is a pinned, versioned snapshot (dump date or source git SHA) so it can be regenerated later.
- **FR-002**: The captured WAL fixture MUST be committed under `crates/core/tests/fixtures/` (as a new fixture, distinct from the existing small hand-crafted `wal/*.jsonl` format fixtures), size-managed (compressed and/or trimmed if the raw capture is large), and contain no PII or secrets.
- **FR-003**: The fixture's source corpus, license, and pinned snapshot identifier (dump date or commit SHA) MUST be documented alongside the fixture (e.g., in a fixture-local README), consistent with the existing documentation precedent in `crates/core/tests/fixtures/README.md`.
- **FR-004**: A new e2e test MUST rebuild the fixture WAL into a fresh lbug DB using the existing WAL-replay path (`knowledge_rebuild_from_wal` / `WalReplayer`), performing **zero** LLM, extractor, or embedder calls during the test run.
- **FR-005**: The e2e test MUST assert post-rebuild entity count, edge count, and episode count against expected values recorded at capture time.
- **FR-006**: The e2e test MUST assert `indices_built: true` after rebuild, and that the entity count exceeds the hybrid-dedup threshold (`LIMINIS_DEDUP_HYBRID_THRESHOLD`, default 1,000), so the hybrid-dedup/index-build path is genuinely exercised rather than incidentally skipped.
- **FR-007**: The e2e test MUST include representative "golden query" assertions (entity search, relationship search) extending the existing `golden_queries.json` pattern, covering at least the known hub entities in the fixture corpus.
- **FR-008**: The e2e test MUST include at least one representative multi-hop traversal assertion.
- **FR-009**: The e2e test MUST include relation-typing assertions sampling edges derived from real `fact` sentences in the corpus, checking their relation type/name against expected values recorded at capture time.
- **FR-010**: The e2e test MUST be deterministic — repeated runs against the same committed fixture produce identical asserted results.
- **FR-011**: The e2e test MUST run as part of the standard `cargo test --release` CI gate (same convention as the existing `wal_replay.rs` / `ipc_parity.rs` tests), without requiring any external network access, API keys, or live LLM/embedding service.
- **FR-012**: The fixture regeneration procedure MUST be documented (script and/or step-by-step instructions) so a future maintainer can reproduce a fixture from the pinned corpus snapshot without needing the original author's context, following the existing capture-procedure precedent in `crates/core/tests/fixtures/README.md`.
- **FR-013**: The captured fixture MUST additionally include `corpus_prose.jsonl`: the cleaned prose text actually fed to the extractor for every consumed article, one record per article with `title` and `revision_id` provenance, behind a header recording the wikitext-cleanup version used to produce it. This is the input artifact required to compare a different extraction model against the same real corpus (a need identified by #228 and the open question on #212) — neither the WAL (post-extraction) nor a future LLM-response cassette (records one model's exchange only) can serve that purpose, and refetching from the pinned Wikipedia revisions at compare-time is not equivalent, because the prose is a derived artifact of the wikitext-cleanup code and is not guaranteed to reproduce byte-identically after a future cleanup fix.
- **FR-014**: `corpus_prose.jsonl` MUST be documented (in the fixture-local README, FR-003) as the dedicated input fixture for extraction-model comparison (#228), explicitly distinct from the committed WAL's rebuild-only role.
- **FR-015**: The regeneration procedure (FR-012) MUST cover regenerating `corpus_prose.jsonl` independently of a full re-capture — re-fetching the pinned revisions already recorded for the consumed articles and re-running the same cleanup function — and this path MUST be verified to require zero LLM or embedding calls.

### Key Entities

- **Golden WAL fixture**: The committed, replayable `.jsonl` WAL capture (possibly compressed) produced by one real ingest run over the selected public corpus; the artifact under test.
- **Corpus snapshot**: The pinned, licensed, versioned source document set (e.g., a dated Wikipedia domain-cluster export, or a pinned Rust RFCs / Kubernetes KEPs commit) used to produce the fixture; not itself committed to the repo, only referenced/documented.
- **Expected results record**: The recorded entity/edge/episode counts, golden queries and their expected results, and expected relation-type samples captured alongside the fixture, against which the e2e test asserts.
- **Rebuild→assert e2e test**: The new test (or test module) that replays the fixture with zero LLM calls and performs all assertions in this spec.
- **Corpus prose fixture**: `corpus_prose.jsonl`, the cleaned prose text (with `title`/`revision_id` provenance and a cleanup-version header) fed to the extractor for every consumed article at capture time; enables comparing a different extraction model against the same real corpus (#228) without re-fetching or re-cleaning it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The e2e test rebuilds the golden fixture and passes with **zero** LLM/extractor/embedder invocations — verifiable by running it with no LLM API key configured and no network access.
- **SC-002**: The rebuilt graph's entity count exceeds the hybrid-dedup threshold (1,000 by default) with enough margin that ordinary dedup collapsing on a real corpus doesn't push it back under the threshold.
- **SC-003**: The full existing pre-commit/CI gate (`cargo fmt --all`, `cargo clippy --release -- -D warnings` / `cargo clippy --all-targets -- -D warnings`, `cargo test --release`) remains green with the new fixture and test added.
- **SC-004**: A maintainer unfamiliar with the original capture, following only the committed regeneration documentation, can reproduce a fixture from the same pinned corpus snapshot that replays to matching entity/edge/episode counts.
- **SC-005**: The new test's contribution to overall CI test-job runtime is small relative to the existing job (replay-only, no network I/O) — the implementer should measure and record the actual added runtime in the regeneration/fixture documentation so future maintainers know what to expect.

## Assumptions

- The **primary corpus choice** is a pinned Wikipedia (or Simple English Wikipedia, for cleaner extraction and smaller size) domain-cluster subset of roughly 200–500 articles on one coherent domain (e.g., "programming languages," "the Apollo program"), licensed CC-BY-SA. The **on-brand alternative** (Rust RFCs or Kubernetes KEPs, Apache-2.0/MIT/CC-BY) is acceptable if it better satisfies the density/relation-typing criteria in FR-001. The exact corpus and article/document list is a Research/Plan-stage decision within the criteria fixed by FR-001 — this spec does not mandate one over the other.
- "Real extractor and embedder" means whatever this repo's production `Extractor`/`Embedder` trait implementations are (e.g., `AnthropicExtractor`, `OaiEmbedder`) rather than `MockExtractor`/`MockEmbedder`/`ConfigurableExtractor`-in-mock-mode — the fixture-capture run itself is a one-time, offline, outside-CI step that does incur real LLM/embedding API calls; only the committed *replay* in CI is LLM-call-free.
- The DocRED-based classification-accuracy eval and any LLM-response "cassette" work described in the issue's Risks/Dependencies section are explicitly out of scope here (see Out of Scope) and tracked as separate follow-on work.
- No numeric CI time budget is mandated by this spec (SC-005 asks the implementer to measure and document the actual figure) — if a hard ceiling is later needed, it can be set once real numbers exist.
- The fixture directory location and naming (e.g., a new `crates/core/tests/fixtures/real_corpus_wal/` alongside the existing `wal/`) is left to the Plan stage; this spec only requires it live under `crates/core/tests/fixtures/`.

## Out of Scope

- LLM-response "cassettes" for the extraction/classification steps (a separate follow-on issue, tracked as #232) — this fixture is post-extraction and cannot test the LLM→entities/edges step itself, nor LLM-calling ops like `reprocess_relation_types`.
- Any production-code change (this is a test-fixture-and-harness-only issue).
- Adding fixture-based assertions to #208 / #209 / #210 directly (they remain covered by their own synthetic tests; this fixture is available for them to build on afterward, not a hard blocker).
- A private/reporter-specific corpus as the committed fixture (only a public corpus is acceptable for the committed artifact; a private corpus could at most be used as an optional, uncommitted local validation extra, which is not part of this issue's deliverable).
- The DocRED-based relation-classification accuracy eval mentioned in the issue's Risks/Dependencies section.

## Source References

- `crates/core/tests/fixtures/wal/` — existing hand-crafted WAL format fixtures and `WalReplayer` test pattern (`crates/core/tests/wal_replay.rs`).
- `crates/core/tests/fixtures/golden_queries.json` and `crates/core/tests/fixtures/README.md` — existing golden-query and fixture-capture-procedure precedent (for the IPC parity corpus).
- `crates/core/src/episode.rs` — `LIMINIS_DEDUP_HYBRID_THRESHOLD` / `hybrid_threshold()` (default 1,000).
- `crates/core/src/extractor.rs` — `Extractor` trait, `MockExtractor`, `AnthropicExtractor`, `ConfigurableExtractor`.
- `crates/core/src/embedder.rs` — `Embedder` trait, `MockEmbedder`, `OaiEmbedder`.
- Issue #208 (index health / hybrid-dedup at scale), #209 (edge resolution / hub entities), #210 (relation typing), #202/#203/#204/#207 (originating field reports).
