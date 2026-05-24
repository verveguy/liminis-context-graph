# ADR 0042: Reader/Writer Split via `tokio::sync::RwLock`

**Date**: 2026-05-19
**Status**: Accepted

## Context

`lbug` (LadybugDB, v0.16.1) serializes all write connections internally: a second concurrent write returns `Error::FailedQuery("Connection exception: write query is in progress")`. Without explicit write serialization in Rust, concurrent `add_episode` calls from different tokio tasks will collide on lbug and return `-32000` JSON-RPC errors to callers.

At the same time, extraction via the Anthropic API takes approximately 30 seconds per episode. If a write lock were held for the full duration of extraction, search reads would queue behind every in-flight extraction — violating the core UX promise (p95 search latency ≤ 500 ms, SC-001).

## Decision

A `tokio::sync::RwLock<()>` is carried in `AppState` alongside `Arc<Db>`.

**Write path** (`add_episode`, `delete_episode`, `build_indices`):

1. All async HTTP work (extraction, embedding, DedupAdapter verification) runs first, with **no lock held**.
2. The write guard is acquired (`write_lock.write().await`) immediately before the DB commit `spawn_blocking` closure.
3. The guard is **not** moved into the closure (`RwLockWriteGuard` is not `Send`); it stays alive in the enclosing async stack frame while `spawn_blocking` executes and is dropped after `.await` returns.

**Read path**:

*Hot search handlers* (`find_entities`, `find_relationships`): no lock is held. These are on the critical latency path and lbug supports concurrent reads natively; holding a read guard here would cause them to queue behind in-progress writes unnecessarily.

*Other retrieval handlers* (`get_episodes`, `get_nodes_by_group`, `get_edges_by_group`, `get_edges_by_uuids`, `query_cypher`):

1. A shared read guard is acquired (`write_lock.read().await`) before the DB query `spawn_blocking` closure.
2. The guard stays alive in the enclosing async stack frame while `spawn_blocking` executes and is dropped after `.await` returns.

This means:
- Concurrent search reads are never blocked by in-flight LLM calls (30 s). They are blocked only by the brief DB commit window (< 10 ms typical for lbug mutations).
- Concurrent writes are serialized: a second `add_episode` reaching Phase C while one is in progress will wait at `write().await` rather than receiving a lbug error.
- The lock is per-process (single workspace per process), satisfying the per-workspace requirement from the spec.

## Consequences

- **p95 search latency**: bounded by the lbug commit duration, not the LLM call duration. Bench in `benches/concurrent_rw.rs` asserts ≤ 500 ms with ≥ 100 concurrent extraction tasks.
- **Write throughput**: single-writer serialization. `N` concurrent `add_episode` calls overlap freely during Phases A and B (HTTP + dedup); only Phase C (DB commit) queues. For typical episode sizes, Phase C is fast and contention is low.
- **lbug error elimination**: the hard `-32000` error under concurrent extraction load is eliminated by ensuring the write guard is always held during lbug write connections.
- **Conn<'db> lifetime**: the existing `db.connect()` inside `spawn_blocking` pattern is preserved. The `Arc<Db>` clone into the closure is sufficient for lbug's lifetime requirements.

## SC-003 Deferral Note

Acceptance criterion SC-003 requires that the Anthropic prompt-cache hit rate on the Sonnet extraction path meets or exceeds the baseline in `project_context_graph_caching_2026_04_30.md`. That file does not exist in this repository.

**This issue (Issue #4) delivers the structural prompt-caching changes only**:
- `anthropic-beta: prompt-caching-2024-07-31` header on the Sonnet path.
- `"cache_control": {"type": "ephemeral"}` on the system message object, Sonnet path only.
- Token usage parsing for `cache_read_input_tokens` and `cache_creation_input_tokens` (already implemented).

The quantitative hit-rate assertion (SC-003) is deferred to follow-up issue **#12 "Establish prompt-caching baseline measurement in-repo"**. That issue must record the baseline figure and add a bench or test that asserts against it on a fixed replay corpus.

## References

- Project constitution: `.specify/memory/constitution.md` v1.0.0 — Principle V (adapters out-of-process), Principle IV (WAL before DB write)
- Original liminis-project ADR-042: `kiln/worktrees/verveguy-liminis/docs/project_notes/decisions/adr-042.md` (Python asyncio.Lock model; superseded by this lbug-specific Rust design)
- Issue #4 spec: `specs/001-rust-knowledge-graph/issues/004-concurrent-rw-llm-routing-spec.md`
- Implementation plan: `specs/001-rust-knowledge-graph/issues/004-concurrent-rw-llm-routing-plan.md`
