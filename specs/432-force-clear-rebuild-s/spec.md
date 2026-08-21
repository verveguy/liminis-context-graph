# Feature Specification: force_clear rebuild's emptiness guard checks the wrong group_id for migrated legacy WAL streams

**Feature Branch**: `fabrik/issue-432`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "`knowledge_rebuild_from_wal`'s `force_clear: true` pre-clear guard (added by #353, scoped per-group by #378) checks and clears data keyed by the request's `group_id` (i.e. the owning group of the WAL directory being replayed) before a `from_seq: 0` full rebuild. This assumes the WAL directory's owning group_id always matches the `group_id` recorded inside the WAL content's own Cypher/params. That assumption is false for a migrated pre-#378 legacy WAL stream whose content carries a different `group_id` than `DEFAULT_GROUP_ID` ('liminis') — the group `wal_group::migrate_wal_root_if_needed` always relocates flat/legacy WAL files into."

## Background

`knowledge_rebuild_from_wal` supports a `force_clear: true` option that, before performing a `from_seq: 0` full rebuild, checks whether the target group already contains data and — if so — clears it first. This guard exists (issue #353, FR-005) to prevent a full replay from re-issuing native `CREATE`s for rows that already exist, which fails with a duplicate-primary-key error per row. Issue #378 scoped this guard per group_id, so that rebuilding one group's data doesn't collide with, or get blocked by, another group's data.

Both the emptiness check and the subsequent clear operate on the **request's `group_id`** — the group that owns the WAL directory being replayed (defaulting to `DEFAULT_GROUP_ID`, `"liminis"`, when the caller omits it). This is correct as long as the WAL directory's owning group_id always matches the `group_id` recorded inside that directory's own WAL content (the `group_id` param embedded in each row's Cypher parameters).

That assumption does not hold for **migrated legacy WAL streams**. Before issue #378 introduced per-group WAL directories, WAL content was written flat, with no directory-level group scoping. `wal_group::migrate_wal_root_if_needed` relocates any such pre-#378 flat/legacy stream into the directory for `DEFAULT_GROUP_ID`, purely because that's the only directory that existed for it to land in — regardless of what `group_id` value(s) the stream's own rows actually carry in their params. A legacy stream whose rows were written for a different, non-default group (e.g. `"apollo_program"`) ends up living in the `DEFAULT_GROUP_ID` directory, and a rebuild request against it (which naturally uses `DEFAULT_GROUP_ID`, since that's the WAL directory's owning group) triggers an emptiness check against `DEFAULT_GROUP_ID` — which may well be empty — while the replay itself issues mutations against the real, already-populated `"apollo_program"` group embedded in the content. The guard passes (finds `DEFAULT_GROUP_ID` empty), no clear happens, and replay proceeds directly against populated data it was never checked against.

This was discovered while implementing issue #429 (fixing four pre-existing test failures on `main`). #429's FR-004 investigation into `[WAL WARN] replay execution error: ... Found duplicated primary key value ...` warnings during `mcp_real_corpus_admin_data_e2e.rs`'s `force_clear: true` full-rebuild block traced the root cause with a temporary debug print in `handle_rebuild_from_wal`'s emptiness-check closure (`crates/core/src/handlers.rs`):

```
[DEBUG 429] non_empty check for "liminis": entity=0, episodic=0, relates=0, force_clear=true
```

The rebuild request omitted `group_id`, defaulting to `"liminis"`. The guard checked (and, finding it empty, never cleared) the `"liminis"` group. But the real-corpus WAL fixture is a pre-#378 flat stream, migrated into `wal_root/liminis/` by `migrate_wal_root_if_needed` — its content carries `"group_id":"apollo_program"` in every row's params (confirmed directly in the fixture's `.jsonl` content and `expected_results.json`). The already-seeded base workspace has ~1,500 `apollo_program`-scoped entities from a prior full rebuild. Replay proceeded directly against the populated `apollo_program` group without ever clearing it, producing ~3,131 of ~11,487 WAL lines failing with "Found duplicated primary key value" — confirmed exact counts via `wal_replay_complete`'s `mutations_replayed: 8356, failed_lines: 3131`.

This is not merely a test-fixture artifact. Any real deployment that predates #378's per-group WAL directories, whose flat pre-#378 stream contains rows for a group_id other than the default, hits the identical guard failure on the first post-upgrade `force_clear: true` full rebuild. The emptiness check and the subsequent clear both operate on the request's `group_id` (== the WAL directory's owning group after migration), not on whatever `group_id` value(s) are actually embedded in that directory's WAL content. The guard silently fails to protect against the exact duplicate-key collision it exists to prevent (FR-005 in #353/#378's history). Because `mcp_real_corpus_admin_data_e2e.rs`'s own assertions in that block are loose (golden-query overlap only, no exact post-rebuild counts), this data-loss potential (~27% of WAL lines silently dropped, per the reproduction above) is currently invisible to callers who don't inspect `failed_lines`/`[WAL WARN]` output.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Full rebuild against a migrated legacy WAL stream does not silently drop data (P1)

An operator (or an automated recovery flow) calls `knowledge_rebuild_from_wal` with `force_clear: true` and `from_seq: 0` (a full rebuild) against a WAL directory that was migrated from a pre-#378 flat/legacy layout. The directory's content contains rows recorded under a `group_id` different from the directory's owning group_id (and hence different from the request's own `group_id`, which the caller typically omits, defaulting to `DEFAULT_GROUP_ID`). That embedded `group_id` already has pre-existing data in the graph, e.g. from an earlier rebuild of the same content. Today, the guard checks emptiness only for the request's `group_id`, finds it empty, and never clears the group the content actually targets — so replay proceeds against already-populated data and a large fraction of WAL lines fail silently with duplicate-primary-key errors, without the caller's request itself reporting failure. After the fix, the guard identifies every `group_id` actually referenced by the WAL directory's content, finds the embedded group_id non-empty, and clears it (since `force_clear: true` was passed) before replay begins — so the full rebuild completes without duplicate-key collisions attributable to this defect.

**Why this priority**: This is the only user story — the issue is a single, well-scoped defect in one guard. It is P1 because it is silent data corruption: the affected caller receives no error and no obviously-wrong top-level result, only a `failed_lines` count and `[WAL WARN]` log lines that most callers do not inspect.

**Independent Test**: Reproducible today via `cargo test --release -p lcg-service --test mcp_real_corpus_admin_data_e2e -- --ignored`, whose "User Story 2 (FR-004) + User Story 4 Scenario 1" block exercises exactly this scenario against the real-corpus fixture. The fix is verified by that same test (or a variant with tightened assertions, see SC-002) reporting zero failures attributable to this collision.

**Acceptance Scenarios**:

1. **Given** a WAL directory owned by group_id G (e.g. `DEFAULT_GROUP_ID`) whose content's rows carry an embedded `group_id` F different from G, and F already has pre-existing data in the graph, **When** `knowledge_rebuild_from_wal` is called with `from_seq: 0` and `group_id` omitted (or set to G) and `force_clear: true`, **Then** the pre-replay guard detects that F is non-empty, clears F's existing data before replay, and the replay completes without duplicate-primary-key failures caused by F's pre-existing data.
2. **Given** the same setup as Scenario 1 but `force_clear` is omitted (or `false`) and no reset is auto-detected, **When** `knowledge_rebuild_from_wal` is called with `from_seq: 0`, **Then** the call is refused with an error identifying that group F (not only G) already contains data, mirroring today's error for the case where G itself is non-empty.
3. **Given** the same setup as Scenario 1 but the call is a `dry_run`, **When** `knowledge_rebuild_from_wal` is called with `from_seq: 0` and `dry_run: true`, **Then** the response reports that a full rebuild would collide with existing data in group F (not only G), consistent with today's dry-run behavior when G itself is non-empty.
4. **Given** a WAL directory whose content's embedded `group_id` values are entirely equal to the request's own `group_id` (the common, non-legacy case unaffected by this defect), **When** `knowledge_rebuild_from_wal` is called with `from_seq: 0` and `force_clear: true`, **Then** behavior and outcome are unchanged from today's behavior.
5. **Given** a WAL directory whose content is empty (no rows) or references only group_id(s) with no pre-existing data, **When** `knowledge_rebuild_from_wal` is called with `from_seq: 0`, **Then** no clear is triggered and replay proceeds as it does today.

### Edge Cases

- WAL directory content references more than one distinct `group_id` value across its rows (e.g. a legacy stream that mixed multiple groups before per-group directories existed). The guard must check and, if `force_clear: true`, clear every referenced group_id that has pre-existing data — not only the first one found or the request's own.
- A row's content is malformed or its `group_id` param cannot be determined. The guard must not crash or fail the whole rebuild request because of an unparseable row; it should behave at least as safely as today's un-queryable-label handling (treated as not blocking the guard), i.e. err on the side of not silently skipping a real collision where it can be avoided, but must not turn a previously-working rebuild into a hard failure for reasons unrelated to this defect.
- `from_seq > 0` (incremental resume, not a full rebuild). The guard does not run in this case today and must continue not to run — this defect and its fix are scoped entirely to the `from_seq: 0` full-rebuild path (see FR-006 of #353/#378's history, which this spec does not revisit).
- A `reset_detected` auto-heal path (unrelated generation mismatch) that also forces `from_seq = 0`. The same guard logic applies here since it already forces `force_clear` semantics regardless of what the caller passed.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: For a `from_seq: 0` full rebuild of a WAL directory, the pre-clear emptiness guard MUST determine emptiness for every `group_id` actually referenced by that directory's WAL content, not only for the request's own `group_id`.
- **FR-002**: If any `group_id` referenced by the WAL content already contains data in the graph, the guard MUST treat this as a collision requiring the same handling that today applies when the request's own `group_id` is non-empty (refuse without `force_clear`, clear-then-replay with `force_clear: true`, refuse-with-explanation on `dry_run`).
- **FR-003**: When `force_clear: true` is set and one or more `group_id`s referenced by the WAL content already contain data, `knowledge_rebuild_from_wal` MUST clear all such groups' data before replay begins — not only the request's own `group_id`.
- **FR-004**: When `force_clear` is not set (and no reset is auto-detected) and any `group_id` referenced by the WAL content already contains data, the rebuild MUST be refused with an error that identifies which group_id(s) collide, extending today's single-group error message to cover this case.
- **FR-005**: In `dry_run` mode, the same check (across every referenced `group_id`, not only the request's own) MUST be evaluated and reported, consistent with today's single-group dry-run refusal behavior.
- **FR-006**: The fix MUST NOT change behavior for `from_seq > 0` incremental resumes — this guard continues to apply only to `from_seq: 0` full rebuilds, exactly as today.
- **FR-007**: The fix MUST NOT change behavior or outcome for the common case where every `group_id` referenced by the WAL content already equals the request's own `group_id` — this is the current, correctly-handled case and must remain unaffected.
- **FR-008**: The fix MUST NOT weaken or bypass the existing concurrency safeguards around clearing (the `active_writes` in-flight-write check and the write lock already used by the single-group clear path) when the clear now spans multiple groups.

### Key Entities

- **WAL directory owning group_id**: The group_id a WAL directory is stored under on disk (`wal_root/<group_id>/`), determined by directory layout — today's guard checks/clears only this value.
- **WAL content embedded group_id**: The `group_id` value recorded inside a WAL row's own Cypher/params, which is the group that row's mutation actually targets when replayed. For WAL directories written after #378, this always equals the owning group_id. For a directory populated by `migrate_wal_root_if_needed` from a pre-#378 flat/legacy stream, it may differ.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a migrated legacy WAL directory whose content's embedded `group_id` differs from the directory's owning group_id and already has pre-existing data, running `knowledge_rebuild_from_wal` with `force_clear: true, from_seq: 0` against it clears that pre-existing data before replay and produces zero "duplicate primary key" WAL warnings/`failed_lines` attributable to this collision.
- **SC-002**: `mcp_real_corpus_admin_data_e2e.rs`'s "User Story 2 (FR-004) + User Story 4 Scenario 1" `force_clear: true` full-rebuild block, when its assertions are tightened to check exact `failed_lines`/`mutations_replayed` counts, shows `failed_lines: 0` and `mutations_replayed` equal to the WAL's total line count — instead of today's `mutations_replayed: 8356, failed_lines: 3131` out of ~11,487 lines.
- **SC-003**: For WAL directories where the content's embedded `group_id` already matches the request's own `group_id` (the common, non-legacy case), rebuild behavior, timing characteristics, and outcome counts are unchanged from today's behavior.

## Assumptions

- The fix is scoped to `handle_rebuild_from_wal`'s pre-clear guard (`non_empty` check) and the clear it triggers (`clear_group_for_rebuild`) in `crates/core/src/handlers.rs`. Other WAL- or group-scoped operations (e.g. `knowledge_delete_by_group`, `group_purge`) are out of scope unless they are found to share this same request-group-vs-content-group assumption.
- Determining which `group_id`(s) a WAL directory's content actually references requires inspecting that content (not just the directory's owning group_id). The concrete mechanism for doing this efficiently (e.g. reusing data already read during replay, a lightweight pre-scan, or restructuring migration to split legacy streams by embedded group_id at migration time instead) is a technical design decision left to the Research/Plan stages, per the issue's own "Suggested fix direction (not investigated in depth)" — this spec defines the required behavior, not the mechanism.
- This defect can only manifest for `from_seq: 0` full rebuilds, since that is the only case in which the emptiness guard runs at all today.
- "Pre-existing data" for a group_id is determined the same way the current guard determines it (entity/episodic/relates-to counts by group_id), extended across multiple group_ids rather than one.

## Out of Scope

- Changing `migrate_wal_root_if_needed`'s migration behavior itself (e.g. splitting a legacy stream into per-embedded-group-id directories at migration time) is one possible mechanism but not mandated by this spec; the Plan stage may choose it or an alternative that satisfies the Functional Requirements above.
- Any other caller or code path that assumes a WAL directory's owning group_id matches its content's embedded group_id, beyond the specific `force_clear` guard described here, unless discovered during Research to share the same root cause.
- Retroactively repairing data that was already silently corrupted by a prior `force_clear: true` rebuild that hit this defect before the fix ships.

## Source References

- `crates/core/src/handlers.rs` — `handle_rebuild_from_wal`'s `non_empty` emptiness-check closure and `clear_group_for_rebuild`.
- `crates/core/src/wal_group.rs` — `migrate_wal_root_if_needed`, `DEFAULT_GROUP_ID`.
- `crates/service/tests/mcp_real_corpus_admin_data_e2e.rs` — "User Story 2 (FR-004) + User Story 4 Scenario 1" block, the existing reproduction.
- Issue #353 (original `force_clear` guard, FR-005), Issue #378 (per-group WAL directory scoping, FR-006/FR-012), Issue #429 (where this defect was discovered while investigating unrelated pre-existing test failures).
