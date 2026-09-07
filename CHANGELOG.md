# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 development; see `git log` for history before 0.1.0.

## [Unreleased]

lbug 0.18.1 → 0.20.2, superseding the 0.20.1 pin that `73d8c0cc` rolled back after a deterministic
deadlock. **One-way storage migration (42 → 47) — read Upgrading.**

### Upgrading

1. **One-way migration.** A database at storage version 42 (written by 0.18.1, including
   everything v0.14.0 and v0.14.1 shipped) opens directly under the new binary — no export, WAL
   untouched. The first checkpoint rewrites it to storage 47, after which an older binary will
   not open it. Roll back by stopping the service, moving `.lcg/db/` aside, and starting the old
   binary; it rebuilds from the WAL.
2. No API changes and no other manual step.

### Fixed

- The cached-plan fast path could return stale rows from a re-executed parameterized query —
  `query_params`/`exec_params`'s normal calling pattern, not an edge case — fixed upstream by
  `LadybugDB/ladybug#877`/`#878` and shipped in 0.20.2. Research for this change found the
  regression was introduced by upstream's plan-caching optimization landed after 0.19.1 and fixed
  within the same 0.20.x line, so the currently-shipping 0.18.1 does not appear to exhibit it —
  this is a forward-looking correctness fix, not a fix for a bug already present in a release
  that shipped. Covered by new re-execution regression tests exercising the exact pattern.
- Picks back up several fixes the 0.18.1 rollback deferred: `ladybug#845` (FTS heap corruption
  under concurrent scan/write — this service runs `CREATE_FTS_INDEX` and queries it concurrently
  as a live process), `#837` (primary-key-lookup alignment, which issue #221 depends on), `#864`
  (silent row loss in `LOAD FROM`/`UNWIND` feeding a `MATCH` primary-key predicate), `#894`
  (several planner bugs), and `#884` (an `ArrowResultCollector` downcast fix).

### Internal

- The 0.20.1 deadlock that forced the original rollback does not reproduce under 0.20.2, per a
  retest in the same container shape that wedged deterministically before (reported upstream as
  `LadybugDB/ladybug#911`, now closed with the maintainer's acknowledgment).
- `LBUG_EXTENSION_VERSION` moves to `0.20.0` (not `0.20.2`) — the lbug 0.20.2 crate compiles
  against and the CDN publishes under extension-directory version `0.20.0`, a divergence from the
  crate's own semver. Getting this wrong either fails loudly (a 404 during staging) or, if bytes
  were hand-staged under a mismatched directory name, would silently reintroduce the CDN
  dependency #559 removed; the latter is prevented structurally by `stage-lbug-extensions.sh`
  being the sole writer of that directory tree, not by a runtime check.
- `enable_cached_prepared_statement`, new in the 0.20.2 bundle, is confirmed present and settable
  as an operator escape hatch (`CALL enable_cached_prepared_statement='NONE'`) for
  `ladybug#883` — an open, unfixed SIGSEGV in the cached-prepared-statement path that needs
  hundreds of parameterized queries in one session to surface. It is not enabled by default. A
  1,200-iteration single-session regression test found no crash, hang, or stale result, reducing
  but not eliminating this risk.

## [0.14.1] - 2026-09-06

A maintenance release: startup no longer depends on a third-party CDN, and two unbounded-wait
defects on the embedder path are fixed. No migration, no API changes.

### Added

- Bundled lbug vector/fts extensions: the release archive now ships the extension binaries for
  its target platform, so `Db::open` never needs to download them from
  `extension.ladybugdb.com` at startup — a fresh install on an air-gapped or
  egress-restricted host now works with zero prior cache and zero network access. Falls back
  to today's download-on-demand behavior when no bundle is found (e.g. running from source).
  `LCG_LBUG_HOME` lets an operator redirect resolution to a directory of their own. See the
  README's "Offline / air-gapped startup" section and
  [ADR-0559](docs/adr/0559-bundle-lbug-extensions.md) (#559).

### Fixed

- The UDS embedder transport had no timeout. `LCG_EMBEDDING_TIMEOUT_MS` (whole-request) and
  `LCG_EMBEDDING_CONNECT_TIMEOUT_MS` (acquiring a pooled connection) now bound it, with the same
  defaults and validation the HTTP transport already used. This is the unbounded hang #510 fixed
  for HTTP, still present on the **default** transport: a hung or unresponsive sidecar could
  stall an embed call indefinitely. See
  [ADR-0541](docs/adr/0541-uds-embedder-transport-timeouts.md) (#541).
- `state.write_lock` is no longer held across the embedder round trip. `knowledge_assert_entity`
  and `knowledge_assert_relationship` now resolve-and-update under the lock, release it for the
  embed, then re-acquire and re-resolve before inserting — falling back to the update path if a
  concurrent writer won the race. Unrelated writes are no longer serialized behind a slow
  embedder. See [ADR-0543](docs/adr/0543-narrow-write-lock-around-embedder-round-trip.md) (#543).

### Internal

- Closed a TOCTOU port race in `embedder_degraded_mcp.rs`'s test helpers that had caused a flaky
  `AddrInUse` failure, and made a lost race in the delayed-bind stub fail loudly rather than
  masquerading as an embedder-unreachable product failure (#560).
- The release runbook now requires the two sidecar-gated tests to be run and their figures
  recorded before a release — they skip silently when no sidecar is present, so a green board
  never proved they ran (#531, #566).

## [0.14.0] - 2026-09-02

lbug 0.17.0 → 0.18.1, the entity-name lookup moved from process memory to a database-native ART
index, embedder API keys, and structured metadata on episodes. **One-way database migration and one
breaking API change — read Upgrading.** Full detail: [docs/releases/0.14.0.md](docs/releases/0.14.0.md).

### Upgrading

1. **One-way migration.** A 0.13.x database (storage 41) opens directly — no export, WAL untouched.
   The first checkpoint rewrites it to storage 42, after which an older binary will not open it.
   Roll back by stopping the service, moving `.lcg/db/` aside, and starting the old binary; it
   rebuilds from the WAL.
2. **First start blocks on a large graph** while the new `Entity.lookup_key` ART index is built by a
   one-shot scan of every entity.
3. **Run `knowledge_backfill_summary_embeddings`** after upgrading, or summary semantic search
   returns nothing useful — existing entities are zero-filled at migration.
4. **BREAKING: `facts` is now `edges`** on `knowledge_find_relationships` and
   `knowledge_list_relationships`. A client still reading `facts` gets a silently empty list, not an
   error.

### Added

- Structured metadata on episodes: `attributes` on `knowledge_process_chunk`/`knowledge_add_episode`,
  returned by `knowledge_get_episodes`/`knowledge_search_passages` (#528)
- Embedder API-key support, so hosted providers work (#497, #509)
- Semantically searchable entity summaries (#470)
- Per-group ontology drift in `knowledge_status` (#451)
- MCP client configuration recipes for the embedder (#498)
- Docs site published from release tags (#477)

### Changed

- `NameIndex` replaced by a database-native ART index on `Entity.lookup_key` (#221)
- Embedding vectors no longer written to the WAL — they were 89.9% of WAL bytes (#526, #440)
- Embedding calls batched across ingest and WAL replay (#445, #486)
- **BREAKING:** relationship lists returned under `edges`, not `facts` (#524)
- lbug 0.17.0 → 0.18.1, which is what enables the ART index above (`ladybug#582`). 0.19.x/0.20.x
  were evaluated and rejected: 0.20.1 deadlocks under concurrent use (`ladybug#911`) and 0.20.2 ships
  no working extensions (`ladybug#903`). (#398, #529, #556, superseding #190/#220)

### Fixed

- Embedder HTTP request/connect timeouts, so a hung embedder cannot hold the write lock (#510)
- Bounded startup retry, and degraded start for `--mcp-stdio` instead of exiting (#499)
- Assert handlers no longer embed before checking existence (#444)
- Legacy WAL migration files its stream under the right group (#467)
- WAL rebuild reports its own backfill outcome separately (#491)
- Ontology drift clears are no longer overwritten by a stale insert (#495)
- Cross-group episode deletes attributed to the right WAL stream (#402)
- OpenSSL 3 is now required at runtime: `brew install openssl@3` (macOS), `apt install libssl3` (Linux)
- Forward-ported from 0.13.4: #410 and #451 (#494)

### Internal

- Cassette coverage for embedder API-key leaks (#509); Swift fixtures ship `weight.bin` (#518)

## [0.13.4] - 2026-08-24

A patch release on the 0.13 maintenance line, extending drift detection to cover the per-group
ontologies 0.13.3 introduced, and correcting a piece of public API documentation that claimed a
guarantee the code never made.

### Added

- **Ontology drift is now detected per group, not just workspace-wide.** 0.13.3 added per-group
  ontologies (`.lcg/ontology/<encoded_group_id>.yaml`, #446) but deliberately left drift detection
  workspace-scoped — ADR-0446 recorded this as a known v1 limitation, meaning a group governed only
  by its own ontology file got **no drift warning at all** when that file changed across a restart.
  Drift exists to catch one hazard: the ontology changed while the database still holds data
  ingested under the previous vocabulary. Per-group ontologies reintroduced that hazard without
  the corresponding alarm. Each group's ontology is now hashed against its own sidecar record and
  compared on first use, warning and recommending Recreate + re-ingest exactly as the
  workspace-wide check does. (#451)
- **`knowledge_status` reports a `group_ontology_drift` breakdown.** An array of
  `{group_id, drifted, drift_summary}`, one entry per group this process has resolved an ontology
  for. Drift is computed lazily on a group's first use, so a group this process has not touched is
  **absent from the array** rather than reported with `drifted: false` — "not yet computed" is
  deliberately distinguishable from "not drifted", so the field is never a false all-clear. The
  existing workspace-wide ontology and drift fields are unchanged. (#451)

### Fixed

- **Edge `episode_uuids` no longer documented as provenance.** The field's name and its MCP tool
  descriptions implied the episodes listed on a relationship edge were the evidence that produced
  that fact. They are not: `get_episode_info_for_entities` returns every episode mentioning
  **either** endpoint entity, deduplicated, regardless of whether it had anything to do with
  asserting the relationship — ADR-0012's deliberate either-endpoint semantics, accurately
  described in the function's own doc comment but never surfaced to callers. For a well-connected
  entity that can be most of the corpus. Consumers treating the field as evidence were drawing
  conclusions the data does not support.

  The behaviour is unchanged and intentional; the documentation now matches it. Tool descriptions
  for `knowledge_list_relationships` and `knowledge_get_entity_neighbors` state the either-endpoint
  co-occurrence semantics explicitly. Three further read paths —
  `knowledge_find_relationships`, `knowledge_get_edges_by_group` and `knowledge_get_edges_by_uuids`
  — never populate the field at all and now say so, rather than leaving callers to infer meaning
  from an empty array. A registry test asserts these descriptions stay accurate. (#410)

### Internal

- CI's `push` trigger now covers `maint/**`, so maintenance branches get the same post-merge signal
  `main` has. Fabrik's merge train is main-only (it pins `DefaultBaseBranch()`), so release-line
  fixes routed to a maintenance branch via `base:<branch>` are hand-landed and previously produced
  no post-merge run. GitHub runs a workflow from the pushed branch's own tree, so this line has to
  exist on each maintenance branch independently — adding it to `main` alone does not make it fire.
  (#484)

## [0.13.3] - 2026-08-22

A patch release closing the upgrade-path regression 0.13.2 introduced, scoping the last two
ontology-driven maintenance operations to a single group, and adding per-group ontologies. The group
boundary work that began in 0.13.0 is now consistent across reads, writes, deletes and maintenance:
**an operation names the group it acts on, and touches nothing else.**

### Fixed

- **`migrate_wal_root_if_needed` now stamps a generation for the legacy flat WAL it migrates.** A
  legacy flat WAL (`.lcg/wal/*.jsonl`, pre-0.13.0) predates generation identity (#387) entirely,
  and migration now *assumes* it is locally owned (an assumption, not a proof — see #431's
  Assumptions section), minting one for the destination group as part of the same relocation —
  the same act `WalWriter::new` performs for any other stream
  created with no prior content. This closes the gap without weakening #414's refusal for any
  other reason a stream might arrive with no generation (e.g. a publish step that dropped the
  dot-namespace): the new call is scoped strictly to the exact directory this migration just
  relocated content into, never a general sweep over every group. `knowledge_status` now reports
  a non-null `generation` and `generation_status: "known"` for a freshly migrated group. See
  [ADR-0414](docs/adr/0414-wal-generation-unknown-refuses-replay.md)'s amendment note. (#431)
- **`knowledge_rebuild_from_wal`'s unknown-generation refusal message now names both possible
  remedies.** The two situations that produce this refusal — a received stream whose sidecar was
  stripped, and a workspace migrated by a binary older than this fix — are indistinguishable on
  disk, so the message can no longer assume which one applies. It now states both: republish the
  stream's full directory if it came from a publisher, or hand-create `.wal-generation.json` for a
  local workspace with no publisher. (#431)
- **`knowledge_canonicalize_relations` and `knowledge_backfill_relation_types` now require a
  `group_id` and are fully group-scoped.** Both previously selected `RelatesToNode_` candidates
  database-wide with no group filter and flushed every mutation to the default group's WAL
  stream, regardless of which group's data they actually touched. On a multi-group workspace, the
  *correct, intended* use of either operation — canonicalizing the one group a node owns — rewrote
  every co-resident group's edges as a side effect and misattributed the resulting mutations to
  the default group's stream. This is the same failure class as #368/#406 (cross-group mutation)
  and #385 (WAL misattribution), previously left as a documented exception for these two
  operations. `group_id` is now required from the outset — omitted, `null`, or empty is rejected
  rather than falling back to a database-wide rewrite or the default group. No known deployment
  invoked either operation before this fix, so the required parameter breaks no existing caller.
  See ADR-0378's FR-004 section and ADR-0385's Context section, both amended by this issue. (#447)

- **The legacy `.graphiti` → `.lcg` workspace migration now honours `LCG_WAL_DIR` /
  `GRAPHITI_WAL_DIR`.** It always moved `.graphiti/wal` to a hardcoded `.lcg/wal`, so a workspace
  configured with a custom WAL root had its legacy content land somewhere the service would never
  read — the WAL was intact on disk and invisible to the process. The move now targets the resolved
  WAL root, handles a destination on a different filesystem (a custom root often is), and merges into
  a root that already has content rather than failing. (#442)

### Changed

- **BREAKING: `group_id` is now a required, non-empty parameter on
  `knowledge_canonicalize_relations` and `knowledge_backfill_relation_types`, on both the MCP and
  IPC surfaces.** The MCP tool schemas for both methods move `group_id` from optional into
  `required`. A caller that previously omitted `group_id` (absent, `null`, or empty) got a
  successful — and, per the `### Fixed` entry above, cross-group — rewrite; it now receives an
  error naming the missing parameter instead, and no rows are read or written. The remedy is to
  pass the caller's own group explicitly; there is no default to fall back to, because a silent
  default (the default group) is the defect this release fixes. Unlike #406's equivalent change,
  no known deployment invokes either operation today, so this breaks no existing caller in
  practice — see the issue's Release timing discussion. (#447)

### Added

- **Per-group ontology support.** A `group_id` can now have its own extraction/validation
  vocabulary, independent of every other co-resident group in the same workspace. Place a
  group-specific ontology at `{workspace}/.lcg/ontology/<group_id>.yaml` (using the existing
  ontology YAML format); a `group_id` unsafe as a bare filesystem path component is
  percent-encoded with the same bijective scheme already used for per-group WAL directory names.
  A group with no per-group file falls back to the existing workspace-wide `.lcg/ontology.yaml`,
  exactly as before this feature — a workspace that hasn't adopted per-group ontologies behaves
  identically to pre-0.14.0. The resolved per-group ontology governs, for that group only,
  extraction guidance, `mode: strict` validation, canonicalization, and reprocessing
  (`knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`). A malformed or
  unreadable per-group file falls back to the workspace-wide ontology when one exists, or to no
  ontology otherwise (logged either way), rather than failing the group's extraction outright.
  Direct-assert
  (`knowledge_assert_entity`/`knowledge_assert_relationship`) is unaffected, as before. When a
  group's stream is published, the ontology that guided its extraction now travels alongside it
  as `.wal-ontology.json` — a new, purely informational addition to the publish contract (see
  [Operations](docs/operations.md)): a consumer can inspect it as documentation, but it is never
  applied to the consumer's own extraction, validation, canonicalization, or reprocessing, and its
  absence never affects replay or correctness. See [Ontology](docs/ontology.md#per-group-ontologies)
  for the full resolution/fallback contract. (#446)
- **`knowledge_status` reports a new `hydration_status` field**, alongside the existing
  `applied_seq`/`max_seq` values, in both the flat `wal` object and every `wal_groups[*]` entry:
  `"hydrated"` (the database is caught up with its WAL), `"wal_ahead"` (the WAL holds content the
  database has not applied — e.g. a wiped or fresh database beside a populated WAL directory), or
  `"not_applicable"` (the group has no WAL content at all). Previously, a genuinely empty group and
  a group whose WAL was simply ahead of the DB were indistinguishable unless a caller manually
  compared `applied_seq` against `max_seq` itself. Purely additive — `applied_seq`/`max_seq` are
  unchanged, and `health_check`'s `healthy`/`degraded` determination is unaffected. (#456)

### Internal

- **The required CI test gate could not fail.** The `test` job piped `cargo test` through `tee`
  without `pipefail`, so the pipeline reported `tee`'s status and a failing suite still went green.
  (#430)
- **Four tests that had been failing behind that masked gate were fixed** once it started reporting
  honestly — stale fixtures and drifted assertions, not new breakage. (#429)
- **Startup migration ordering is documented and regression-tested.** The `.graphiti` → `.lcg` move
  must precede the per-group WAL relocation, or a `.graphiti`-era workspace leaves its WAL files
  loose and invisible. The ordering was already correct via the call graph — it was diagnosed as
  broken from textual line order — so this hardens an implicit dependency with explicit
  preconditions and un-quarantines the regression test #430 had parked. (#437)
- **Socket-connectability is documented as *not* a readiness signal.** The socket binds before the
  database opens, deliberately, so recovery IPC stays reachable in degraded mode (ADR-0009); the
  correct signal is a `health_check` round-trip reporting `healthy`, which cannot be true until
  after migration. (#436)
- **`wal_root_migration`'s legacy-upgrade acceptance test now runs in CI.** It drives the real
  74MB/12,481-seq fixture through migration and asserts parity with 0.13.1's rebuild, and had run
  only when invoked by hand — while five defects accumulated on the path it guards. (#460)
- **The real-corpus fixture's `group_id` now matches the WAL directory it migrates into.** It was
  captured under a non-default group while committed in the flat pre-0.13.0 layout, so its content
  and directory disagreed — a shape no deployment has, which was nonetheless read as a product
  defect. (#469)

## [0.13.2] - 2026-08-16

A patch release hardening the group boundary in both directions. Deletes could reach across every
group when a caller omitted `group_ids`; reads returned nothing at all in the same situation. Both
are fixed: **an omitted `group_ids` now means all groups on every read tool, and the two episode
deletion tools require an explicit scope instead of defaulting to one.** Ingest is unchanged — a
`knowledge_process_chunk` or `knowledge_add_episode` call that omits a group still writes to the
default group (`"liminis"`), exactly as before. Alongside that, WAL stream generation identity stops
failing open — a stream whose generation cannot be verified is refused rather than replayed
silently — and `knowledge_process_chunk` reports what it dropped instead of only how much.

### Fixed

- **`knowledge_delete_chunk_episode` and `knowledge_delete_by_source` now require an explicit
  group scope.** Both matched `Episodic` rows by `name` or `source_description`/prefix without
  any group predicate whenever `group_ids` was absent, `null`, or `[]`, so the subsequent
  `DETACH DELETE` removed every matching row across every group in the database — reachable on
  every call the liminis app makes, since it never sends `group_ids` on this path
  (`indexing-queue.ts`'s `unlink` handling issues one unscoped delete per chunk on an ordinary
  heading rename). This is the same failure class as #368 (a write in one group destroying
  another group's data), which 0.13.0 treated as release-blocking (see ADR-0368, ADR-0371,
  ADR-0385). Both methods now reject a missing/null/empty `group_ids` with an actionable error
  naming the parameter, and `Conn::remove_episodes_by_chunk_id`/`remove_episodes_by_source` take
  a mandatory (non-`Option`) group scope, so an unscoped, all-groups query is no longer
  representable at the data-access layer, not merely blocked in the handler above it. A valid
  scope that matches nothing still returns a successful `deleted_count: 0`, unchanged. (#406,
  folds in #403)
- **`knowledge_rebuild_from_wal` now refuses to replay a group whose current on-disk generation
  is unknown, once a position has already been recorded for it.** ADR-0387's reset detection
  compares a group's recorded WAL stream generation against what's currently on disk
  (`.wal-generation.json`) to tell a genuine producer-side reset apart from ordinary forward
  progress — but a stream whose generation was never recorded (missing or corrupt sidecar, most
  commonly because a publish step globbed `*.jsonl` and silently dropped the dot-namespace) made
  that comparison permanently inert: every real hydrated stream reported `generation: null`, and
  reset detection never once had a value to compare. The call now fails outright with an
  actionable error naming the group, rather than silently proceeding as an ordinary incremental
  replay with no trace that the safety check couldn't run. Applies uniformly to `dry_run: true`;
  no configuration flag, environment variable, or request parameter bypasses it. Scoped to the
  affected group only — a sibling group sharing the same WAL root with a known generation remains
  independently replayable. A group's first-ever encounter (no position recorded yet) is
  unaffected and still adopts an unknown generation exactly as before. See
  [ADR-0414](docs/adr/0414-wal-generation-unknown-refuses-replay.md). (#414)
- **Every MCP read tool now means the same thing by an omitted `group_ids`: all groups.**
  `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_nodes_by_group` and
  `knowledge_get_edges_by_group` resolved an omitted `group_ids` to the single default group
  (`"liminis"`), while `knowledge_search_passages`, `knowledge_list_entities`,
  `knowledge_list_relationships` and the other read paths resolved it to all groups. On a
  multi-group graph — the arrangement 0.13.0 exists to support — a reader following the documented
  "query them all together with no filter" contract got **zero** entities and relationships back,
  with no error to indicate why, while passage search over the same query returned results
  normally. The four divergent tools now match the majority behaviour, and
  `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group` drop the `"required"` constraint
  their schemas advertised but their handlers never enforced. `knowledge_delete_by_group` is
  deliberately unchanged: `group_ids` stays genuinely required there. Every affected tool's schema
  now states per-tool what an omitted value resolves to, replacing the previous
  "(or the default group, depending on the tool)" hedge. (#413)

### Changed

- **BREAKING: `group_ids` is now a required, non-empty parameter on `knowledge_delete_chunk_episode`
  and `knowledge_delete_by_source`, on both the MCP and IPC surfaces.** The MCP tool schemas for
  both methods move `group_ids` from optional into `required` and add `minItems: 1`. A caller
  that previously omitted `group_ids` (absent, `null`, or `[]`) got a successful — and, per the
  `### Fixed` entry above, potentially catastrophic — delete across every group; it now receives
  an error naming the missing parameter instead, and no rows are deleted. The remedy is to pass
  the caller's own group explicitly; there is no default to fall back to, because a silent
  default (e.g. `DEFAULT_GROUP_ID`) is the defect this release fixes. This is shipped in a patch
  release deliberately: the alternative is leaving active cross-group data loss reachable on
  `main`. (#406)

### Added

- **`knowledge_process_chunk` now warns on oversized `chunk_text`.** Extraction quality degrades
  well before any context-window or output-token limit is reached, so the response gains an
  additive `warning` field — `{"type": "chunk_text_oversized", "chunk_text_chars", "recommended_max_chars", "message"}`
  — whenever `chunk_text`'s character count exceeds an advisory threshold (default 8,000
  characters, configurable via `LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS`; an invalid value falls back to
  the default and logs a warning to stderr). Every successful oversized call also emits a
  `chunk_text_oversized` telemetry event, so oversized ingestion is visible in aggregate across a
  telemetry stream, not just in a single call's response. This is visibility only: nothing is
  truncated, split or rejected, the call still succeeds above the threshold exactly as it did
  before, episode counts are unchanged, and no existing response or tool schema field becomes
  required. The `knowledge_process_chunk` MCP tool description, README, and `docs/configuration.md`
  now document the recommended maximum and that splitting oversized input is the caller's
  responsibility. (#407)
- **`knowledge_process_chunk` now reports per-edge detail behind `edges_dropped_unresolvable`.**
  The response gains an additive `dropped_edges` field: a list with one entry per edge counted in
  `edges_dropped_unresolvable`, in extraction order, each carrying that edge's extracted
  `source_name`, `target_name`, `relation_type`, and `fact`, plus `unresolved_endpoint`
  (`"source"`, `"target"`, or `"both"`) naming which endpoint(s) failed to resolve. Previously the
  count was the only signal a caller had — the specific fact and endpoint that caused a drop were
  unrecoverable once logged. `dropped_edges` is always present, an empty list when nothing was
  dropped; `edges_dropped_unresolvable`'s existing meaning is unchanged. (#411)
- **`knowledge_status` reports a new `generation_status` field**, alongside the existing
  `generation` value, in both the flat `wal` object and every `wal_groups[*]` entry:
  `"not_applicable"` (no WAL stream yet), `"unknown"` (a stream exists but its generation is
  currently unrecoverable), or `"known"`. Previously, "never hydrated" and "hydrated but
  generation unknown" both collapsed indistinguishably to `generation: null`. Purely additive —
  `generation` itself is unchanged. (#414)

### Documentation

- **Clarified what "publishing a WAL stream" means** in `docs/operations.md`: copying a group's
  stream directory means copying the entire directory, dot-namespace included (`cp -R`/`rsync -a`
  with no include-filter, or `git add -A`) — never a `*.jsonl`/`wal/*` glob, which silently drops
  every dotfile while appearing to publish the complete stream. Documents which dot-namespace
  entries are load-bearing (`.wal-generation.json`, MUST travel), a safely-omittable cache
  (`.wal-bounds.json`), or local-only state (`.checkpoints/`). (#414)

### Internal

- **The multi-stream end-to-end test covers four more compositions.** `mcp_multistream_e2e`
  (added in 0.13.1) grew from nine phases to fourteen, adding an upstream stream reset under a new
  generation (#387), a cross-group merge asserting on disk that a merge in one group writes only to
  its own stream (ADR-0371), ambiguous endpoint resolution — a `binding_state` the suite had never
  once produced — and a stream arriving with no `.wal-generation.json` at all, which is the
  condition that reached a real deployment undetected and motivated #414. The merge phase runs
  before the purge/restore sequence so cross-group pointers start `bound` by construction rather
  than by accident. Still in the default `cargo test --release` pass, no network, corpus, or LLM.
  (#400)

## [0.13.1] - 2026-08-15

A patch release closing an integrity gap in 0.13.0's layered-graph model: a cross-group pointer
left broken by an earlier operation could not be repaired through the public API.

### Fixed

- **`knowledge_rebind_pointers` now repairs pointers it had recorded as broken.** Its staleness
  gate compared only WAL positions, so a pointer whose recorded `binding_state` was `unbound` or
  `ambiguous` was skipped whenever the source group's applied position happened to look unchanged
  — the tool declined to repair a fact it had written itself, and reported `checked: 0` as though
  there were nothing to do. This is reachable through a documented flow: purge a group (#361), then
  restore it to a checkpoint-bounded position (#365) matching where the pointers into it were
  originally bound. Every layer graph pointing into that group stayed permanently broken with no
  public repair. The gate now skips a pointer only when it is currently `bound` *and* its position
  looks fresh; `unbound`/`ambiguous` pointers are always re-resolved. The position-based
  optimisation is unchanged for pointers that are already correct, so a repeat call with no
  intervening change remains a no-op. (#392,
  [ADR-0392](docs/adr/0392-rebind-pointers-staleness-gate-binding-state.md))

### Changed

- **`knowledge_rebind_pointers` responses gain a `staleness_skipped` count**, distinguishing
  "examined and already correct" from "not examined because it looked fresh". A `checked: 0`
  result is no longer ambiguous about whether anything was actually looked at. Additive only —
  no existing field changes meaning. (#392)

### Added

- **An end-to-end multi-stream integration test** (`crates/service/tests/mcp_multistream_e2e.rs`),
  running in the default suite in a few seconds with no LLM, corpus or network. It drives a live
  MCP process across three groups — two sources and a layer graph — and asserts on the on-disk WAL
  layout as well as API responses, spanning per-group streams, positions, mutation attribution,
  checkpoints, group purge, cross-group pointers and rebind in one composition. It found the defect
  above, along with #383 and #385 during 0.13.0. (#394)

## [0.13.0] - 2026-08-13

A minor release making the graph genuinely multi-tenant. Groups become a real isolation boundary
rather than a filter applied by convention: each group gets its own WAL stream, merges can no
longer reach across groups, and cross-group references are expressed as resolvable pointers
instead of edges that silently break. Alongside that, the WAL gains the primitives needed to
treat it as a backup you can restore *from* — a replay upper bound, named recovery positions, and
a generation identity that lets a consumer tell an advance from a reset.

### Added

- **One WAL directory per group.** A group's mutations are written to its own WAL stream rather
  than a single shared log, so a consumer can hydrate, replay, or discard one group without
  touching another. This is the structural change the rest of the group work depends on.
  (#378, [ADR-0378](docs/adr/0378-multi-stream-wal-per-group-directory.md))
- **Generation identity on WAL streams.** A stream now carries an identity that distinguishes a
  forward advance from a reset, so a consumer can no longer mistake a rebuilt stream for an
  extension of the one it already replayed — previously indistinguishable, and silently corrupting
  for an incremental consumer. (#387, [ADR-0387](docs/adr/0387-wal-stream-generation-identity.md))
- **`knowledge_delete_by_group`** — a group-scoped purge that removes entities and edges, not just
  episodes. The pre-existing episode-deletion paths only `DETACH DELETE` the `Episodic` node,
  leaving the entity and edge data they created behind, so a graph could report zero episodes while
  still holding real content. (#361, [ADR-0361](docs/adr/0361-group-scoped-purge.md))
- **`knowledge_wal_mark_create` / `_list` / `_delete`** — named, retained WAL positions ("this
  graph was known-good here"), stored in a `.checkpoints/` subdirectory of the WAL directory
  rather than in the database, so they survive exactly the database loss they exist to recover
  from. `_list` reports whether each position is still reachable given the WAL content on disk.
  (#365, [ADR-0365](docs/adr/0365-wal-checkpoints-directory-per-name-store.md))
- **A `to_seq` upper bound on `knowledge_rebuild_from_wal`.** Replay previously accepted only a
  lower bound and always ran to the end of the WAL, so a bad mutation — itself recorded in the WAL
  — was replayed back on every restore. A bounded rebuild can now stop before it. Note this is not
  durable: entries beyond `to_seq` remain on disk unapplied, and a later unbounded rebuild will
  reapply them. (#362)
- **`knowledge_add_cross_group_edge` and `knowledge_rebind_pointers`** — cross-group references
  expressed as resolvable semantic pointers rather than raw edges. This supports a hub topology:
  N independently-hydrated source groups living in one database, with a separate *layer graph*
  carrying its own `group_id` whose edges connect entities across two source groups. A pointer
  survives its target group being re-ingested or re-canonicalised, where a raw edge would dangle.
  (#369, [ADR-0369](docs/adr/0369-resolvable-cross-group-pointers.md))
- **`knowledge_assert_entity` and `knowledge_assert_relationship`** — a direct assertion API for
  writing graph content without going through episode extraction.
  (#379, [ADR-0379](docs/adr/0379-direct-assertion-conventions.md))
- **A cached WAL seq-bounds manifest**, so establishing a stream's minimum and maximum sequence no
  longer requires scanning every file in the WAL directory — the operation sits behind reachability
  checks and status calls at deployments with tens of thousands of WAL files.
  (#375, [ADR-0375](docs/adr/0375-wal-max-seq-bounds-manifest.md))

### Fixed

- **Entity merge silently destroyed cross-group edges.** `has_directed_edge` and
  `get_full_edges_for_entity` were not group-scoped, so a merge in one group could consider — and
  destroy — edge data belonging to another. (#368,
  [ADR-0368](docs/adr/0368-group-scoped-edge-dedup-in-merge.md))
- **Merge could write another group's data.** Foreign-group edges encountered during a merge are
  now skipped and left for their owning group to re-bind, rather than being rewritten by a merge
  that has no authority over them. (#371,
  [ADR-0371](docs/adr/0371-merge-never-writes-foreign-group-data.md))
- **`delete_by_group` and `rebind_pointers` wrote other groups' mutations to the default WAL
  stream**, breaking per-group stream isolation for exactly the operations most likely to span
  groups. Mutations are now attributed to the group they belong to. (#385,
  [ADR-0385](docs/adr/0385-per-group-mutation-attribution-for-multi-group-writers.md))
- **`applied_seq` never advanced for `wal_flush_ungrouped` writes** — which is every write path
  except episode ingest. Harmless while episode ingest was the only writer, but with the assertion
  API (#379) and per-group positions (#378) both landed, it left the re-bind trigger inert for
  graphs built by any other path. (#383)
- **Semantically-empty required fields were persisted.** #342 (0.12.1) made extraction-response
  parsing tolerant of items that fail to *deserialize*, but an empty or whitespace-only string
  deserializes fine — so an edge whose `fact` was blank had no validation at any layer and could
  reach storage. Blank required fields are now rejected during item salvage, at parse time, across
  both the Anthropic and OAI-compatible paths, and counted in the existing drop counters. The eval
  path, which consumes extractor output directly, now sees the same filtered item set the ingest
  path would store. (#347,
  [ADR-0347](docs/adr/0347-reject-semantically-empty-required-fields-during-salvage.md))

## [0.12.2] - 2026-08-06

A patch release implementing a community-requested feature (#351): a reliable, cheap boot-time
check for whether a local DB is consistent with the WAL. Shipped as a patch alongside the
prerequisite `global_seq` re-derivation fix (#352) because the requesting deployment needs both
together.

### Added

- **`wal.applied_seq` and `wal.max_seq` in `knowledge_status`**, letting a client decide — from
  one call and an integer comparison — whether its DB already reflects the WAL, needs an
  incremental resume, or needs a full rebuild. `applied_seq` is a new piece of persisted DB
  state (survives restart, not cached in memory) recording the highest WAL `seq` whose
  mutations are committed in the current graph; `max_seq` is the highest `seq` actually present
  across the WAL files, read fresh on every call so an externally-updated WAL is observed
  immediately. `applied_seq` is `null` (unknown), `0` (nothing applied), or a positive integer
  (a known position) — see `docs/operations.md` for the full consumer decision table and a
  cross-language footgun around `null` in numeric comparisons. A pre-existing, populated DB that
  predates this feature backfills a usable position on first open via
  [ADR-0026](docs/adr/0026-episode-cursor-wal-resume.md)'s retroactive episode-cursor mechanism,
  rather than reporting `null` — except when that mechanism itself can't find the last episode's
  uuid in the WAL, the one case where `null` remains the correct, documented report.
  (#353, [ADR-0353](docs/adr/0353-persist-and-expose-applied-wal-seq.md))

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
