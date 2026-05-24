# Issue #4 Spec: Concurrent Reader/Writer with Per-Role LLM Routing

**Issue**: #4  
**Created**: 2026-05-19  
**Status**: Specified  
**Maps to**: User Story 3 — Concurrent reader/writer with per-role LLM routing (P2)  
**Blocked by**: #2 (IPC parity)

---

## Goal

Extraction (Sonnet, ~30 s/episode) must not block search reads. The service must enforce the reader/writer split from ADR-042, route each LLM role (extraction, dedup, embedding) to independently-configured providers, and fall back automatically on primary failure. Telemetry counters for fallback events and per-role token usage are part of this issue; the full telemetry surface is deferred to US6 (#6).

---

## User Stories

### Story 1 — Search reads never blocked by in-flight extractions (P1)

A developer runs a long extraction backlog and issues concurrent search queries. Search results are returned at or below the performance budget regardless of how many extraction workers are active.

**Why P1**: This is the core UX promise of the reader/writer split. Collapsing reads and writes onto the same lock would defeat the purpose of the rewrite.

**Independent test**: Start a 100-episode extraction backlog; concurrently fire search queries from a separate process; assert p95 search latency ≤ 500 ms for the duration.

**Acceptance scenarios**:

1. **Given** a backlog of 100 episodes extracting, **When** search queries fire concurrently, **Then** search p95 ≤ 500 ms.
2. **Given** a write lock held by an in-flight extraction, **When** a search query arrives, **Then** the search completes without waiting for the write to release.

---

### Story 2 — Per-role LLM routing with automatic fallback (P1)

An operator configures extraction, dedup, and embedding to different providers via env vars. When the primary provider for a role fails, the service falls back to the configured secondary, logs the failure exactly once per process lifetime (not per call), and continues serving.

**Why P1**: Ops levers for provider routing and graceful degradation on API failure are required from day one; without them a single misconfigured key or transient outage takes down the entire ingest pipeline.

**Independent test**: Set `LCG_EXTRACTION_LLM` to an invalid key; trigger extraction; verify the fallback model is used and exactly one error line is written to the log.

**Acceptance scenarios**:

1. **Given** a misconfigured Anthropic key, **When** extraction runs, **Then** the local fallback model produces results and the failure is logged exactly once per process lifetime, not once per call.
2. **Given** separate env vars for extraction, dedup, and embedding, **When** the service starts, **Then** each role uses the configured provider independently.
3. **Given** a primary extraction LLM that returns HTTP 529, **When** extraction is in progress, **Then** the service backs off and retries (or fails over), and no partial episode state is written to the WAL.

---

### Story 3 — Anthropic prompt-cache efficiency maintained on the Sonnet path (P2)

Repeated extraction runs against similar corpora hit the Anthropic prompt cache at or above the baseline established in the 2026-04-30 caching audit. Static system message content stays stable across calls so the cached prefix is not invalidated by variable data.

**Why P2**: Cache misses on the Sonnet path add latency and token cost; the caching audit already paid the cost of establishing the right prompt structure — this issue must not regress it.

**Independent test**: Run a fixed extraction corpus 10 times; assert the Anthropic-reported cache hit rate (from response headers or API metadata) meets the baseline from `project_context_graph_caching_2026_04_30.md`.

**Acceptance scenarios**:

1. **Given** the Sonnet extraction path, **When** repeated extractions run on similar content, **Then** Anthropic prompt-cache hit-rate meets or exceeds the baseline documented in `project_context_graph_caching_2026_04_30.md`.
2. **Given** the Haiku or local-model paths, **When** prompt caching is not applicable, **Then** no cache-control headers are sent and no cache-hit metric is recorded.

---

## Scope

### In scope

- Reader/writer split: write operations serialized per workspace; reads never blocked by an in-flight write (per ADR-042).
- Per-role LLM routing via env vars `LCG_EXTRACTION_LLM`, `LCG_DEDUP_LLM`, `LCG_EMBEDDING_MODEL`; each var accepts a primary and optional fallback identifier.
- Adapters (all out-of-process per Principle V):
  - Anthropic HTTP client for extraction (Sonnet/Haiku).
  - Local-model adapter for dedup — initially MLX qwen reached via subprocess or local HTTP.
  - Embedder adapter for `bge-base-en-v1.5` — initially Python sentence-transformers reached via local HTTP.
- Anthropic prompt-caching on the Sonnet extraction path only: static content in system messages, per the 2026-04-30 audit.
- Telemetry hooks for: fallback events (provider, role, error class), and per-role token usage (input, output, cache hit/miss tokens where applicable).
- Benchmark in `benches/` proving the p95 search-latency budget while extractions are in flight.

### Out of scope

- HNSW + BM25 dedup at scale (US4 / #5).
- Full telemetry surface beyond per-role counters and fallback events (US6 / #6).
- Replacing or modifying the existing IPC surface (that belongs to #2).
- In-process model loading (Principle V is NON-NEGOTIABLE).

---

## Requirements

### Functional Requirements

- **FR-001**: Service MUST implement a reader/writer split where write operations are serialized per workspace and reads are never blocked by an in-flight write, as specified in ADR-042.
- **FR-002**: Service MUST accept `LCG_EXTRACTION_LLM`, `LCG_DEDUP_LLM`, and `LCG_EMBEDDING_MODEL` env vars to configure each role's provider independently.
- **FR-003**: Each role env var MUST support a primary and an optional fallback provider; if the primary fails, the service MUST use the fallback without restarting.
- **FR-004**: On primary-provider failure for a role, the service MUST log the failure exactly once per process lifetime (not once per request).
- **FR-005**: Service MUST implement an Anthropic HTTP adapter for extraction supporting at minimum `claude-sonnet-4-5` (or current Sonnet) and `claude-haiku-4-5` (or current Haiku).
- **FR-006**: Service MUST implement an out-of-process dedup adapter callable via subprocess or local HTTP (initially targeting MLX qwen).
- **FR-007**: Service MUST implement an out-of-process embedding adapter callable via local HTTP (initially targeting Python sentence-transformers serving `bge-base-en-v1.5`).
- **FR-008**: On the Sonnet extraction path, service MUST place static prompt content in system messages and send Anthropic prompt-caching headers to maintain cache hit rate ≥ the baseline from the 2026-04-30 audit.
- **FR-009**: On non-Sonnet paths (Haiku, local model), service MUST NOT send prompt-caching headers.
- **FR-010**: Service MUST emit a telemetry event per fallback activation containing: role, primary provider attempted, error class, fallback provider used.
- **FR-011**: Service MUST emit per-role token-usage counters: input tokens, output tokens, and (Sonnet path only) cache-hit tokens and cache-miss tokens.
- **FR-012**: On Anthropic HTTP 529 (overloaded), service MUST back off and NOT write partial episode state to the WAL.
- **FR-013**: Service MUST include a benchmark in `benches/` that launches ≥ 100 concurrent extraction tasks and measures p95 search latency, asserting ≤ 500 ms.
- **FR-014**: Tasks touching adapters MUST be tagged `[ADAPTER]`, search hot paths `[HOT]`, IPC handlers `[IPC]` in the task list.

### Key Entities

- **LlmRouter**: Routes each role (extraction, dedup, embedding) to its configured provider; manages primary→fallback transitions; emits telemetry on fallback events.
- **ExtractionAdapter**: Out-of-process Anthropic HTTP client; constructs prompts with static system-message content for caching; handles 429/529 back-off.
- **DedupAdapter**: Out-of-process local-model adapter; reached via subprocess or local HTTP; configurable endpoint.
- **EmbeddingAdapter**: Out-of-process embedder adapter; reached via local HTTP; configurable endpoint and model name.
- **RwLock (workspace)**: Synchronization primitive enforcing the reader/writer split per workspace; write path acquires exclusive; read path acquires shared.
- **TelemetryHook**: Sink for per-role token-usage counters and fallback events; emits structured records consumable by US6 (#6).

---

## Success Criteria

- **SC-001**: p95 search latency ≤ 500 ms under a concurrent 100-episode extraction backlog, as proven by the bench in `benches/`.
- **SC-002**: A misconfigured primary-extraction provider logs exactly one error line per process lifetime and falls back cleanly; no extraction requests fail visible to callers.
- **SC-003**: Anthropic prompt-cache hit rate on the Sonnet extraction path meets or exceeds the baseline in `project_context_graph_caching_2026_04_30.md` on a fixed replay corpus.
- **SC-004**: Per-role token-usage telemetry is emitted and countable for a known extraction workload.
- **SC-005**: No ML-runtime crate (`tch`, `candle`, `onnxruntime`, etc.) appears in `cargo tree` output after this issue lands.

---

## Assumptions

- **Fallback chain format**: `LCG_EXTRACTION_LLM` (and sibling vars) accept a colon-separated list, e.g. `claude-sonnet-4-5:claude-haiku-4-5`, where the first entry is primary and the second is fallback. A single value means no fallback. The Plan stage will confirm or revise this format.
- **"Once per session"** means once per process lifetime (from service start to stop). The per-role error deduplification is in-memory and is not persisted across restarts.
- **Prompt-cache baseline** is defined in `project_context_graph_caching_2026_04_30.md` in the broader `liminis-project` tree. The Plan stage is responsible for reading and quoting the exact hit-rate figure into the bench assertion.
- **ADR-042** (reader/writer split) is a design decision that must be authored as part of this issue's deliverables (it is referenced but does not yet exist in `docs/adr/`).
- **Local adapter endpoints** are configurable via env vars (e.g. `LCG_DEDUP_ADAPTER_URL`, `LCG_EMBEDDING_ADAPTER_URL`) with defaults appropriate for local development; these details are resolved in the Plan stage.
- **Telemetry output format** is structured log lines (same channel as existing service logging) since the full telemetry surface (US6) is deferred; no separate metrics server is introduced here.
- The reader/writer split applies per workspace (i.e. per `group_id`-scoped DB file), not globally across all workspaces.
- All adapters are reached over localhost; no TLS or authentication is required for the local adapters in v1.

---

## Out of Scope

- HNSW + BM25 hybrid dedup (US4 / #5).
- Full telemetry/metrics server surface (US6 / #6).
- IPC surface changes (US1 / #2).
- In-process model loading of any kind (constitution Principle V).
- Cross-workspace or multi-tenant reader/writer concerns.
