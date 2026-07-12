# Feature Specification: Rust Knowledge Graph Service

**Feature Branch**: `001-rust-knowledge-graph`
**Created**: 2026-05-18
**Status**: Draft
**Input**: User description: "Replace the upstream Python graphiti-core service with a thin Rust service over LadybugDB's native bindings, preserving the surface that liminis-framework already uses, and packaged as a standalone library/service reusable by other projects."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Drop-in service replacement for ingest + search (Priority: P1)

A Liminis workspace currently spawns the upstream Python graphiti-core service over a Unix socket to ingest episodes and answer search queries. The Rust service must implement the same IPC contract so the existing in-process MCP servers (knowledge-reader, knowledge-writer, semantic-search) and the chat agent's retrieval calls keep working without any client-side change.

**Why this priority**: Without parity on the existing IPC, nothing in the desktop app keeps working. This is the gate that lets the rewrite be staged behind a feature flag.

**Independent Test**: Run the existing app against the Rust service binary. Onboarding flow ingests a corpus; knowledge-reader and semantic-search return non-empty, ranked results for known queries; retrieve_episodes lists by source.

**Acceptance Scenarios**:

1. **Given** an empty workspace, **When** the app onboards a sample notebook of ~500 episodes, **Then** Entity and Episodic nodes plus RELATES_TO and MENTIONS edges are persisted and queryable, with identical group_id scoping to the Python service.
2. **Given** a populated workspace, **When** the chat agent calls hybrid node search (RRF) for an entity name, **Then** the Rust service returns results ordered consistent with the Python service to within rank-correlation ≥ 0.9 on a fixed 50-query golden set.
3. **Given** a workspace populated by the Python service, **When** the Rust service is started against that same `.lcg/db`, **Then** all reads (search, get_by_group_ids, raw Cypher) succeed without any schema migration.

---

### User Story 2 - WAL parity for git-friendly persistence (Priority: P1)

The current service writes a JSONL WAL under `.lcg/wal/` that is checked into the workspace's git repo. The Rust service must produce a byte-compatible WAL stream so existing workspaces' commits stay diff-readable, and must replay an existing WAL into a fresh DB on cold boot.

**Why this priority**: WAL files are user data already on disk in every shipped workspace. Breaking the format breaks every existing workspace.

**Independent Test**: Take a workspace with a non-trivial WAL, delete `.lcg/db`, boot the Rust service; verify the resulting DB matches the Python-rebuilt DB by node/edge counts and a deterministic hash of (sorted) UUIDs.

**Acceptance Scenarios**:

1. **Given** a WAL produced by the Python service, **When** the Rust service replays it, **Then** node, edge, and embedding counts match (zero tolerance for nodes/edges; exact embedding presence).
2. **Given** a write transaction in the Rust service, **When** it completes, **Then** the WAL line(s) appended are valid JSONL that the Python `replay_wal_ladybug` reader also accepts (forward-compat during rollout).

---

### User Story 3 - Concurrent reader/writer with per-role LLM routing (Priority: P2)

Extraction (Sonnet, ~30s/episode) must not block search reads. Dedup uses a small local model. The service must keep the reader/writer split documented in ADR-042 and let extraction, dedup, and embedding be configured to different providers, with fallback on primary failure.

**Why this priority**: Loses major UX (search latency) and ops levers if collapsed. The engineering cost was already paid in the Python service; must not regress.

**Independent Test**: Launch a long-running extract job; concurrently fire search queries; measure p95 search latency stays under target. Force the primary extraction LLM to error; verify fallback runs and the request still succeeds.

**Acceptance Scenarios**:

1. **Given** a backlog of 100 episodes extracting, **When** search queries fire concurrently, **Then** search p95 ≤ 500 ms.
2. **Given** a misconfigured Anthropic key, **When** extraction runs, **Then** the local fallback model produces results and the failure is logged once per session, not per call.

---

### User Story 4 - HNSW + BM25 hybrid dedup at scale (Priority: P2)

Entity dedup currently uses brute-force cosine over all entities in a group; this was a deliberate fork choice after FalkorDB's HNSW broke. With LadybugDB native HNSW now landed (PR #16, 0.6.92), the Rust service should use HNSW + BM25 candidate filtering, falling back to brute-force only when index population is below a threshold.

**Why this priority**: Workspaces are reaching sizes where O(N) cosine matters; doing the rewrite is the right moment to take advantage of native indices.

**Independent Test**: On a 50k-entity workspace, dedup-step time is ≤ 30% of the Python brute-force baseline, with dedup-decision overlap ≥ 95% on a labeled set.

---

### User Story 5 - Reusable as a library by non-Liminis projects (Priority: P2)

The service exists as its own repo so other projects can adopt it. The crate must build standalone, have a usable Rust API for in-process embedding, and a binary entrypoint for the Unix-socket IPC service. A consumer who knows nothing about Liminis should be able to read the README and `examples/` and ingest+search against a local LadybugDB file within an hour.

**Why this priority**: The "make it a separate repo" decision is load-bearing on the assumption that this is reusable. If it ossifies into a Liminis-only artifact the repo split was wasted.

**Independent Test**: A toy consumer project in `examples/` (not depending on any Liminis code) ingests text, runs search, and exits successfully under CI.

**Acceptance Scenarios**:

1. **Given** a fresh checkout, **When** `cargo build --release` runs, **Then** both the library crate and the service binary build with no Liminis-internal dependencies.
2. **Given** the example consumer, **When** it ingests three sample documents and queries for an entity, **Then** results return without manual setup beyond the documented quickstart.

---

### User Story 6 - Telemetry and operator visibility (Priority: P3)

The service should emit per-call timing, token usage by role, fallback events, and WAL append/replay stats in a structured form the desktop app (and other consumers) can surface.

**Why this priority**: The current Python service is opaque; the 2026-04-30 caching audit required ad-hoc instrumentation. Worth doing once, in the rewrite.

**Independent Test**: Drive a known workload; verify metrics are emitted and counts match an external trace.

### Edge Cases

- WAL contains lines from a graphiti-core library version newer than the Rust service understands — must skip-with-warning, not crash, and surface unknown line types in a structured error.
- LadybugDB DB file open by another process (stale lock from a killed Python service) — must detect and surface a clear error, not silently corrupt.
- Embedding dimension changes (e.g., user swaps bge-base for a 1024-dim model) — must refuse to write into a graph whose existing embedding dim differs, unless an explicit rebuild flag is set.
- Anthropic API returning a 529 (overloaded) mid-extraction — must back off, not poison the WAL with partial episode state.
- A workspace last written by the Python service holds entity labels in an order the Python code enforced (Entity-first) — Rust service must enforce the same invariant on write.
- Consumer using the library API on a thread without an async runtime — must surface a clear error, not deadlock.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Service MUST expose the Unix-socket IPC contract currently consumed by liminis-framework's `graphiti_service.py` clients (request/response shapes for ingest, search, retrieve, admin).
- **FR-002**: Service MUST implement `add_episode(name, body, source, source_description, reference_time, group_id)` end-to-end: chunk → extract entities and edges → embed → dedup → persist → append WAL.
- **FR-003**: Service MUST implement `search` (edge hybrid) and `search_` (node hybrid using RRF over BM25 + vector) returning the same shape liminis-framework destructures today.
- **FR-004**: Service MUST implement `retrieve_episodes`, `remove_episode`, `EntityNode.get_by_group_ids`, `EntityEdge.get_by_group_ids`, `EntityEdge.get_by_uuids`, and pass-through raw Cypher execution.
- **FR-005**: Service MUST implement `build_indices_and_constraints` and `close` (clean WAL flush) lifecycle calls.
- **FR-006**: Service MUST support multi-label freeform entity classification (no closed `entity_types` ontology required) with the Entity-first label-order invariant.
- **FR-007**: Service MUST append a WAL line per mutation in JSONL, format-compatible with the existing `replay_wal_ladybug` reader and the `wal_chunk()` buffering contract.
- **FR-008**: Service MUST replay an existing WAL on cold boot, with chunked buffering, recovering from truncated final lines.
- **FR-009**: Service MUST allow extraction LLM, dedup LLM, and embedding model to be configured independently via env vars (`LCG_EXTRACTION_LLM`, `LCG_DEDUP_LLM`, `LCG_EMBEDDING_MODEL`) and support a primary→fallback chain per role.
- **FR-010**: Service MUST support the existing roster: Anthropic (Sonnet/Haiku) over HTTP for extraction; a local model for dedup via an out-of-process adapter (initially MLX qwen via subprocess or local HTTP); bge-base-en-v1.5 embeddings via an out-of-process adapter (initially Python sentence-transformers).
- **FR-011**: Service MUST implement a reader/writer split: write operations serialized per workspace, reads never blocked by an in-flight write.
- **FR-012**: Service MUST use LadybugDB native HNSW + full-text indices for dedup candidate generation when present, with brute-force cosine fallback when index population is below a configurable threshold.
- **FR-013**: Service MUST refuse to start against a DB whose persisted embedding dimension differs from the configured embedding model's dimension, unless an explicit `--rebuild-embeddings` flag is passed.
- **FR-014**: Service MUST emit structured telemetry: per-call timing, per-role token usage and cost, fallback events, WAL append/replay throughput.
- **FR-015**: Service MUST honor Anthropic prompt-caching headers and place static prompt content in system messages, per the 2026-04-30 caching audit (only on the Sonnet path where it is net-positive).
- **FR-016**: Service MUST ship as a standalone binary invokable without a Python runtime for the core graph path; LLM/embedding adapters MAY be out-of-process.
- **FR-017**: Service MUST support `reprocess_entity_types` (reclassify entities across a group) as an admin endpoint.
- **FR-018**: Service MUST scope every read and write by `group_id`, defaulting to `"liminis"` if unspecified.
- **FR-019**: Repo MUST be consumable as both a library crate (in-process embedding) and a binary (IPC service), with no compile-time dependency on any Liminis-internal code.
- **FR-020**: Repo MUST include a runnable `examples/` consumer that demonstrates ingest + search against a local LadybugDB file in under 50 lines.

### Key Entities

- **Episodic**: A chunk of source content (text/message/json) with `content`, `content_embedding`, `source`, `source_description`, `valid_at`, `group_id`.
- **Entity**: A node with multi-label classification (`labels: ['Entity', <freeform-type>, ...]`), `name`, `summary`, `name_embedding`, `group_id`.
- **RELATES_TO edge**: An entity-to-entity relationship with `fact`, `fact_embedding`, temporal validity (`valid_at`, `invalid_at`), `group_id`.
- **MENTIONS edge**: Episodic-to-entity link recording which entities appear in which episode.
- **WAL line**: A JSON record per mutation: `{op, timestamp, payload}`, replay-deterministic.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A workspace last written by the Python service can be opened by the Rust service with zero schema migration and zero data loss across nodes, edges, and embeddings.
- **SC-002**: For a fixed 50-query golden set, top-10 search results match the Python service's results with rank-correlation ≥ 0.9.
- **SC-003**: Cold-boot WAL replay throughput ≥ 3× the Python baseline on the same workspace.
- **SC-004**: p95 search latency stays under 500 ms while an extraction backlog of ≥ 100 episodes is processing.
- **SC-005**: Dedup wall time on a 50k-entity workspace is ≤ 30% of the Python brute-force baseline, with dedup-decision overlap ≥ 95%.
- **SC-006**: Memory footprint at steady state with a 100k-node workspace open is ≤ 60% of the Python service's footprint on the same workspace.
- **SC-007**: 100% of the IPC calls liminis-framework makes today have a passing parity test against the Python service on a recorded request/response corpus.
- **SC-008**: A non-Liminis consumer following the README quickstart can run ingest + search against a local LadybugDB file in under one hour from clone to working query.

## Assumptions

- LadybugDB's Rust bindings are stable enough to be load-bearing for the core query path (validation: spike against the current release before committing).
- The first version keeps LLM and embedding providers out-of-process: extraction calls Anthropic HTTP directly; the dedup model and embedder are reached via local HTTP or subprocess shims so the Rust binary stays free of MLX/sentence-transformers. Cross-process IO is acceptable overhead.
- The Anthropic prompt-caching audit's conclusion holds: caching is enabled on the Sonnet extraction path only.
- Existing in-process Node MCP servers (knowledge-reader, knowledge-writer, semantic-search proxy) do not change; the swap is at the Python-service boundary.
- The Rust service is shipped alongside the Python service (feature-flagged in liminis-framework) for at least one release so parity bugs can be diagnosed against a live oracle.
- The repo lives at `~/dev/liminis-project/liminis-graph` and is published under the Liminis org once it is past internal-alpha quality.

## Out of Scope

- Replacing the Node-side MCP servers or the chat agent's retrieval glue.
- Community detection, cross-encoder reranking, bulk loader, OpenTelemetry export.
- Drivers for Neo4j, Kuzu, or FalkorDB (LadybugDB only).
- Replacing the MLX dedup model or the sentence-transformers embedder in-process (out-of-process adapters for v1).
- Schema migrations to a new graph shape (this is a port, not a redesign).
- Hosted/multi-tenant deployment.

## Source References

- `graphiti/graphiti_core/` — surface to preserve (especially `driver/ladybugdriver.py`, `nodes.py`, `edges.py`, `search/`).
- `liminis-framework/framework/src/skills/knowledge-graph/scripts/graphiti_service.py` — the call sites and IPC contract this service must serve.
- ADR-042 (concurrent read/write split), ADR-052 (LadybugDB migration), `project_context_graph_caching_2026_04_30.md`, `project_hnsw_migration.md`, `project_qr_leak_fix.md`.
