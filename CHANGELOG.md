# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 development; see `git log` for history before 0.1.0.

## [0.12.1] - 2026-08-05

A patch release fixing a data-loss bug reported against 0.11.0/0.12.0 (#340): a single malformed
entity or edge in an extraction response could lose an entire chunk, and for a client that treats
a chunk-level error as fatal, an entire multi-chunk document.

### Fixed

- **A single malformed entity or edge failed the whole chunk.** `knowledge_process_chunk` returned
  a hard `-32000` error whenever the extraction LLM emitted one entity or edge missing its `name`
  (or, for an edge, `source_name`/`target_name`), even though every other item in the response was
  well-formed. The malformed item is now dropped and counted; the rest of the chunk's items are
  processed normally, and the chunk succeeds. A community report (#340) traced a ~40-chunk document
  lost in full to exactly this: one field-less item in chunk 13. (#342, [ADR-0342](docs/adr/0342-salvage-malformed-extraction-items.md))

### Added

- **`entities_dropped_malformed` / `edges_dropped_malformed` in `knowledge_process_chunk`'s
  response**, reporting how many entities/edges were dropped in that chunk for failing
  required-field validation during extraction-response parsing. Additive; existing clients are
  unaffected. (#342)
- **A `"salvaged"` `structured_output` telemetry outcome**, distinguishing "parsed successfully but
  one or more items were dropped" from `clean`/`recovered`/`malformed`/`schema_invalid`. Emitted by
  both the Anthropic and OAI-compatible extraction paths — the Anthropic path now emits
  `structured_output` telemetry on a successful call for the first time (previously silent on
  success). (#342)

## [0.12.0] - 2026-08-04

An extraction-quality release. Strict-ontology mode stopped destroying data it couldn't classify,
malformed model output stopped discarding whole chunks, and extraction failures became visible
instead of silent. Alongside that, two regressions reported by downstream consumers are fixed, and
the project gained a documentation site.

### Upgrade notes

- **Ingest output changes again.** Four separate changes alter what extraction produces (#306, #307,
  #310, #312). Re-ingesting a corpus will not reproduce a 0.11.0 graph. As with 0.11.0, nothing
  migrates automatically and existing data is untouched — but new ingest differs from old.
- **Strict-ontology users should re-read the ontology section.** Strict mode previously *deleted*
  entities and edges whose type fell outside the declared vocabulary. It now reclassifies them and
  preserves the original label. If you relied on strict mode as a filter, it is no longer one.
- **`knowledge_reprocess_relation_types` gained a response field.** Additive; existing clients are
  unaffected.
- **Read-only deployments no longer need an extraction provider.** If you were passing a placeholder
  endpoint to satisfy the 0.11.0 startup check, you can drop it.

### Added

- **Documentation site** at **<https://v3rv.com/liminis-context-graph/>** — getting started,
  configuration, IPC/MCP reference, telemetry, ontology, operations, and the ADR index, published
  from `docs/` on every merge. The README is now an overview that links into it rather than a
  950-line reference. Machine-readable `llms.txt` / `llms-full.txt` ship alongside, with a CI check
  that fails if they drift from source. (#295, ADR-0295)
- **`breakdown` in `knowledge_reprocess_relation_types`' apply response**, matching the shape the
  dry-run path already returned. Abstention is the headline behaviour of that method, and after an
  apply the `UNCLASSIFIED` count was previously unrecoverable — 500 confidently-classified relations
  and 500 abstentions both reported `reclassified_count: 500`. (#305, #332)
- **Extraction-failure capture**, recording failures whole rather than as a count, and surfacing
  truncation in the report. (#306)
- **Published extraction-quality evaluation** for 2026-07 — hosted vs local backends and the
  measured effect of an ontology. (#304, `docs/history/extraction-eval-2026-07.md`)

### Fixed

- **Strict ontology mode deleted out-of-vocabulary entities.** An entity whose type wasn't in the
  declared vocabulary was dropped entirely. It is now reclassified to `Unclassified` with the
  original type preserved in the entity's attributes — never deleted. Edges were already being
  preserved by #310; this closes the entity half of the same defect.
  ([ADR-0312](docs/adr/0312-entity-strict-mode-reclassifies-not-drops.md), #312)
- **Strict ontology mode dropped declared aliases and never told the model the constraint.** A
  declared alias like `LAUNCHED_BY` was destroyed rather than normalised to its canonical
  `LAUNCHED`, and the model was never informed of the vocabulary it was expected to honour.
  ([ADR-0310](docs/adr/0310-strict-mode-reclassifies-not-drops.md), #310)
- **A missing `summary` field discarded the whole chunk** and reported it as malformed JSON — so a
  single absent field lost every entity and relationship in that chunk, under a misleading
  diagnosis. (#314)
- **Token-budget policy and edge budget-exhaustion semantics** are now defined rather than
  incidental. ([ADR-0307](docs/adr/0307-token-budget-policy-and-edge-exhaustion-semantics.md), #307)
- **`lcg-service` refused to start without an extraction provider** — a 0.11.0 regression for
  read-only consumers, who never extract. Validation moved from startup to first use, so a reader
  serving `knowledge_find_*` and hydrating via `knowledge_rebuild_from_wal` needs no provider
  configured. Extraction calls without one still fail with the same actionable error.
  ([ADR-0331](docs/adr/0331-lazy-extraction-provider-validation.md), #330, #331)
- **`indices_built` was not set after a runtime recovery**, so `knowledge_status` under-reported
  readiness while the indices genuinely existed. (#297)
- **`knowledge_status` errored instead of reporting degraded state when a core table was missing** —
  the health-check endpoint failing at exactly the moment it is most needed. It now reports a
  `queryable` field distinguishing "graph not queryable" from "graph empty".
  ([ADR-0325](docs/adr/0325-knowledge-status-open-db-not-queryable.md), #325)

### Documentation

- The docs are now the published site above; `CONTRIBUTING.md` gained the ADR-numbering rule
  external contributors need, and `CLAUDE.md`'s long-running-command guidance was corrected. (#315)

### Internal

Not user-facing, but this cycle's development-loop work is why the above could be verified:
`real-corpus-e2e` now runs on every PR rather than only post-merge (#328, ADR-0328), failing
non-gating workflows file a tracking issue automatically (#298), docs-only PRs skip the Rust suite
(#322, ADR-0322), and the required CI check dropped from ~18 minutes to ~8–10 by fixing a Criterion
target-layout footgun (#316, ADR-0316).

## [0.11.0] - 2026-07-30

The first release driven substantially by outside bug reports. Six issues filed by
[@totalslacker](https://github.com/totalslacker) and [@bdueck](https://github.com/bdueck) against 0.9.0/0.10.0 —
[#201](https://github.com/verveguy/liminis-context-graph/issues/201), [#202](https://github.com/verveguy/liminis-context-graph/issues/202),
[#203](https://github.com/verveguy/liminis-context-graph/issues/203), [#204](https://github.com/verveguy/liminis-context-graph/issues/204),
[#205](https://github.com/verveguy/liminis-context-graph/issues/205), [#206](https://github.com/verveguy/liminis-context-graph/issues/206) —
account for most of what follows. Several were silent data-loss bugs that our own fixtures were too small to expose.

### Upgrade notes

- **Ingest output changes.** The entity/edge extraction prompts were rewritten (#281). Re-ingesting a corpus
  produces a different set of entities and edges than 0.10.0 did — more complete on the documents we have measured,
  but *different*, and graphs built before and after this release are not directly comparable. Nothing migrates
  automatically; existing data is untouched, but new ingest will not match old ingest. If you run a user-defined
  ontology, re-measure against your own corpus rather than assuming our figures transfer — they were taken with the
  built-in ontology.
- **`knowledge_backfill_relation_types` is deprecated** in favour of the new `knowledge_reprocess_relation_types`.
  It still exists, but its description now says not to use it. See below.
- **No schema change and no manual migration.** Vector/FTS indexes are now built eagerly at startup, so the first
  launch after upgrading may take slightly longer on a large graph.

### Added

- **Local / OpenAI-compatible extraction** (`OaiExtractor`): `--extractor-uds <path>` and `--extractor-http <url>`,
  plus `LCG_EXTRACTION_URL`, make the "fully local" claim true — extraction no longer requires `ANTHROPIC_API_KEY`.
  Selection is explicit: a reachable local sidecar is never silently preferred over a configured API key.
  ([ADR-0041](docs/adr/0041-local-openai-compatible-extraction-adapter.md), #201, #212)
- **`knowledge_reprocess_relation_types`** — fact-based LLM relation classification, replacing the string-prefix
  heuristic, with honest abstention when a fact maps to no declared type. Brings the MCP tool surface to 34.
  ([ADR-0037](docs/adr/0037-relation-classification-abstention-writes-unclassified.md), #204, #210)
- **`lcg-eval`** — a new workspace crate: an extraction-quality evaluation harness with replay support, ontology
  modes, and blind pairwise LLM-as-judge scoring, so model and prompt changes are measured rather than asserted.
  ([ADR-0048](docs/adr/0048-rust-extraction-quality-eval-harness.md),
  [ADR-0050](docs/adr/0050-blind-pairwise-judging.md),
  [ADR-0049](docs/adr/0049-bare-path-ontology-loader-and-cli-mode-override.md),
  #228, #263, #266, #269, #273, #279)
- **LLM cassette record/replay** (`LCG_RECORD_LLM` / `LCG_REPLAY_LLM`) — record an extraction pass once, replay it
  deterministically at zero API cost. Makes extraction-path tests reproducible and offline.
  ([ADR-0044](docs/adr/0044-llm-cassette-record-replay-seam.md), #232)
- **`--help` / `--version`**, and unknown flags are now rejected instead of ignored. (#198)
- **End-to-end MCP suites** over a golden real-corpus WAL fixture, covering the read, write/mutation, and
  admin/lifecycle paths. (#217, #234, #235, #236)
- **Published extraction-quality evaluation methodology and model rankings.** (#227)

### Fixed

- **Edges were silently dropped whenever an endpoint wasn't in the chunk's own extracted entity list.** This hit two
  ways. An edge referencing a recurring hub entity created by an *earlier* chunk was discarded even though the entity
  existed in the graph. And on long documents the extraction prompts worked against each other — the entity prompt
  forbade abstract concepts while the edge prompt was asked for facts between entities, so the natural hub of a
  document's facts was often never extracted, leaving every edge to it unresolvable.

  Endpoint handling was reworked end to end (#202, #209, #281):

  - The `extract_edges` tool schema now constrains `source_name`/`target_name` to an `enum` of the batch's entity
    names, so on the Anthropic path a compliant model cannot name an off-list endpoint at all. The local /
    OpenAI-compatible path has no tool schema to enforce, so it relies on the steps below.
  - The two prompts were reconciled, so the concepts edges hub on can be extracted as entities.
  - An off-list endpoint that does arrive is **salvaged** against the batch's entities rather than dropped outright.
  - The drop decision moved to the write-lock commit, where resolution falls back to the persisted `Entity` table
    scoped by `group_id` — so a cross-chunk endpoint resolves there. This replaced the earlier pre-lock lookup, which
    duplicated the same query less safely.
  - `knowledge_process_chunk` now reports `edges_dropped_unresolvable`, so remaining loss is visible in the response
    rather than only in a log line.

  ([ADR-0051](docs/adr/0051-edge-endpoint-salvage-and-deferred-drop.md))

  The long-document effect scaled with chunk size. Replaying the 0.10.0 prompts over one reporter's corpus discarded
  0% of extracted edges at ~4.8 KB, 5.6% at ~12.8 KB, and 45 of 46 edges on a single 257 KB article. **Those figures
  are from the default built-in ontology with the default extraction model, over three documents** — they illustrate
  the failure mode, they are not a benchmark. Behaviour under a user-defined ontology (where strict mode filters
  extracted entities by type) is not characterised yet; systematic measurement across ontology modes is in progress
  (#248, #266).
- **`entity_name_embedding_idx` went missing under sustained ingest**, breaking every subsequent query until the
  database was deleted. Indexes were built lazily and the dedup path queried one it never triggered a build for.
  They are now built eagerly after schema init — before the socket accepts requests — and after recovery, with the
  same missing-index auto-heal the search handlers use now extended to the dedup path.
  ([ADR-0036](docs/adr/0036-eager-index-build-at-startup.md), #203, #208)
- **`knowledge_backfill_relation_types` minted garbage pseudo-types** from fact prefixes. (#205, #211)
- **Attached-mode MCP (`--connect`)**: long whole-graph operations no longer false-timeout, and progress is
  reported for `reprocess_entity_types`. ([ADR-0040](docs/adr/0040-attached-mode-reconnect-retry-boundary.md), #206, #213)
- **WAL replay hardening**, across four issues:
  - Rebuild statistics were discarded, so a partial replay reported as clean, and files could replay out of
    sequence. ([ADR-0043](docs/adr/0043-wal-replay-seq-ordering-and-noop-accounting.md), #237)
  - The prepared-statement cache grew unbounded across a large rebuild.
    ([ADR-0045](docs/adr/0045-wal-replay-prepared-statement-cache-scope.md), #238)
  - One malformed template blinded the entire failure report, and `rebuild_from_wal` was not idempotent.
    ([ADR-0046](docs/adr/0046-wal-replay-failure-dedup-and-rebuild-idempotency.md), #239)
  - Replay now has defined transaction boundaries and a defined recovery state, rather than partial writes on
    failure. ([ADR-0047](docs/adr/0047-wal-replay-transaction-boundaries.md), #240)
- **Entity name lookup was a full table scan on every ingest**, now served by an in-process `NameIndex`
  ([ADR-0038](docs/adr/0038-in-process-name-index.md), #219) — with a bounded scan fallback so an index miss can no
  longer be mistaken for "this entity does not exist", which would silently drop an edge
  ([ADR-0283](docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md), #283).
- **Embedder and extractor UDS connections are pooled** instead of dialling, handshaking and tearing down per call.
  ([ADR-0039](docs/adr/0039-uds-embedder-connection-pooling.md), #229;
  [ADR-0042](docs/adr/0042-oai-extractor-uds-connection-pooling.md), #230)
- Service now logs the sender PID of a received `SIGTERM`, so unexplained shutdowns are attributable. (#247)

### Deprecated

- **`knowledge_backfill_relation_types`** — use `knowledge_reprocess_relation_types`. The tool description now says
  so; the method still works. (#211)

### Documentation

- Extraction claims corrected to match behaviour, and `canonicalize_relations` / recovery semantics documented. (#206, #214)
- ADR numbers are now issue numbers, ending a shared sequential counter that collided whenever two issues were in
  flight. (#289)
- Local verification given an explicit 10-minute budget, and the lbug build story corrected — it is a downloaded
  prebuilt bundle, not a source build. (#256)
- **Documentation drift accumulated over this cycle was audited and corrected** (PR #294). Several docs had gone from
  merely incomplete to actively wrong, which matters because this repo is built from source and read by coding
  agents: `docs/telemetry.md` described two events as "not yet emitted" that had been emitting for weeks, and
  documented a `wal_replay_complete` payload whose fields do not exist; the README undercounted the JSON-RPC surface
  and omitted six environment variables the code reads; and the MCP description for `knowledge_process_chunk` never
  mentioned `edges_dropped_unresolvable`, leaving the release's headline new signal invisible to the clients meant to
  act on it. All eleven telemetry events and all twenty-six environment variables are now documented, and
  `CONTRIBUTING.md` gained the ADR-numbering rule external contributors need.

## [0.10.0] - 2026-07-23

### Added

- **Native MCP-over-stdio transport** (`--mcp-stdio`): the binary can now run as a [Model Context Protocol](https://modelcontextprotocol.io) server over stdin/stdout (via `rmcp`), exposing the `knowledge_*` methods as MCP tools to any client — Claude Code, Claude Desktop, other agents — with no Electron/Node dependency. Per-scope tool gating via `--scope` (`read` / `write` / `cypher` / `admin` / `all`); standalone mode opens the database directly, `--connect <sock>` attaches to an already-running service instead. Long operations bridge to MCP progress notifications. See the README's "MCP-over-stdio transport" section and [ADR-0035](docs/adr/0035-mcp-stdio-transport.md). (#195)

### Fixed

- Attached-mode MCP calls (`--connect`) now fail with a clean timeout error instead of blocking forever if the remote service stalls mid-call, and the JSON-RPC response id is validated so a late/stale reply can't be misdelivered to the next call (idle-read timeout `LCG_ATTACHED_CALL_TIMEOUT_MS`, default 30s). (#196)
- MCP `tools/call` validates required arguments at the transport layer, so a call missing a required field returns a clean tool error instead of silently reaching the handler with an empty or default value. (#196)

## [0.9.0] - 2026-07-13

Initial public release: a local-first context graph engine combining property-graph storage, HNSW vector search, and full-text search in a single embedded service over LadybugDB, with a git-friendly JSONL write-ahead log as the source of truth and a 34-method JSON-RPC 2.0 surface over a Unix socket. See the [README](README.md) for the full feature set and architecture.

### Added

- Prebuilt binaries for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu` now published as GitHub Release assets via `cargo-dist`. One-line install: `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-context-graph/releases/latest/download/lcg-service-installer.sh | sh`.

### Changed

- Bump lbug pin from 0.16.1 to 0.17.0 (see PR #127 for delta summary; new `SystemConfig` defaults: `throw_on_wal_replay_failure=true`, `enable_checksums=true`; also removes `LBUG_BUILD_FROM_SOURCE` — 0.17.0 prebuilt is a self-contained fat bundle).
