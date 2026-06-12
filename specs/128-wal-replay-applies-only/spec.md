# Feature Specification: WAL Replay: Fix Apostrophe-Induced Parse Failures and Cascading Binder Errors

**Feature Branch**: `fabrik/issue-128`
**Created**: 2026-06-12
**Status**: Draft
**Input**: Production WAL recovery, 2026-06 — replaying a workspace of 43,821 WAL files produced `mutations_replayed: 2,552,224` and `failed_lines: 2,156,181` (84.5% failure rate). The rebuilt DB measured ~71 MB vs. the original ~713 MB. Replay reported "complete" with no error.

## Background

WAL replay (`WalReplayer::replay_opts` → `knowledge_rebuild_from_wal` / `rebuild_from_workspace_wal`) is the documented recovery path for liminis-graph. It is what the app's "Rebuild from WAL" toast triggers and what operators use when the on-disk database is lost or corrupted.

In a production recovery, 84.5% of mutations silently failed. The rebuilt database was roughly one-tenth the size of the original. The replay reported "complete" — failures appeared only as `[WAL WARN]` log lines — so the operator received a populated-but-drastically-incomplete graph with no error signal.

The root cause is that `liminis-graph-core/src/replay.rs` reconstructs each Cypher statement by **string-interpolating params into the template** using a hand-rolled escaper (`json_to_cypher_literal`) and then executes the result via `raw_query`. The escaper doubles apostrophes (`'` → `''`) following standard SQL convention, but the lbug Cypher parser does not accept `''` as an escaped single quote inside a single-quoted literal — it parses up to the first lone `'`, then fails on the remainder. Any WAL mutation whose string params contain an apostrophe (a common occurrence in natural-language knowledge-graph content) produces a Cypher parse error.

These ~18,523 parse errors cascade: when a node `CREATE`/`MERGE` fails to parse, the subsequent `MATCH (n {uuid: …}) SET …` statements for that node bind to an empty result, and accessing `.uuid`, `.episodes`, etc. on the empty match generates additional binder failures. This accounts for the ~1.9 million cascading `Binder exception: Cannot find property uuid for .` errors that dominate the failure count.

A small additional class (~84 failures) references legacy graphiti/FalkorDB-era schema constructs (`Community` table, `HAS` relationship type, `episodes` property) that no longer exist in the current lbug schema. These are schema-compatibility mismatches, not parse errors, and they do not cascade.

The WAL files themselves are intact and uncorrupted. Once this bug is fixed, a re-replay of the same files should reconstruct the full graph.

Related prior issues that addressed different WAL replay problems:
- **Issue #109** — replayer silently dropped MATCH-prefixed mutation queries (classification bug, write-path fix; different from the escaping bug here)
- **Issue #110** — `ReplayStats` did not distinguish failed vs. unrecognised vs. unparseable lines (observability improvement; the stats infrastructure it adds is the appropriate basis for the loud-failure requirement here)

## User Scenarios & Testing *(mandatory)*

### User Story 1 — WAL with Apostrophes in Content Replays Without Data Loss (Priority: P1)

A user with a workspace whose knowledge-graph content contains apostrophes (e.g., `Bob's team`, `Alice's findings`, possessives in any language, contractions) triggers `knowledge_rebuild_from_wal` after losing their database. The replay completes with zero failures for the apostrophe-bearing mutations, and the rebuilt graph contains the expected content.

**Why this priority**: This is the root cause of the 84.5% loss in production. Any content referencing ownership, names with apostrophes, or contractions triggers the bug. It is the default case, not an edge case.

**Independent Test**: Construct a WAL file containing a CREATE mutation whose string params include an apostrophe (e.g., `fact: "Alice's plan"`). Run `knowledge_rebuild_from_wal`. Assert: `failed_lines = 0` and the created node exists in the rebuilt graph with the correct fact string including the apostrophe.

**Acceptance Scenarios**:

1. **Given** a WAL mutation with `fact: "Bob's team findings"` in params, **When** replay runs, **Then** the mutation applies successfully and `failed_lines` is not incremented.
2. **Given** a WAL containing 100 mutations, 50 of which have apostrophes in string params, **When** replay runs, **Then** `mutations_replayed = 100` and `failed_lines = 0` (no parse failures).
3. **Given** a WAL mutation whose string params contain multiple consecutive apostrophes (e.g., `"it's Alice's"`) or apostrophes adjacent to other special characters, **When** replay runs, **Then** the mutation applies without parse error.
4. **Given** the rebuilt DB after replay, **When** the content node is queried, **Then** the stored string value exactly matches the original (apostrophe not doubled, not escaped, not stripped).

---

### User Story 2 — Cascade Is Contained: One Failed CREATE Does Not Generate Thousands of Derived Failures (Priority: P1)

If a mutation does fail for any reason (e.g., schema incompatibility, constraint violation), the failures for subsequent operations on that same node's UUID are counted but not amplified into thousands of new `failed_lines` entries. The reported `failed_lines` count accurately reflects the number of root mutations that failed, not a cascaded product.

**Why this priority**: The current 1.9M `failed_lines` from ~18k root failures makes the stats untrustworthy and misleading. Operators need `failed_lines` to be interpretable as the number of distinct mutations that failed, not as a cascaded amplification.

**Independent Test**: Construct a WAL with one intentionally failing CREATE (e.g., a duplicate unique-key violation on a known node) followed by 10 subsequent `MATCH (n {uuid: …}) SET …` statements referencing the same UUID. Replay. Assert `failed_lines ≤ 11` (the 1 root failure plus at most 10 dependent misses), not a growing cascade.

**Acceptance Scenarios**:

1. **Given** a root CREATE that fails, **When** subsequent MATCH-SET statements for the same UUID execute, **Then** their failure (empty-match binder error) is still counted in `failed_lines` but does not itself generate further derived failures beyond the MATCH-SET count.
2. **Given** a WAL where the root failures are fixed (apostrophe fix from US-1 applied), **When** replay runs on content that previously produced 18k parse errors cascading to 1.9M binder errors, **Then** `failed_lines` is near zero (the cascades no longer occur because the root causes are fixed).

---

### User Story 3 — High Failure Rate Is Reported as a Failed Recovery, Not as Success (Priority: P1)

When replay completes with a `failed_lines / mutations_replayed` ratio above a configurable threshold (default: 10%), `knowledge_rebuild_from_wal` returns an error-class result (or at minimum an unambiguous warning prominently surfaced in the IPC response), not a success response. The operator is not left with a silently truncated graph.

**Why this priority**: Silent data loss — reporting "complete" after discarding 84.5% of mutations — is the user-facing symptom that made this bug so dangerous. Even if the root cause escaping bug is fixed, replay may still partially fail for other reasons (schema drift, constraint violations). The caller must be able to act on a high failure rate.

**Independent Test**: Construct a WAL where 50% of mutations are intentionally invalid (e.g., references to a non-existent table). Run `knowledge_rebuild_from_wal`. Assert: the IPC response does not carry a success-class status, or it carries a clearly populated warning field (e.g., `"fidelity_warning": "50.0% of mutations failed (threshold: 10%)"`) that the caller can detect programmatically.

**Acceptance Scenarios**:

1. **Given** a replay where `failed_lines / total_lines > 10%`, **When** replay completes, **Then** the IPC response includes a fidelity warning field with the observed failure ratio, not just the raw counts.
2. **Given** a replay where `failed_lines / total_lines ≤ 10%`, **When** replay completes, **Then** no fidelity warning is emitted (normal completion).
3. **Given** the fidelity threshold set to 0% via env var (zero-tolerance mode), **When** any mutation fails, **Then** the fidelity warning fires.

---

### User Story 4 — Legacy Schema Records Are Skipped With Count, Not Counted as Failures (Priority: P2)

WAL files written by the earlier graphiti/FalkorDB-based stack may reference schema constructs (`Community` node label, `HAS` relationship type, `episodes` property) that no longer exist in the current lbug schema. These records are skipped with an informative count in the stats, not treated as failures against the failure-rate threshold.

**Why this priority**: The 84 legacy-schema failures from the production recovery are a separate class from the escaping failures. They should be handled as a known-incompatibility skip, not amplified by the fidelity threshold. Mixing them into `failed_lines` would mislead operators about the health of the replayer.

**Independent Test**: Construct a WAL with one `Community` node CREATE and one current-schema Entity CREATE. Replay against a DB with the current schema (no `Community` table). Assert: `mutations_replayed = 1` (the Entity), `legacy_skipped_lines = 1` (the Community), `failed_lines = 0`.

**Acceptance Scenarios**:

1. **Given** a WAL mutation referencing `Community` or `HAS`, **When** replay runs against a current-schema DB, **Then** the mutation is classified as a legacy skip (not a failure), it does not increment `failed_lines`, and it does not trigger the fidelity warning.
2. **Given** a replay stats response, **When** any legacy records were skipped, **Then** the response includes a `legacy_skipped_lines` count so the operator knows how many pre-cutover records were encountered.

---

### Edge Cases

- **String params containing only an apostrophe** (e.g., `"'"`): MUST parse and store correctly.
- **String params containing a backslash before an apostrophe** (`"\\'"`): MUST be stored correctly; no double-escaping.
- **String params containing embedded quotes of the other variety** (e.g., double quotes inside single-quoted context, or vice versa): MUST not be corrupted.
- **Parameterized query API unavailable on replay connection**: If lbug's replay-path connection type does not expose a parameterized-query API, the fallback MUST escape to match the lbug lexer's actual grammar rule (not SQL's `''` convention). The Research stage should determine which API is available. If no parameterized API exists on the replay connection, this must be flagged and the correct lbug escape sequence determined from lbug's lexer source.
- **WAL where all lines are legacy-schema records** (`mutations_replayed = 0`): should not divide-by-zero in fidelity ratio computation.
- **WAL with zero lines**: All counters zero; no fidelity warning; success response.
- **Very long string params** (e.g., embeddings as JSON arrays): parameterized binding must handle large values correctly (already handled if using the same API as the write path).
- **Replay of a WAL already replayed once (idempotency)**: the current behavior on duplicate key violations (MERGE semantics) is out of scope; this issue does not change idempotency behavior.
- **Concurrent replay** (currently single-threaded): no change to concurrency behavior needed; if parallelism is added later, the stats counters must be reviewed at that time.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `interpolate_params` + `raw_query` path in `replay.rs` MUST be replaced with a parameterized-query execution path that passes `wal_line.cypher` and `wal_line.params` to the lbug driver without client-side string escaping. This is the preferred fix. If lbug's replay connection type does not expose a parameterized API, Research must determine the correct lbug escape convention for single-quoted literals and fix `json_to_cypher_literal` accordingly (the `''` doubling is demonstrably incorrect).
- **FR-002**: After FR-001, the `interpolate_params` function and `json_to_cypher_literal` function MUST be removed or deprecated from the replay code path. No hand-rolled Cypher escaping may remain on the hot path of WAL replay. (If the functions are retained for other uses, they are out of scope here.)
- **FR-003**: The replay loop MUST track the set (or count) of node UUIDs whose root CREATE/MERGE mutations failed. Subsequent MATCH-prefixed mutations targeting those UUIDs MAY be counted as cascaded failures with a distinct counter (e.g., `cascaded_failed_lines`) rather than against `failed_lines` — so the operator can distinguish "1 root failure produced 100 cascaded misses" from "100 independent failures." Alternatively, if parameterized queries eliminate the root parse failures entirely (FR-001), this counter may be omitted — the cascade disappears with the root cause. The Research stage must determine which approach is appropriate.
- **FR-004**: `ReplayStats` (or the IPC response shape, if they differ) MUST include a `fidelity_warning` field (or equivalent) that is populated when `failed_lines / (mutations_replayed + failed_lines) > threshold`. The default threshold MUST be 10% and MUST be overridable via env var `LCG_REPLAY_FIDELITY_THRESHOLD` (float, 0.0–1.0). The warning MUST include the observed ratio and the threshold.
- **FR-005**: The `wal_replay_complete` IPC event / `knowledge_rebuild_from_wal` response MUST include the `fidelity_warning` field. If the caller is liminis-app, it MUST surface the warning in the UI rather than treating any replay as a success.
- **FR-006**: WAL mutations that fail due to a schema-compatibility mismatch on a known-legacy construct (`Community` node label, `HAS` relationship type, `episodes` property) MUST be counted in a `legacy_skipped_lines` counter in `ReplayStats`. They MUST NOT increment `failed_lines` and MUST NOT count against the fidelity threshold.
- **FR-007**: Detection of legacy-schema mutations MUST be based on the lbug error text (pattern-match on known error strings like `Table Community does not exist`, `Table HAS does not exist`, `Cannot find property episodes for`). If the error text changes in a future lbug version, the detection may miss new occurrences; this is an accepted limitation. The detection pattern MUST be documented as a constant in the source.
- **FR-008**: A regression test MUST verify: (a) a WAL mutation with an apostrophe in a string param applies successfully and the stored value is correct; (b) a WAL with `failed_lines / total > 10%` produces a non-empty `fidelity_warning`; (c) a WAL referencing `Community` increments `legacy_skipped_lines` and not `failed_lines`; (d) existing replay tests for CREATE/MERGE paths still pass.
- **FR-009**: The existing `failed_samples` mechanism (from issue #110, if landed) MUST continue to capture samples from `failed_lines`. If issue #110 has not yet landed, this issue MUST NOT remove the `[WAL WARN]` log lines (the only existing diagnostic signal).

### Key Entities

- **`ReplayStats`**: The Rust struct (and its IPC serialization) that accumulates per-replay counters. This issue adds or modifies: `failed_lines` (already exists per issue #110), `legacy_skipped_lines` (new), `fidelity_warning: Option<String>` (new).
- **`interpolate_params` / `json_to_cypher_literal`**: The hand-rolled param interpolation functions in `replay.rs` that are the root cause. FR-001 removes them from the hot path.
- **Legacy-schema constructs**: Node label `Community`, relationship type `HAS`, property `episodes` — graphiti/FalkorDB-era names not present in the current lbug schema. Treated as a known skip class.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Re-running the production recovery WAL (43,821 files, 2,552,224 total mutations) after this fix produces `failed_lines` near zero (target: < 1% for mutations whose params contain apostrophes; the only remaining failures should be genuine schema or constraint issues unrelated to escaping).
- **SC-002**: The rebuilt DB from the production recovery WAL is within 10% of the original DB size (~713 MB target, previously ~71 MB rebuilt). This is the concrete durability-guarantee outcome.
- **SC-003**: A unit test with a WAL line containing `fact: "Bob's team"` passes with `mutations_replayed = 1, failed_lines = 0`.
- **SC-004**: A unit test with a WAL whose `failed_lines / total > 10%` produces a non-null `fidelity_warning` in the IPC response.
- **SC-005**: A unit test with a `Community` node CREATE produces `legacy_skipped_lines = 1, failed_lines = 0`.
- **SC-006**: Existing replay regression tests pass unchanged (no regressions on CREATE/MERGE paths).
- **SC-007**: No hand-rolled Cypher string-escaping remains in `replay.rs` on the mutation-execution hot path after the fix.

## Assumptions

- **A1**: The lbug driver used on the replay connection (`Conn` or equivalent) exposes a parameterized-query API compatible with the `{key: value}` param map stored in the WAL. If it does not, Research must identify the alternative (correct lbug escape rule or a different API surface). This is the most important open question for Research.
- **A2**: The WAL files from the production recovery are intact (confirmed in the issue — the user states "the WAL files themselves are intact — they have been preserved"). A re-replay after this fix will recover the full graph.
- **A3**: `legacy_skipped_lines` detection via lbug error-string pattern matching is sufficient for the ~84 known legacy-schema failures. We do not need to pre-parse the Cypher to detect these.
- **A4**: The fidelity threshold (10% default) is appropriate for the current deployment. If operators have workloads with intentionally high legacy-skip rates, they can lower the threshold via env var.
- **A5**: The `wal_replay_complete` IPC event and the `knowledge_rebuild_from_wal` response carry the same `ReplayStats` shape (or the IPC shape is derived from it). If they diverge, both must be updated.
- **A6**: The liminis-app UI can be updated to surface `fidelity_warning` alongside the existing replay result display. The app-side change is in scope (it is the final step in the user-facing fix) but its implementation details are left to the Implement stage.

## Out of Scope

- Fixing the app-side lockout that forced the manual WAL-rebuild path in the first place (verveguy/liminis#846 — the degraded-mode guard that blocks `knowledge_rebuild_from_wal`; a separate liminis-app issue).
- Retrying failed mutations automatically during replay.
- Persisting failure details to disk for post-hoc analysis beyond the in-memory `failed_samples` approach.
- Full schema migration / translation of legacy-era records into current schema equivalents. Skipping-with-count is the scope here; a proper migration tool is a separate effort.
- Parallelising WAL replay (counter thread-safety at that point is deferred).
- Redacting sensitive content from `failed_samples` Cypher snippets.
- Changing the WAL write format to pre-escape or pre-validate strings at write time. The write path is not touched; only the replay path is changed.

## Source References

- `liminis-graph-core/src/replay.rs` — `WalReplayer`, `interpolate_params`, `json_to_cypher_literal`, `raw_query` call site; the root-cause implementation
- `liminis-graph-core/src/handlers.rs` — `knowledge_rebuild_from_wal` handler; IPC return shape
- `liminis-graph-core/src/telemetry.rs` — `WalReplayProgress` and related telemetry event types
- Issue #110 — `ReplayStats` split into `failed_lines` / `unrecognised_lines` / `unparseable_lines` / `failed_samples`; this issue builds on that infrastructure
- Issue #109 — MATCH-prefixed mutation classification fix; distinct from the escaping bug here but related WAL replay work
- Issue #84 — WAL file rotation; context for WAL layout and multi-file replay
