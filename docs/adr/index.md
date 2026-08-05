---
layout: default
title: ADR Index
---

# Architecture Decision Records

**These are historical decision records, not current-state documentation.** Each ADR captures
the reasoning behind a decision at the time it was made and is never edited to reflect later
changes — some entries below are explicitly marked `_(superseded)_`, and others (e.g. ADR-0025's
lazy index build, later revisited by ADR-0034/ADR-0036's eager build) describe behavior the
codebase has since moved past without the ADR text itself being marked. For what the system
does *today*, use the reference pages linked from the [documentation home](../) —
[Operations](../operations.md), [Configuration](../configuration.md), and
[IPC & MCP Reference](../ipc-mcp-reference.md) — and treat an ADR as the record of *why*, not a
live description of *what*.

Numbers are project-local and immutable once assigned. See [ADR-0001](0001-record-architecture-decisions.md) for the format.

**From 2026-07-30, a new ADR takes the number of the GitHub issue that motivated it** — [`0283-name-index-scan-fallback-for-endpoint-authority.md`](0283-name-index-scan-fallback-for-endpoint-authority.md), from issue #283, is the first — matching the `specs/<issue_number>-<slug>/` convention and for the same reason: a shared sequential counter is claimed at branch time, so two issues in flight always claim the same number. `0001`–`0052` predate this and keep their sequential numbers — **the gap between `0052` and the first issue-numbered ADR is expected, not missing history.** See CLAUDE.md for the full rule.

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
| [0036](0036-eager-index-build-at-startup.md) | Eager HNSW/FTS Index Build at Startup + Dedup-Path Auto-Heal | 2026-07-24 |
| [0037](0037-relation-classification-abstention-writes-unclassified.md) | Relation Classification Has No Open-Ended Mode and Abstention Writes `UNCLASSIFIED` | 2026-07-25 |
| [0038](0038-in-process-name-index.md) | In-Process NameIndex Accelerator for Entity Name Lookup | 2026-07-25 |
| [0039](0039-uds-embedder-connection-pooling.md) | UDS Embedder Connection Pooling | 2026-07-25 |
| [0040](0040-attached-mode-reconnect-retry-boundary.md) | Attached-Mode Reconnect — Retry Only Write-Time Failures | 2026-07-25 |
| [0041](0041-local-openai-compatible-extraction-adapter.md) | Local/OpenAI-Compatible Extraction Adapter | 2026-07-25 |
| [0042](0042-oai-extractor-uds-connection-pooling.md) | OaiExtractor UDS Connection Pooling | 2026-07-25 |
| [0043](0043-wal-replay-seq-ordering-and-noop-accounting.md) | WAL Replay — Seq-Based File Ordering and MATCH-Write No-Op Accounting | 2026-07-25 |
| [0044](0044-llm-cassette-record-replay-seam.md) | LLM Cassette Record/Replay Seam | 2026-07-26 |
| [0045](0045-wal-replay-prepared-statement-cache-scope.md) | WAL Replay Prepared-Statement Cache — LRU-1 Scope and Deferred Connection Recycling | 2026-07-26 |
| [0046](0046-wal-replay-failure-dedup-and-rebuild-idempotency.md) | WAL Replay — Deduplicated Failure Samples and Fail-Fast Rebuild Idempotency | 2026-07-26 |
| [0047](0047-wal-replay-transaction-boundaries.md) | WAL Replay Transaction Boundaries — Batch-Aligned, Not Chunk-Aligned | 2026-07-26 |
| [0048](0048-rust-extraction-quality-eval-harness.md) | Rust Extraction-Quality Eval Harness — Architecture and Judge Design | 2026-07-26 |
| [0049](0049-bare-path-ontology-loader-and-cli-mode-override.md) | Bare-Path Ontology Loader and CLI Mode-Override Precedence | 2026-07-27 |
| [0050](0050-blind-pairwise-judging.md) | Blind Pairwise Judging for the Extraction-Quality Eval Harness | 2026-07-27 |
| [0051](0051-edge-endpoint-salvage-and-deferred-drop.md) | Edge Endpoint Salvage and Deferred Drop Decision | 2026-07-29 |
| [0052](0052-lcg-eval-dry-run-shares-the-real-run-resolution-path.md) | `lcg-eval --dry-run` Shares the Real Run's Resolution Path | 2026-07-29 |
| [0283](0283-name-index-scan-fallback-for-endpoint-authority.md) | Bounded Scan Fallback and Trust State for NameIndex Endpoint Resolution | 2026-07-30 |
| [0295](0295-github-pages-documentation-site.md) | GitHub Pages Documentation Site | 2026-08-02 |
| [0298](0298-ci-failure-notification.md) | CI Failure Notification for Non-Gating Workflows | 2026-07-30 |
| [0306](0306-extraction-failure-sidecar-and-truncation-visibility.md) | Extraction-Failure Sidecar and Truncation Visibility | 2026-08-01 |
| [0307](0307-token-budget-policy-and-edge-exhaustion-semantics.md) | Token-Budget Policy and Edge Budget-Exhaustion Semantics | 2026-08-01 |
| [0310](0310-strict-mode-reclassifies-not-drops.md) | Strict-Mode Relation-Type Filtering Reclassifies, Never Drops | 2026-08-02 |
| [0312](0312-entity-strict-mode-reclassifies-not-drops.md) | Strict-Mode Entity-Type Filtering Reclassifies, Never Drops | 2026-08-02 |
| [0314](0314-missing-summary-salvage-and-schema-invalid-classification.md) | Missing-Summary Salvage and `schema_invalid` Classification | 2026-08-02 |
| [0316](0316-bench-target-per-criterion-group.md) | One `[[bench]]` Target Per `criterion_group!` | 2026-08-02 |
| [0322](0322-ci-docs-only-fast-path.md) | CI Docs-Only Fast Path via Job-Level Skip | 2026-08-02 |
| [0325](0325-knowledge-status-open-db-not-queryable.md) | `knowledge_status` Reports "Open But Not Queryable" as a Second Degraded State | 2026-08-02 |
| [0328](0328-real-corpus-e2e-on-pr-path.md) | Run `real-corpus-e2e` on the PR Path as a Non-Required Check | 2026-08-03 |
| [0331](0331-lazy-extraction-provider-validation.md) | Validate the Extraction Provider on First Use, Not at Startup | 2026-08-03 |
| [0341](0341-build-release-artifacts-once.md) | Build Release Artifacts Once and Share Across the Test and E2E Jobs | 2026-08-04 |
| [0342](0342-salvage-malformed-extraction-items.md) | Per-Item Salvage of Malformed Extraction Items | 2026-08-04 |

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
