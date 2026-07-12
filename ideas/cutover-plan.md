# liminis-graph cutover plan

**Status:** Draft for discussion, 2026-05-24
**Context:** ~85% of the 2026-05-21 audit's 26-method contract is implemented. The remaining work is no longer "can we port this?" but "how do we swap out `graphiti_service.py` safely?"

## Goal

Replace `liminis-framework/graphiti_service.py` with the Rust `liminis-graph` binary as the production graph backend, while keeping reader/writer MCP servers and `service_client.py` unchanged.

## Non-goals

- Replacing the chunking pipeline. Chunking lives in **liminis-app** (TS), not in graphiti or its replacement. liminis-app chunks markdown documents and feeds the job queue a stream of chunks; the graph backend only ever sees `process_chunk`. The graphiti-era `process_document` / `index_document` methods are vestigial from graphiti's intended usage as a standalone document ingester — we never used them.
- Replacing the embedder. CoreML sidecar work (liminis#794–797, liminis-framework#179) is in flight separately.
- Rewriting reader/writer MCP servers. They speak the IPC protocol; swapping the daemon doesn't change their job.

## Stage 0 — Finish the contract (small, ~1–2 PRs)

Decide what to do with the remaining graphiti methods.

| Method | Recommendation | Why |
|---|---|---|
| `knowledge_list_sources` | Port to Rust | Read-only; trivial against current DB |
| `knowledge_preview_chunks` | Port to Rust | Read-only inventory |
| `knowledge_suggest_duplicates` | Port to Rust | Already have brute-force dedup logic from issue #59 |
| `knowledge_entity_edge_analysis` | Port to Rust | Read-only analytics |
| `knowledge_index_document` | **Drop** | Vestigial — chunking lives in liminis-app, not in the backend |
| `knowledge_process_document` | **Drop** | Same |

**Output:** ADR documenting that the backend's surface ends at `process_chunk` and explicitly excluding document-level orchestration. Four implementation issues filed in liminis-graph. Effective contract size: **24 methods** (was 26 — dropping the two vestigial document methods).

## Stage 1 — Collapse the audit's straddle points

These are in **liminis-app**, not liminis-graph. They exist because graphiti_service.py and liminis-graph both run today; consolidating now makes cutover surgical instead of risky.

1. **Tool schemas (3× duplication).** Single source for the knowledge_* tool definitions. Reader and writer MCP servers should consume schemas, not embed them.
2. **Path conventions (3 files).** `graphiti-service-lifecycle.ts`, `graphiti_service.py`, `graphiti-handlers.ts` all hard-code the `.graphiti/` layout. One canonical Constants module.
3. **Env-var parsing.** `common.py:GraphitiConfig.from_env` + Electron's `loadEnvFile` parse the same vars differently. Consolidate.
4. **GraphitiPanel leaky state.** Indexing pause/resume/schedule reads from both IPC and renderer-local state. Move to main-process owned (per ADR-032).

**Output:** Four liminis issues. Each can ship independently of the cutover; they make the cutover easier.

## Stage 2 — Cutover prerequisites

Things that must work before we can flip the default backend.

1. **Embedder sidecar lifecycle.** Today the Swift sidecar must be launched manually. liminis-app must spawn/supervise it just like it does graphiti_service.py. Covered by liminis#795 (`LOCAL_INFERENCE_SOCKET` env propagation) and liminis-framework#179 (UDS client cutover). Both already filed.
2. **Binary bundling.** liminis-graph must ship inside the .app, not require a dev symlink. Covered by liminis#794 (mlmodelc cache setup) — extend scope to cover the Rust binary itself if not already.
3. **Feature flag.** Backend choice per-workspace, not global. Options to design between:
   - **Workspace-local `.env`** (already user-managed; consistent with how `ANTHROPIC_API_KEY` etc. are scoped).
   - **liminis-app settings store** (workspace-keyed map in main-process state, surfaced via IPC).
   - A global `GRAPHITI_BACKEND` env var is **not** sufficient — it would force every workspace to the same backend, defeating the gradual-rollout goal in Stage 3. Defaults to `python` during validation; flip new workspaces to `rust` for cutover.
4. **Side-by-side validation harness.** Spin up both backends against a sample workspace, replay the same IPC sequence, diff responses. Needs to be smarter than `diff`: must handle streaming `_progress_token` responses (only `knowledge_rebuild_from_wal` uses them today), normalize non-deterministic fields (uuids, timestamps, ordering of nondeterministic result sets) before comparison, and treat semantic equivalence as success — exact equality is unrealistic. Implementation choice (bash + jq vs a small Rust/TS tool) deferred until we get there; the requirement is "structured diff that ignores known noise."

**Output:** Cutover is gated on these. None of them require touching liminis-graph itself; they're all in the integration layer.

## Stage 3 — Cutover

The actual switch.

1. **Default new workspaces to the Rust backend** (using the per-workspace flag mechanism chosen in Stage 2). Existing workspaces keep their current setting.
2. **Monitor for one week.** Watch for crashes, IPC errors, embedding mismatches, perf regressions.
3. **Flip existing workspaces** to Rust once stable. Keep Python as the manual override for diagnosis.

**Output:** No code change beyond defaults. The risk is operational, not technical.

> **WAL backup blocker resolved (2026-05-25, issue #74).** The application WAL was never populated by the Rust binary (`WalWriter::log_mutation` had zero callers). This meant switching a workspace from Python to Rust silently dropped its WAL backup story — `knowledge_rebuild_from_wal` was non-functional on Rust-ingested data. Fixed: every Cypher mutation is now appended to the WAL after successful execution, restoring parity with the Python `LadybugDriver` pipeline. Stage 3 is no longer blocked by this issue.

## Stage 4 — Tear down

After 2–4 weeks of stable Rust-default operation:

1. Delete `graphiti_service.py`, `common.py`, the Python `service_protocol.py` dispatch (keep the contract definition as documentation).
2. Drop the dependency on the graphiti fork in `liminis-framework`. Archive the fork repo (or freeze it at the last working commit).
3. Remove `GRAPHITI_BACKEND` flag.
4. Update ADR-035 (GraphRAG with Graphiti & FalkorDB) → mark superseded by a new ADR pointing at liminis-graph.
5. Complete the rename umbrella (liminis-graph#64 Part 1/3 + the two follow-on parts in liminis and liminis-framework).

**Output:** ~2000 lines of Python deleted. One fewer language runtime in the app. graphiti fork frozen.

## Sequencing notes

(Stage numbering in this document refers to the cutover plan only. Distinct from the rename migration's Phase A / B / C scheme in `liminis-graph/src/main.rs`.)

- **Stages 0 and 1 are parallelizable.** Different repos, different concerns.
- **Stage 2 is the critical path.** Embedder lifecycle and binary bundling are blockers; the feature flag is trivial.
- **Stage 3's "monitor for a week" is real time, not work time.** Plan accordingly.
- **Stage 4 should not start until Stage 3 has stabilized.** Otherwise we lose the ability to fall back.

## Open questions

1. **Bundling layout.** Does the Rust binary live in `app.asar.unpacked/bin/liminis-graph` alongside the Swift sidecar, or does it have its own resource path? (Affects spawn-env construction.)
2. **Graphiti fork future.** Freeze at HEAD, or actively maintain for upstream PRs we still want to land? (Recent caching work suggests we may still want changes.)
3. **OSS launch interaction.** OSS launch is **confirmed**: MIT, no CLA, but deferred — not blocking on cutover. The MCP-in-binary vs out-of-process question should be revisited closer to launch, but doesn't drive Stage 4 sequencing.
4. **Naming.** Are we cutting over to "graphiti backend = liminis-graph" or to "Liminis Context Graph"? The rename and the cutover overlap; doing both at once is cleaner but bigger.
