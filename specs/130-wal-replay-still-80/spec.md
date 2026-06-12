# Feature Specification: WAL Replay Timestamp Typing — Fix STRING→TIMESTAMP Cast Failures

**Feature Branch**: `fabrik/issue-130`
**Created**: 2026-06-12
**Status**: Draft
**Input**: User description: "WAL replay still ~80% lossy after #128: timestamps interpolated as STRING literals fail TIMESTAMP columns, cascading ~1.98M binder errors (edges lost)"

## Background

After issue #128 fixed apostrophe escaping in `json_to_cypher_literal` (replay.rs), WAL replay
**still fails ~80.1% of mutations** in production. A real recovery run on the same WAL set as
#128 shows:

```json
{"mutations_replayed": 2565540, "failed_lines": 2054053,
 "legacy_skipped_lines": 88812, "unrecognised_lines": 0,
 "unparseable_lines": 0, "duration_ms": 2466219}
```

Full error taxonomy for that run:

| Count | Error | Class |
|------:|-------|-------|
| 1,879,387 | `Binder exception: Cannot find property <x> for .` | cascade |
| 70,298 | `Expression <ts> has data type STRING but expected TIMESTAMP. Implicit cast is not supported.` | **root** |
| 55,027 | `Binder exception: Cannot find property <x> for r` | cascade (relationships) |
| 47,735 | `Binder exception: Cannot find property <x> for e` | cascade (entities) |
| 28 | `Binder exception: Table HAS …` | legacy |
| **0** | `expected rule oC_SingleQuery` | ✅ fixed by #128 |

**Root cause**: `json_to_cypher_literal` now escapes strings correctly (#128) but has no typed
handling for temporal values. A timestamp arrives in `wal_line.params` as a JSON string — e.g.
`"2026-03-25T16:58:57.761788+00:00"` — and is emitted as a plain single-quoted Cypher literal:

```
SET e.created_at = '2026-03-25T16:58:57.761788+00:00'
```

lbug binds this as `STRING`, but the column is declared `TIMESTAMP`, and lbug does not support
implicit `STRING → TIMESTAMP` casts. The statement fails. Because timestamps appear on
episode/edge creates (`created_at`, `valid_at`, `expired_at`, etc.), those creates fail, and
every subsequent `MATCH` that looks up the node/edge by uuid binds nothing — hence the 1.98M
cascade of "Cannot find property uuid" errors.

**Net data effect**: The rebuilt DB has nodes but is missing most of its edge structure. The DB
grew from 71 MB (pre-#128) to 240 MB (post-#128) as apostrophe-heavy entity creates now land,
but edges — which carry temporal validity fields — are still largely lost. For a knowledge graph,
this means recovery produces an almost-edgeless graph: structurally unusable.

This is the same class of bug as #128 (hand-interpolating typed values as Cypher string literals
rather than typed Cypher literals or bound parameters). #128 fixed the *escaping* symptom;
this issue fixes the *typing* problem for non-string values. The recurring pattern is that
`json_to_cypher_literal` is a hand-rolled per-type literal emitter that has been missing cases.

Rebuilt DB size comparison: 71 MB (pre-#128) → 240 MB (post-#128, nodes land but edges lost) →
713 MB (original). The gap to 713 MB is the lost edge structure this issue must close.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — WAL Recovery Restores a Complete Graph Including Edges (Priority: P1)

A user with a production workspace whose WAL contains temporal properties on edges and episodes
triggers `knowledge_rebuild_from_wal` and obtains a graph with both nodes and edges intact —
not just nodes. The recovered DB's size and edge count approach the original DB's values.

**Why this priority**: Without edges, a knowledge graph is not usable. WAL replay is the
primary recovery path for production workspaces. Until this is fixed, WAL replay cannot be
considered a viable recovery story.

**Independent Test**: Take a WAL fixture whose params include ISO-8601 timestamp strings on
edge/episode creates (`created_at`, `valid_at`, `expired_at`). Replay into an empty DB. Assert:
`failed_lines == 0`, the edge is created with the correct `TIMESTAMP` value, and the `uuid` of
the edge is subsequently retrievable by MATCH.

**Acceptance Scenarios**:

1. **Given** a WAL line that sets `e.created_at = '<ISO-8601 timestamp>'`, **When** replay
   executes it, **Then** the mutation succeeds and `e.created_at` is stored as a `TIMESTAMP`
   (not a string).
2. **Given** a WAL containing episode creates and subsequent edge creates each carrying
   `created_at`, `valid_at`, `expired_at` timestamps, **When** replay completes, **Then**
   `failed_lines` is zero and the edge nodes are findable by uuid in subsequent MATCHes.
3. **Given** the production WAL set from the #130 bug report, **When** replay completes,
   **Then** `failed_lines` is ≤ 1% of `mutations_replayed` (vs the current 80.1%).

---

### User Story 2 — The Literal Serializer Does Not Require Future Issues for Further Temporal Types (Priority: P2)

The `json_to_cypher_literal` function handles all JSON value types that appear in production
WAL params in one well-tested path, so that no future temporal or typed value (dates, durations,
lists of timestamps) silently reverts to bare-string emission.

**Why this priority**: #128 fixed strings; this issue fixes timestamps. The same anti-pattern
will recur for every new type unless the serializer is made structurally complete. A
comprehensive approach prevents a third issue in this class.

**Independent Test**: Pass a representative set of JSON value types through the serializer
(string, integer, float, boolean, null, ISO-8601 timestamp) and assert each produces the correct
Cypher literal form with no bare-string emission for non-string types.

**Acceptance Scenarios**:

1. **Given** a JSON integer value (e.g. `42`), **When** `json_to_cypher_literal` is called,
   **Then** it emits `42` (no quotes).
2. **Given** a JSON boolean value (e.g. `true`), **When** called, **Then** it emits `true` (no
   quotes).
3. **Given** a JSON `null` value, **When** called, **Then** it emits `null`.
4. **Given** a JSON string whose content matches ISO-8601/RFC-3339 format (e.g.
   `"2026-03-25T16:58:57.761788+00:00"`), **When** called, **Then** it emits a typed temporal
   Cypher literal using the constructor form that lbug's parser accepts (exact form to be
   confirmed in Research), **not** a bare single-quoted string.
5. **Given** a JSON string that does NOT match the ISO-8601 pattern (ordinary text), **When**
   called, **Then** it still emits an escaped single-quoted string (existing #128 behavior
   preserved).

---

### User Story 3 — Regression Test Prevents Silent Recurrence (Priority: P1, paired with US-1)

A unit test fixture with WAL lines containing timestamp params is added, and the test asserts
`failed_lines == 0` and that the stored value round-trips as a `TIMESTAMP`. This test
immediately catches any future regression that would strip timestamp-typed handling.

**Why this priority**: The #128 and #130 bugs both went undetected until real recovery runs.
Regression coverage at the unit level means the next related mistake surfaces in CI, not in a
recovery run on production data.

**Independent Test**: Add a test that replays a minimal WAL fixture containing at least one
episode CREATE with `created_at` and one edge CREATE with `created_at`, `valid_at`,
`expired_at`. Assert: (a) `failed_lines == 0`, (b) the stored `created_at` value is `TIMESTAMP`
(not `STRING`), (c) a subsequent MATCH by uuid retrieves the node successfully.

**Acceptance Scenarios**:

1. **Given** the timestamp fixture WAL, **When** replayed in CI, **Then** `failed_lines == 0`.
2. **Given** a regression that removes timestamp detection from `json_to_cypher_literal`,
   **When** CI runs, **Then** the test fails visibly.

---

### Edge Cases

- **Timezone offset vs. UTC shorthand**: Both `2026-03-25T16:58:57.761788+00:00` and
  `2026-03-25T16:58:57Z` (and `+05:30` offsets) MUST be detected as timestamps. The regex
  pattern MUST cover the full RFC-3339 family.
- **Timestamps with microseconds**: `2026-03-25T16:58:57.761788+00:00` (6 decimal places) MUST
  be detected, as this is the Python `datetime.utcnow()` format used by graphiti.
- **Date-only strings** (`2026-03-25`): These are not present in production WAL params; treating
  them as regular strings (no special-casing) is acceptable. If lbug has a `date()` type and
  date-only WAL params are discovered during Research, a follow-up issue should handle them.
- **Duration strings** (e.g. `PT1H30M`): Not present in production WAL params; out of scope.
  A follow-up issue should address if discovered.
- **List-typed params**: JSON arrays in params (e.g., embedding vectors) MUST NOT be affected
  by the timestamp detection path. Only scalar strings should be inspected for ISO-8601 shape.
- **Strings that coincidentally match ISO-8601**: A property value like `"2026-01-01T00:00:00Z"`
  stored as a note or label (STRING column) would be emitted as a temporal literal and fail if
  the column is STRING. This is an accepted trade-off: the WAL carries no type tag, and all
  known production uses of ISO-8601-shaped strings in params are on TIMESTAMP columns. If a
  false-positive is discovered, the WAL writer should add explicit type hints (future work,
  see Out of Scope).
- **Existing `legacy_skipped_lines` handling**: The 88,812 lines classified as legacy
  (Community/HAS/episodes schema from pre-#128 WAL) MUST remain correctly classified and MUST
  NOT be affected by this fix.
- **`fidelity_warning`**: The 80% failure rate currently triggers `fidelity_warning` in the IPC
  response (added in #128). After this fix, `fidelity_warning` MUST NOT fire for a production
  WAL whose only failures were timestamp-typed (i.e., the warning mechanism MUST remain
  correctly calibrated to actual failure rates).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `json_to_cypher_literal` in `liminis-graph-core/src/replay.rs` MUST detect
  JSON string values whose content matches the ISO-8601/RFC-3339 datetime format (date + `T` +
  time + optional fractional seconds + timezone offset or `Z`) and emit them as typed temporal
  Cypher literals rather than plain single-quoted strings.
- **FR-002**: The temporal Cypher literal form emitted MUST be the constructor syntax that
  lbug's Cypher parser accepts for TIMESTAMP columns. The exact function name and call form
  (e.g., `timestamp('…')`, `datetime('…')`, or another form) MUST be verified against lbug's
  Cypher documentation or test suite during the Research stage before implementation.
- **FR-003**: The ISO-8601 detection MUST use a regex or equivalent pattern that matches the
  full RFC-3339 family: date portion (`YYYY-MM-DD`), `T` separator, time portion
  (`HH:MM:SS`), optional fractional seconds (`.N+`), and timezone designator (`Z` or
  `±HH:MM`). Simple prefix/suffix checks are not sufficient.
- **FR-004**: The detection MUST be applied only to JSON string values. JSON numbers, booleans,
  nulls, and arrays MUST NOT be inspected for the timestamp pattern.
- **FR-005**: The handling of all other JSON value types MUST be reviewed and made exhaustive in
  the same change: at minimum, JSON integers, floats, booleans, and null MUST emit their
  correct unquoted Cypher literal forms, not string representations. (Note: booleans and
  integers may already be handled; this requirement ensures the audit is done and any missing
  cases are fixed in the same PR.)
- **FR-006**: A regression test fixture MUST be added containing at minimum:
  (a) an episode CREATE with `created_at` set to an ISO-8601 timestamp;
  (b) an edge CREATE with `created_at`, `valid_at`, and `expired_at` set to ISO-8601 timestamps;
  (c) a subsequent MATCH on the created edge by uuid.
  The test MUST assert `failed_lines == 0` and that the edge is retrievable after replay.
- **FR-007**: The fix MUST NOT change the WAL file format. Existing WAL files on disk MUST replay
  correctly without migration. The detection and literal conversion happen at replay time, not at
  write time.
- **FR-008**: The `fidelity_warning` threshold (10%, added in #128) MUST remain unchanged. After
  this fix, a production WAL whose only source of failures was the timestamp typing issue MUST
  produce a `fidelity_warning: false` response.

### Key Entities

- **`json_to_cypher_literal`** (replay.rs): The function that maps a JSON `Value` to its
  Cypher literal string. This is the single code site where all typed-literal emission
  must happen.
- **WAL param**: A JSON value stored in `wal_line.params` that is interpolated into the
  Cypher string during replay. WAL params carry no type tag; type inference happens at replay
  time.
- **Temporal Cypher literal**: The typed constructor form that lbug's Cypher binder accepts
  for TIMESTAMP columns — as opposed to a bare quoted string which it rejects.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Replaying the production WAL set from the #130 bug report yields `failed_lines`
  ≤ 1% of `mutations_replayed` (vs 80.1% today). The residual failures (if any) are from
  causes unrelated to timestamp typing.
- **SC-002**: The rebuilt DB from a full production WAL replay contains edges at a rate
  consistent with the original DB (i.e., the edge-to-node ratio approaches the original 713 MB
  DB, not the edge-sparse 240 MB post-#128 DB).
- **SC-003**: After the fix, `fidelity_warning` is `false` in the `knowledge_rebuild_from_wal`
  IPC response for a production WAL whose primary failure mode was timestamp typing.
- **SC-004**: The regression test (FR-006) passes in CI: `failed_lines == 0`, edge created with
  TIMESTAMP-typed properties, edge findable by uuid in subsequent MATCH.
- **SC-005**: All existing replay tests pass unchanged — no regressions in CREATE/MERGE paths
  or string-escaping behaviour introduced by #128.
- **SC-006**: `cargo test`, `cargo clippy --release --all-targets -- -D warnings`, and
  `cargo fmt --all` all pass on the PR branch.

## Assumptions

- **A1**: The root cause of the 70,298 `STRING but expected TIMESTAMP` errors is exclusively the
  bare-string emission of ISO-8601 timestamps by `json_to_cypher_literal`. No other code paths
  contribute to this error class.
- **A2**: lbug has a Cypher temporal constructor function (exact name TBD in Research) that
  accepts ISO-8601/RFC-3339 strings and produces a TIMESTAMP value. If lbug uses a different
  typed literal syntax, the Research stage must surface the correct form.
- **A3**: The WAL params do not contain date-only or duration-typed values in the production WAL
  set. If Research surfaces other temporal types in the WAL, those can be added to
  `json_to_cypher_literal` in the same PR.
- **A4**: The `legacy_skipped_lines` classification logic from #128 is correct and complete —
  this fix does not need to change it.
- **A5**: WAL replay is single-threaded. No concurrency consideration applies to the
  serializer change.
- **A6**: The WAL writer (both Python graphiti and Rust liminis-graph) will continue to store
  temporal values as ISO-8601 strings in JSON params. Adding explicit type hints to the WAL
  format is a future improvement (see Out of Scope) and is not a prerequisite for this fix.

## Out of Scope

- **WAL writer type hints**: Adding an explicit type tag to WAL param entries so the replayer
  doesn't need to infer type from value shape. This is a more robust long-term approach but
  requires coordinated changes to the Python and Rust writers and a WAL format version bump.
  It should be a follow-up issue filed after this fix lands.
- **Date-only types** (`YYYY-MM-DD` with no time component): Not present in production WAL
  params. Follow-up if discovered.
- **Duration types** (e.g. `ISO 8601 duration`): Not present in production WAL params.
  Follow-up if discovered.
- **List-of-temporal params**: Not present in production WAL params.
- **Bound parameter APIs**: lbug's ADR-001 references a future bound-parameters API. If/when
  that becomes available, migrating off string interpolation entirely would eliminate the
  entire `json_to_cypher_literal` class of bugs. That migration is out of scope for this fix.
- **Re-running or retrying failed mutations from a previous replay**: Not addressed here. The
  fix prevents future failures; it does not provide tooling to re-replay historically failed lines
  without re-running the full WAL.
- **Performance benchmarks**: This fix changes correctness, not performance. No benchmarking
  is required as part of this issue.

## Source References

- **`liminis-graph-core/src/replay.rs`** — `json_to_cypher_literal` function; the site of the
  fix. Also contains the cascade of binder errors that motivated the stats taxonomy in the issue.
- **Issue #128** — apostrophe escaping fix; same root anti-pattern. `json_to_cypher_literal`
  was updated there; this issue continues that work.
- **Issue #109** — WAL replayer MATCH-prefixed mutation acceptance; same replay.rs file.
- **Issue #110** — WAL replayer stats split (`failed_lines` vs `unrecognised_lines`); the
  per-class counters introduced there allow the 70,298 root-cause errors to be isolated.
- **lbug Cypher temporal syntax** — exact constructor form for TIMESTAMP literals TBD; must be
  confirmed in Research (check lbug test suite or official Cypher docs for lbug's dialect).
