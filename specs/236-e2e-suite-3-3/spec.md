# Feature Specification: MCP Admin/Lifecycle E2E Suite Over the Real-Corpus Fixture

**Feature Branch**: `fabrik/issue-236`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "The admin and lifecycle surface — WAL dump, checkpoint, rebuild, recovery, index build, shutdown — is the engine's data-safety machinery, and it is exercised today only on small synthetic graphs (handlers_wal_admin.rs, auto_recovery.rs, degraded_startup.rs, clean_shutdown.rs). None of it is verified at realistic scale, and none of it through the MCP surface. That's the wrong risk profile: these are precisely the operations a user reaches for when something has already gone wrong, and the ones whose failure loses data. The community field report (#207) described a recovery attempt that couldn't be completed because the relevant tools weren't discoverable — and a relation_type reset that turned out to be unrecoverable by any built-in path. Bugs here are maximally expensive. Extend the MCP e2e suite with admin/lifecycle coverage at real scale: full dump → rebuild → verify round-trips on the 1,506-entity fixture, recovery from a degraded database, index rebuild behaviour, and shutdown semantics — all driven through tools/call with --scope=admin."

## Background

The admin/lifecycle tool surface (`knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_build_indices`, `knowledge_close`) is the engine's data-safety machinery — the tools an operator reaches for specifically *after* something has already gone wrong (a corrupted database, a lost index, a bad shutdown) or as the safety net taken *before* a risky operation (a snapshot before a destructive mutation). Today this surface is exercised only by small synthetic-graph tests (`crates/core/tests/handlers_wal_admin.rs`, `auto_recovery.rs`, `degraded_startup.rs`, `cancel_shutdown.rs`), calling handlers directly in-process — never through the MCP transport (`tools/call`) that real MCP clients actually use, and never at a scale anywhere near where these operations are expensive or slow enough for bugs to hide (rebuild time, checkpoint timing, index-build duration).

That is exactly the wrong risk profile. The community field report (#207) described a real recovery attempt that could not be completed because the relevant tools were not discoverable through the client the user actually had — and a `relation_type` reset that turned out to be unrecoverable by any built-in path, because nothing had proven the recovery/dump path actually worked end to end at scale. A bug in this surface does not just fail a request; it can be the difference between a graph that heals from corruption and one whose data is gone for good, and testing it only against a two-entity synthetic graph gives no confidence it behaves the same way against real data at the scale where recovery is actually needed.

This is the third and final issue in a three-part MCP e2e series over the same golden real-corpus fixture (#217: 1,506 entities / 2,392 relationships / 228 episodes, captured from a real `AnthropicExtractor` + real embedder run, replayable with zero LLM/embedder calls). #234 (read-path) built and documented the reusable fixture-seeding harness (`crates/service/tests/common/real_corpus.rs`: `seed_real_corpus_workspace` / `SeededWorkspace`) that this issue reuses; #235 (write/mutation-path) covers the mutation tool surface. This issue closes the series by covering the admin/lifecycle surface: dump/rebuild round-trips, degraded-mode recovery, index rebuild, checkpoint, and clean shutdown — all driven through real `tools/call` against the real compiled binary, at the fixture's full scale, with `--scope=admin` active.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The dump/export path is proven lossless at scale (Priority: P1)

As a maintainer, when I change `knowledge_dump_wal` or the WAL-replay path it depends on, I want a suite that dumps the seeded real-corpus graph to a fresh compacted WAL through `tools/call`, rebuilds a brand-new database from *that* dump, and asserts the rebuilt graph is equivalent to the original (counts, and a sample of entities/edges/episodes), so that the snapshot/export path users are told to rely on before destructive operations is proven trustworthy at real scale, not just assumed.

**Why this priority**: `knowledge_dump_wal` is documented as the safety net an operator takes *before* a risky operation. If dump→rebuild silently drops or corrupts data, every downstream recovery workflow that depends on "take a snapshot first" is unsound — and #207's unrecoverable `relation_type` reset is exactly the kind of incident this path is supposed to prevent from being catastrophic.

**Independent Test**: Runs against a seeded real-corpus workspace via `tools/call`; the rebuilt-from-dump database is a brand-new, separate workspace, so this test does not depend on or interfere with any other test's graph state.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace (via #234's `SeededWorkspace`), **When** `knowledge_dump_wal` is called through `tools/call` with no `group_id` (dump all groups), **Then** it produces a fresh, compacted WAL directory distinct from the workspace's live application WAL.
2. **Given** that dump output, **When** it is replayed via `knowledge_rebuild_from_wal` into a brand-new, empty database, **Then** the rebuilt graph's entity/relationship/episode counts exactly match the original seeded workspace's counts (1,506 / 2,392 / 228), and a sample of entities, edges, and episodes drawn from the original are present with matching content in the rebuilt graph.
3. **Given** the same dump, **When** `knowledge_dump_wal` is instead called with a specific real `group_id` present in the fixture, **Then** the dump and its rebuild contain only that group's entities/edges/episodes, not the full fixture.

---

### User Story 2 - Rebuilding through MCP restores full functionality with no extra step (Priority: P1)

As a maintainer, when I change `knowledge_rebuild_from_wal` or the index-build path it triggers, I want a suite that fully rebuilds the real 1,506-entity fixture through `tools/call` and asserts that `indices_built` is `true` and that search works immediately afterward — with no separate `knowledge_build_indices` call — so that the documented "rebuild leaves you with a fully working graph" contract is proven true at real scale through the layer clients actually use.

**Why this priority**: This is the core lifecycle operation a user runs to recover from almost any WAL-visible problem. If rebuild silently leaves indices stale or unbuilt, every read-path tool a user tries immediately afterward looks broken, and the user has no way to know a rebuild "succeeded" in name only.

**Independent Test**: Runs a rebuild of a freshly seeded (or dump-rebuilt, from User Story 1) real-corpus workspace and immediately issues a search call, without any intervening `knowledge_build_indices` call.

**Acceptance Scenarios**:

1. **Given** a real-corpus WAL replayed via `tools/call knowledge_rebuild_from_wal` at full fixture scale, **When** the rebuild completes, **Then** the response (or a subsequent `knowledge_status` call) reports `indices_built: true`.
2. **Given** the same rebuilt graph, **When** a golden `knowledge_find_entities` query (from the fixture's `expected_results.json`, per #217/#234's precedent) is issued immediately afterward with no intervening `knowledge_build_indices` call, **Then** it returns the expected entities.

---

### User Story 3 - Recovery from induced database corruption restores a working, complete graph (Priority: P1)

As a maintainer, when I change the degraded-mode startup path, `knowledge_recover`, or `knowledge_recover_full`, I want a suite that corrupts or removes the lbug database beneath a real WAL in an isolated temp workspace, confirms the service still starts and serves the ADR-0009 degraded-mode allow-list, and then confirms that recovery restores a fully working graph with the fixture's contents, so that the #207 field-reported gap — a recovery attempt that couldn't be completed because the right tool wasn't reachable, and data loss when it wasn't — is closed for the operation that exists specifically to handle this exact situation.

**Why this priority**: This is the single highest-consequence scenario in the admin surface: it is both the direct regression guard for #207 and the reason the rest of this issue exists at all ("the ones whose failure loses data"). A recovery path that only works on a two-entity synthetic graph provides no assurance it works at the scale, and against the on-disk shape, where real corruption is actually encountered.

**Independent Test**: Runs entirely against a workspace directory created fresh for this test (never a developer's real `.lcg/`), with the database file deliberately corrupted or removed beneath an intact application WAL before the server process is spawned.

**Acceptance Scenarios**:

1. **Given** a temp workspace seeded from the real-corpus fixture and rebuilt, **When** the lbug database file underneath it is corrupted (or removed) and the real binary is spawned with `--mcp-stdio --scope=admin` against that workspace, **Then** the process starts successfully (no crash, no crash-loop), and `tools/call` for `knowledge_status` and `health_check` succeed while a non-allow-listed method is rejected — confirming the ADR-0009 degraded-mode allow-list is being served, not merely that the process didn't exit.
2. **Given** that degraded server, **When** `knowledge_recover` is called through `tools/call` with a strategy applicable to the induced failure mode, **Then** the call succeeds and the graph is restored to a healthy, non-degraded state.
3. **Given** the recovered graph, **When** its entity/relationship/episode counts and a sample of golden query results are checked, **Then** they match the original fixture's expected values — recovery didn't just clear the degraded flag, it actually restored the fixture's content.
4. **Given** a second, independently-corrupted copy of the seeded workspace, **When** `knowledge_recover_full` (the full autonomous recovery sequence) is called instead of a single named strategy, **Then** it likewise restores a healthy graph matching the fixture's expected counts, and calling it again afterward is a confirmed no-op (idempotent, per its documented contract).

---

### User Story 4 - Progress notifications are observed for a real streaming admin operation (Priority: P2)

As a maintainer, when I change the progress-notification plumbing for streaming admin methods, I want a suite that supplies a progress token to `knowledge_rebuild_from_wal` while rebuilding the full real-corpus fixture and asserts that MCP progress notifications actually arrive before the terminal result, on a graph large enough for intermediate progress to be meaningful, so that a regression that silently drops progress notifications isn't only caught on a fixture too small to produce more than zero or one of them.

**Why this priority**: Named directly in the issue as a distinct acceptance criterion ("assert progress notifications arrive when a progress token is supplied, on a graph large enough for it to matter"). It's a P2 relative to the recovery/dump/rebuild-correctness stories because it verifies an observability property, not data safety, but it still needs the real fixture's scale to be a meaningful test at all.

**Independent Test**: Issues `knowledge_rebuild_from_wal` with a `_progress_token` against the real fixture and collects all JSON-RPC notification frames received on the transport before the terminal response, independent of the other admin stories' assertions.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace, **When** `knowledge_rebuild_from_wal` is called through `tools/call` with a `_progress_token` supplied, **Then** one or more `{"type":"progress",...}` notification frames are observed on the transport before the terminal result arrives.
2. **Given** the same call without a `_progress_token` supplied, **When** the rebuild runs, **Then** no progress notification frames are emitted — confirming the token, not the operation itself, gates notification emission.

---

### User Story 5 - Checkpoint leaves a coherent, still-rebuildable database (Priority: P2)

As a maintainer, when I change `knowledge_prepare_checkpoint`, I want a suite that checkpoints the real fixture graph through `tools/call` and then confirms the database is still coherent (serves reads correctly) and still rebuildable from its WAL afterward, so that a regression in the checkpoint path is caught before it corrupts the on-disk state operators rely on being safe to back up.

**Why this priority**: Named directly in the issue's requirements. It's P2 because, unlike recovery, a checkpoint bug is not itself data-destructive by design intent — but it still guards a real correctness property (post-checkpoint coherence) that this suite is uniquely positioned to check at scale.

**Independent Test**: Runs a checkpoint against a real-corpus workspace and then performs both a read assertion and a from-scratch rebuild assertion against the same workspace, independent of the other admin stories.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace, **When** `knowledge_prepare_checkpoint` is called through `tools/call`, **Then** the call succeeds and a subsequent `knowledge_status` / golden read query against the same workspace returns correct results (the database was not left in an inconsistent state by the checkpoint).
2. **Given** the checkpointed workspace, **When** `knowledge_rebuild_from_wal` is subsequently run against it, **Then** the rebuild completes successfully and reproduces the fixture's expected counts — the checkpoint didn't leave the WAL in a state the replayer can't handle.

---

### User Story 6 - Index rebuild restores search on an index-less graph (Priority: P2)

As a maintainer, when I change `knowledge_build_indices`, I want a suite that starts from a real-scale graph with its indices absent, calls `knowledge_build_indices` through `tools/call`, and asserts that search is restored afterward with `indices_built` accurately reflecting reality at every point, so that the #203 class of failure (index loss under sustained write, silently stale `indices_built` reporting) is guarded at the scale where it was originally field-reported, not just against a small synthetic graph.

**Why this priority**: Directly named in the issue as guarding "the #203 class of failure." It's P2 rather than P1 because it's a narrower, single-tool correctness check compared to the broader lossless-round-trip and recovery stories, but the #203 precedent means it must be checked at real scale, not assumed safe by analogy to a small fixture.

**Independent Test**: Runs against a real-corpus workspace whose indices have been made absent (e.g. by a targeted index-only reset distinct from full corruption), verifying search fails or is degraded beforehand and is restored afterward.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace with its search indices made absent, **When** `knowledge_status` is checked, **Then** `indices_built` reports `false` (or another accurate not-built indicator) — not stale-`true` while search is actually broken.
2. **Given** that same workspace, **When** `knowledge_build_indices` is called through `tools/call`, **Then** it completes successfully, `indices_built` becomes `true`, and a golden search query immediately afterward returns the expected entities.

---

### User Story 7 - Clean shutdown checkpoints the WAL, and remote-close gating behaves as documented (Priority: P2)

As a maintainer, when I change `knowledge_close` or its interaction with `--allow-remote-close`, I want a suite that shuts down the server cleanly at real fixture scale and confirms a WAL checkpoint was written (per ADR-0017), and that in attached mode the `--allow-remote-close` flag gates whether the shutdown is forwarded to the remote service exactly as documented, so that a regression here doesn't silently reintroduce the WAL-corruption-on-exit race ADR-0017 fixed, or accidentally let an unauthorized client shut down a shared remote service.

**Why this priority**: Named directly in the issue's requirements, tied to a specific documented ADR (ADR-0017) whose regression this suite is positioned to catch at real scale. P2 because clean shutdown, while important, is a narrower correctness check than the recovery/dump-lossless stories that guard against actual data loss.

**Independent Test**: Runs a standalone `--mcp-stdio` server against a seeded real-corpus workspace, calls `knowledge_close`, and inspects the resulting on-disk WAL state; separately spawns an attached-mode server (`--connect`) with and without `--allow-remote-close` to confirm the forwarding gate.

**Acceptance Scenarios**:

1. **Given** a standalone MCP server backing a seeded real-corpus workspace, **When** `knowledge_close` is called through `tools/call`, **Then** the server shuts down cleanly (no error, no crash) and the on-disk WAL reflects a checkpoint having been written (per ADR-0017's guarantee that the checkpoint fires deterministically before process exit).
2. **Given** an attached-mode server (`--connect` to a real running remote service) spawned *without* `--allow-remote-close`, **When** `knowledge_close` is called through `tools/call`, **Then** the call does not shut down the remote service (documented no-forwarding behavior).
3. **Given** the same setup but spawned *with* `--allow-remote-close`, **When** `knowledge_close` is called, **Then** the shutdown is forwarded and the remote service closes.

---

### User Story 8 - Admin tools are invisible and rejected without admin scope (Priority: P1)

As a maintainer, I want every admin/lifecycle tool covered by this suite to be confirmed absent from `tools/list` and rejected if called anyway when the server is started without `--scope=admin` (or `all`), so that a scope-gating regression that accidentally exposes a destructive admin tool to a non-admin client is caught against real data, mirroring the read-path (#234) and write-path (#235) suites' existing scope-gating coverage.

**Why this priority**: Named as a standalone cross-cutting requirement in the issue, and this is the highest-consequence scope-gating gap in the whole tool registry — these are precisely the tools capable of destroying or corrupting a graph, so a gating bug here is more severe than the equivalent gap on the read or write surface.

**Independent Test**: Runs against a server spawned with a non-admin scope (e.g. `--scope=read` or `--scope=write`) backing a seeded (not necessarily mutated) real-corpus workspace; does not depend on any other story's mutation or recovery having run.

**Acceptance Scenarios**:

1. **Given** a seeded real-corpus workspace, **When** the server is spawned without `admin` in its active scope set and `tools/list` is called, **Then** none of `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_build_indices`, or `knowledge_close` appear in the list.
2. **Given** the same server, **When** any one of those tools is called anyway through `tools/call`, **Then** the call is rejected as an unlisted tool (protocol-level error, consistent with #234's and #235's existing scope-rejection precedent), and the underlying graph is verifiably unchanged.

---

### Edge Cases

- **Destructive-test isolation**: several stories in this suite (induced corruption, dump/rebuild into a new database, index removal, `knowledge_close`) mutate or destroy the workspace they run against. Each such test MUST run against its own isolated copy of the seeded fixture, never a shared workspace another test still depends on — the same isolation concern #235 raised for its mutation suite, sharpened here because some of these operations (induced corruption) are irreversible by design.
- **No test may ever point at a developer's real workspace**: induced database corruption/removal MUST be performed only inside a temp directory created by the test harness for that purpose. This is called out explicitly in the originating issue's Risks section and must be structurally guaranteed (e.g., the corruption helper only ever accepts a path inside a `tempdir()`-created directory), not merely a documented convention.
- **Runtime cost**: full dump/rebuild cycles over the fixture's ~71 MB WAL are slow (on the order of a minute or more per cycle, per #217/#234's measured figures), and this suite needs multiple such cycles (dump round-trip, plain rebuild, two recovery paths, checkpoint+rebuild). The suite's total runtime must be budgeted deliberately and kept off the per-PR CI path, mirroring #217/#234/#235's gating precedent.
- **Distinguishing "index-less" from "corrupted"**: User Story 6 needs a graph with indices specifically absent while the rest of the database is otherwise healthy — a different induced condition from User Story 3's full database corruption. The exact mechanism for producing an index-less-but-otherwise-healthy graph (e.g., a dedicated reset path, or deleting only the index files) is a Research/Plan-stage determination.
- **Attached-mode spawning is new to this suite**: unlike #234 and #235 (standalone `--mcp-stdio` only), User Story 7's `--allow-remote-close` scenarios require spawning a server in attached mode (`--connect` to a separately-running remote service), which existing precedent for this in `crates/service/tests/mcp_attached.rs` (#213) should inform but which this suite must set up specifically for the real-corpus fixture.
- **Recovery strategy selection**: `knowledge_recover` requires a `strategy` argument (`drop_lbug_wal`, `rebuild_from_workspace_wal`, or `restore_from_backup`); which strategy is applicable depends on how corruption was induced. The suite must induce a failure mode compatible with at least one concrete strategy it asserts against, and separately exercise `knowledge_recover_full`'s strategy-selection-free path (User Story 3, Scenario 4).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The suite MUST reuse the seeding harness built by #234 (`crates/service/tests/common/real_corpus.rs`: `seed_real_corpus_workspace` / `SeededWorkspace`) unmodified, per that issue's documented reusability guarantee.
- **FR-002**: The suite MUST assert, for `knowledge_dump_wal`, that dumping the seeded fixture to a fresh compacted WAL and rebuilding a brand-new database from that dump reproduces the original's entity/relationship/episode counts exactly, plus content-matching for a sample of entities, edges, and episodes (User Story 1, Scenarios 1–2).
- **FR-003**: The suite MUST assert that `knowledge_dump_wal` called with a specific real `group_id` produces a dump scoped to only that group's content (User Story 1, Scenario 3).
- **FR-004**: The suite MUST assert, for a full `knowledge_rebuild_from_wal` of the real fixture through `tools/call`, that `indices_built: true` is reported and that a golden search query succeeds immediately afterward with no intervening `knowledge_build_indices` call (User Story 2).
- **FR-005**: The suite MUST assert that supplying a `_progress_token` to `knowledge_rebuild_from_wal` against the full real fixture produces one or more observed progress notification frames before the terminal result, and that omitting the token produces none (User Story 4).
- **FR-006**: The suite MUST assert, for `knowledge_prepare_checkpoint` run against the real fixture, that the database remains coherent for reads afterward and remains successfully rebuildable via `knowledge_rebuild_from_wal` afterward (User Story 5).
- **FR-007**: The suite MUST induce database corruption or removal beneath a real, intact application WAL inside an isolated temp workspace, spawn the real binary with `--scope=admin` against it, and assert both that the process starts successfully and that only the ADR-0009 degraded-mode allow-listed methods (`health_check`, `knowledge_status`, `knowledge_recover`, `knowledge_close`) succeed while a non-allow-listed method is rejected (User Story 3, Scenario 1).
- **FR-008**: The suite MUST assert that `knowledge_recover`, called with a strategy applicable to the induced failure mode, restores a healthy (non-degraded) graph whose entity/relationship/episode counts and a sample of golden query results match the original fixture's expected values (User Story 3, Scenarios 2–3).
- **FR-009**: The suite MUST separately assert that `knowledge_recover_full`, run against an independently-induced degraded copy of the fixture, likewise restores the fixture's expected counts, and that calling it again afterward on the now-healthy graph is a confirmed no-op (User Story 3, Scenario 4).
- **FR-010**: The suite MUST assert, for `knowledge_build_indices` run against a real-corpus workspace with indices specifically made absent (while otherwise healthy), that `indices_built` accurately reports `false` beforehand and `true` afterward, and that a golden search query fails or is degraded beforehand and succeeds afterward (User Story 6).
- **FR-011**: The suite MUST assert, for `knowledge_close` called against a standalone server backing the real fixture, that shutdown completes cleanly and the on-disk WAL reflects a checkpoint having been written per ADR-0017 (User Story 7, Scenario 1).
- **FR-012**: The suite MUST assert, for `knowledge_close` called against an attached-mode server, that the remote service is *not* shut down when spawned without `--allow-remote-close`, and *is* shut down when spawned with it (User Story 7, Scenarios 2–3).
- **FR-013**: The suite MUST assert, against a server spawned without `admin` in its active scope, that `tools/list` excludes all seven admin tools (`knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_build_indices`, `knowledge_close`) and that calling any of them anyway is rejected as an unlisted tool, with the underlying graph verifiably unchanged (User Story 8).
- **FR-014**: Every test that induces database corruption, removal, or destructive mutation MUST run against an isolated temp-directory copy of the seeded fixture created for that test alone, never a shared workspace another test in the same run still depends on, and MUST be structurally incapable of targeting a real (non-temp) `.lcg/` workspace.
- **FR-015**: The suite MUST make zero outbound live-LLM or real-embedder network calls end to end (no `ANTHROPIC_API_KEY`, no network access beyond the suite's own loopback stub embedder), verifiable independently, mirroring #217/#234/#235's explicit call-counting discipline.
- **FR-016**: The suite MUST be gated the same way as #234's and #235's real-corpus suites — excluded from the default per-PR `cargo test --release` gate, with an automatic trigger on push to `main` (and/or `workflow_dispatch`), given its measured runtime cost (multiple full dump/rebuild/recovery cycles over the fixture's WAL).
- **FR-017**: `cargo fmt --all` and the project's clippy gate (`cargo clippy --all-targets -- -D warnings` / `cargo clippy --release -- -D warnings`) MUST remain green with the new suite added.

### Key Entities

- **Seeded MCP workspace**: The real-corpus workspace produced by #234's `SeededWorkspace` / `seed_real_corpus_workspace`, reused unmodified as the starting state for every admin/lifecycle assertion in this suite.
- **Isolated degraded/destructive-test workspace**: A per-test, temp-directory-only copy of the seeded workspace used by tests that induce corruption, dump/rebuild into a new database, remove indices, or shut the server down — never a workspace another test still depends on, and never a real developer workspace.
- **Dump-WAL snapshot**: The fresh, compacted WAL directory produced by `knowledge_dump_wal`, whose rebuild is asserted to be equivalent to the workspace it was dumped from.
- **Admin/lifecycle assertion**: A before/after graph-state or process-behavior assertion (counts, degraded-mode allow-list membership, progress notification presence, checkpoint coherence, scope-rejection) checked through `tools/call` against the real fixture, rather than through direct in-process dispatch or a small synthetic graph.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `knowledge_dump_wal` → rebuild-from-dump → equivalence assertion passes at full fixture scale (exact counts, sampled content match) — the snapshot/export path is proven lossless.
- **SC-002**: A full `knowledge_rebuild_from_wal` of the real fixture through `tools/call` yields `indices_built: true` and an immediately-working golden search query, with no separate index-build step.
- **SC-003**: Progress notifications are observed for `knowledge_rebuild_from_wal` on the real fixture when a progress token is supplied, and are absent when it is not.
- **SC-004**: Induced database corruption in an isolated temp workspace leaves the service reachable and serving exactly the ADR-0009 degraded-mode allow-list, and both `knowledge_recover` and `knowledge_recover_full` are shown to restore the fixture's expected counts and content from independently-induced degraded copies.
- **SC-005**: `knowledge_build_indices` is shown to restore accurate `indices_built` reporting and working search on a real-scale graph with indices specifically made absent.
- **SC-006**: `knowledge_close` is shown to checkpoint the WAL on clean standalone shutdown (ADR-0017), and `--allow-remote-close` is shown to gate whether an attached-mode close forwards to the remote service.
- **SC-007**: All seven admin tools are absent from `tools/list` and rejected when called without `admin` scope active, verified against the real fixture.
- **SC-008**: The suite makes zero live-LLM or real-embedder network calls end to end, independently verifiable (no API key, no network beyond the suite's own loopback stub).
- **SC-009**: `cargo fmt --all` and the project's clippy gate remain green with the new suite added; the suite runs within its own gated CI budget (excluded from the per-PR critical path, per FR-016), not adding to it.

## Assumptions

- This issue reuses #234's `SeededWorkspace` / `seed_real_corpus_workspace` unmodified (FR-001); it does not re-derive or fork the seeding logic.
- The exact mechanism for inducing database corruption/removal (e.g., truncating or overwriting bytes in the lbug database file, deleting it outright) and for producing an index-less-but-otherwise-healthy graph (User Story 6) are Research/Plan-stage decisions; this spec only requires that the induced conditions be realistic proxies for the failure modes ADR-0009 and #203 describe, and that they be applied only inside isolated temp workspaces (FR-014).
- Which `knowledge_recover` `strategy` value(s) this suite exercises is a Research/Plan-stage decision, made to match whichever corruption/removal mechanism is chosen; the spec only requires that at least one named strategy and the strategy-free `knowledge_recover_full` both be exercised and both shown to restore the fixture's content (FR-008, FR-009).
- Attached-mode spawning for User Story 7's `--allow-remote-close` scenarios follows the existing precedent in `crates/service/tests/mcp_attached.rs` (#213); the exact harness reuse/extension is a Plan-stage decision.
- CI gating mirrors #234's and #235's established pattern (excluded from the default PR gate, run on push-to-main / `workflow_dispatch`) — whether this means a new job in the existing real-corpus e2e workflow or a new workflow file is a Plan-stage decision.
- This issue is test-only: no production admin/lifecycle-handler code changes are in scope, except as a genuine bug fix if this suite's real-scale coverage surfaces one (a Plan/Implement-stage determination, not assumed in advance).
- The suite's own runtime budget (how many full dump/rebuild/recovery cycles it can afford before CI cost becomes unreasonable) is a Plan-stage sizing decision informed by #217/#234's measured per-cycle timings; this spec requires the budgeting happen deliberately, not that a specific number of cycles be hit.

## Out of Scope

- Read-path assertions — covered by #234.
- Mutation-path assertions (`knowledge_delete_*`, `knowledge_merge_entities`, `knowledge_apply_corrections`, reprocessing/canonicalization tools, `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_clear_all`) — covered by #235.
- The lbug upgrade.
- `knowledge_query_cypher` beyond whatever incidental use an admin flow in this suite might already require — no dedicated Cypher-escape-hatch assertions are added by this issue.
- Regenerating or modifying the committed `real_corpus_wal` fixture itself (owned by #217).
- Live LLM or real-embedder calls of any kind — this suite runs entirely against loopback stubs.
- Extraction quality / comparing extraction models against the corpus (#228).

## Source References

- `crates/core/tests/handlers_wal_admin.rs`, `auto_recovery.rs`, `degraded_startup.rs`, `cancel_shutdown.rs` — existing synthetic-graph, direct-dispatch coverage of the admin/lifecycle surface that this issue extends to real scale and through MCP.
- `crates/service/tests/mcp_real_corpus_e2e.rs` and `crates/service/tests/common/real_corpus.rs` (#234) — the MCP-over-stdio read-path suite and reusable seeding harness this issue builds on.
- `crates/service/tests/mcp_attached.rs` (#213) — the existing attached-mode (`--connect`) spawn/drive precedent this issue's `--allow-remote-close` scenarios draw on.
- `crates/core/tests/real_corpus_e2e.rs` and `crates/core/tests/fixtures/real_corpus_wal/` (#217) — the golden fixture and its rebuild→assert precedent.
- `crates/service/src/mcp/tools.rs` — the MCP tool registry, the seven-tool admin scope bucket, and `is_streaming_method` (progress-notification gating for `knowledge_rebuild_from_wal`).
- `crates/service/src/cli.rs` — `--allow-remote-close` parsing and its documented no-effect-in-standalone-mode behavior.
- `docs/adr/0009-degraded-mode-startup-recovery.md` (ADR-0009) — the degraded-mode allow-list (`health_check`, `knowledge_status`, `knowledge_recover`, `knowledge_close`) and recovery-serialization contract this suite asserts against.
- `docs/adr/0017-replace-process-exit-with-normal-return.md` (ADR-0017) — the deterministic-checkpoint-before-exit guarantee this suite's `knowledge_close` assertions verify.
- `docs/adr/0025-auto-heal-index-build.md`, `docs/adr/0026-episode-cursor-wal-resume.md`, `docs/adr/0028-db-wal-dump-compaction.md`, `docs/adr/0036-eager-index-build-at-startup.md` — supporting design context for index rebuild, WAL-resume recovery, and dump/compaction semantics.
- Issue #207 (community field report: undiscoverable recovery tooling and an unrecoverable `relation_type` reset — the primary motivating incident for User Story 3), #203 (index loss under sustained write — the motivating defect for User Story 6).
