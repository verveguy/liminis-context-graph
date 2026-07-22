# Architecture Decision Records

Decisions are recorded chronologically. Numbers are project-local and immutable once assigned. See [ADR-0001](0001-record-architecture-decisions.md) for the format.

| ADR | Title | Date |
|-----|-------|------|
| [0001](0001-record-architecture-decisions.md) | Record Architecture Decisions | 2026-05-19 |
| [0002](0002-reader-writer-split.md) | Reader/Writer Split via `tokio::sync::RwLock` | 2026-05-19 |
| [0003](0003-arcswap-db-hot-swap.md) | `ArcSwap<Db>` for Live Database Replacement in `clear_all` | 2026-05-22 |
| [0004](0004-classify-entities-trait.md) | Add `classify_entities` to the `Extractor` trait |  |
| [0005](0005-streaming-ipc-progress-framing.md) | Streaming IPC Progress Framing via `_progress_token` | 2026-05-22 |
| [0006](0006-embedder-http-contract.md) | HTTP Embedding Sidecar Contract | 2026-05-22 |
| [0007](0007-relates-to-two-hop-traversal.md) | Two-Hop RELATES_TO Traversal as Canonical Read Pattern | 2026-05-22 |
| [0008](0008-context-graph-multi-connection-pool.md) | Named Multi-Connection Pool for ContextGraphSocketClient | 2026-05-22 |
| [0009](0009-degraded-mode-startup-recovery.md) | Degraded-Mode Startup and In-Process Recovery | 2026-05-24 |
| [0010](0010-tool-use-extraction.md) | Migrate do_extract to tool_use structured output | 2026-05-24 |
| [0011](0011-auto-heal-write-lock-acquisition.md) | Auto-Heal Write-Lock Acquisition from Search Handlers | 2026-05-24 |
| [0012](0012-edge-episode-via-entity-traversal.md) | Edge-to-Episode Associations via Either-Endpoint Entity Traversal | 2026-05-25 |
| [0013](0013-cancellation-token-shutdown.md) | CancellationToken as the Single Shutdown Signal on AppState | 2026-05-25 |
| [0014](0014-ontology-extractor-trait-parameter.md) | Pass `Option<&Ontology>` as a call-time parameter to `Extractor::extract` | 2026-05-25 |
| [0015](0015-wal-drain-and-flush-pattern.md) | WAL Drain-and-Flush Pattern for Production Write Handlers | 2026-05-25 |
| [0016](0016-oai-embedding-contract-uds-transport.md) | OpenAI-compatible embedding contract over UDS; hyper for UDS transport | 2026-05-25 |
| [0017](0017-replace-process-exit-with-normal-return.md) | Replace `std::process::exit(0)` with Normal Return in async main | 2026-05-25 |
| [0018](0018-ontology-hash-sidecar.md) | Ontology Hash Sidecar for Drift Detection | 2026-05-26 |
| [0019](0019-workspace-migration-resume-vs-schism.md) | Workspace Migration Partial-Resume vs. Schism Marker | 2026-05-26 |
| [0020](0020-ipc-collection-envelope-contract.md) | IPC Collection Response Envelope Contract | 2026-05-26 |
| [0021](0021-cargo-dist-build-setup-env-injection.md) | Inject `LBUG_BUILD_FROM_SOURCE` via cargo-dist `github-build-setup` | 2026-06-01 |
| [0022](0022-lbug-cypher-escaping-convention.md) | lbug Cypher Escaping Convention — Backslash, Not SQL Doubling _(superseded)_ | 2026-06-12 |
| [0023](0023-legacy-wal-translation-module.md) | Legacy-WAL Translation Layer — Cypher-text/Param-shape vs. Param-value Module Split | 2026-06-15 |
| [0024](0024-bound-parameter-db-access.md) | Bound-Parameter DB Access — Retire Cypher String Interpolation | 2026-06-15 |
| [0025](0025-auto-heal-index-build.md) | Auto-Heal Index Build and Bulk-Load Reload Pattern | 2026-06-17 |
| [0026](0026-episode-cursor-wal-resume.md) | Episode-Cursor WAL Resume for Checkpoint Recovery | 2026-06-18 |
| [0027](0027-autonomous-wal-startup-recovery.md) | Autonomous WAL-Corruption Self-Recovery on Startup | 2026-06-18 |
| [0028](0028-db-wal-dump-compaction.md) | DB→WAL Dump / Compaction Pattern | 2026-06-22 |
| [0029](0029-name-first-entity-resolution.md) | Name-First Entity Resolution in add_episode Phase B | 2026-06-22 |
| [0030](0030-batched-write-lock-for-long-running-passes.md) | Batched Write-Lock Acquisition for Long-Running Passes | 2026-06-22 |
| [0031](0031-orphaned-direct-rels-after-noise-deletion.md) | Orphaned Direct RELATES_TO Rels After Noise Edge Deletion _(superseded)_ | 2026-06-22 |
| [0032](0032-ontology-parent-edges-conditional-hash-segment.md) | Ontology `parent_edges:` segment conditionally included in content hash | 2026-06-23 |
| [0033](0033-noise-edges-reclassified-not-deleted.md) | Noise Edges Are Reclassified to UNCLASSIFIED, Not Deleted | 2026-06-23 |
| [0034](0034-observable-index-build-outcome.md) | Observable Index-Build Outcome — Fixing ADR-0025's Dead-Code Failure Path | 2026-07-16 |
| [0035](0035-mcp-stdio-transport.md) | MCP-over-stdio Transport Architecture | 2026-07-21 |

## Historical numbering

Before 2026-07, ADRs lived in two directories (`docs/adr/` numbered `0042`+ and a top-level
`adrs/` numbered from `0001`) with colliding, sometimes duplicated numbers. They were
consolidated into this directory under the single sequence above. Historical documents —
`specs/`, old issues/PRs, commit messages — may cite the old numbers; this table decodes them.
References to `ADR-035`/`ADR-042` prefixed with "the Liminis app's" refer to the parent
application's separate ADR index, not this one.

| Old (in `docs/adr/`) | New | | Old (in `adrs/`) | New |
|---|---|---|---|---|
| 0001 (meta) | 0001 | | 001 (wal-drain) | 0015 |
| 0042 | 0002 | | 0001 | 0012 |
| 0043 (arcswap) | 0003 | | 0002 | 0013 |
| 0043 (classify-entities) | 0004 | | 0003 | 0014 |
| 0043 (streaming-progress) | 0005 | | 0004 | 0018 |
| 0044 (embedder-http) | 0006 | | 0005 | 0019 |
| 0044 (two-hop) | 0007 | | 0006 | 0021 |
| 0045 | 0008 | | 0007 | 0022 |
| 0046 (degraded-mode) | 0009 | | 0008 | 0023 |
| 0046 (tool-use) | 0010 | | 0009 | 0024 |
| 0047 | 0011 | | 0047 | 0025 |
| 0048 | 0016 | | 0048 | 0027 |
| 0049 | 0017 | | 0049 | 0028 |
| 0050 | 0020 | | 0050 | 0029 |
| 0051 | 0026 | | 0051 | 0030 |
| | | | 0052 | 0031 |
| | | | 0053 | 0032 |
| | | | 0054 | 0033 |
