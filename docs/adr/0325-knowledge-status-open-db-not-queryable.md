# ADR-0325: `knowledge_status` Reports "Open But Not Queryable" as a Second Degraded State

**Status**: Accepted
**Date**: 2026-08-02
**Issue**: #325

## Context

`mcp_real_corpus_admin_data_e2e`'s User Story 6 deliberately renames the `Entity` table away
(`ALTER TABLE Entity RENAME TO EntityTmp`, via the `knowledge_query_cypher` admin escape hatch)
to exercise `knowledge_build_indices`'s genuine-failure path, then checks that `knowledge_status`
still reports `indices_built: false` honestly. Instead, `knowledge_status` itself hard-errored:

```
database error: Query execution failed: Binder exception: Table Entity does not exist.
```

`handle_knowledge_status` (`crates/core/src/handlers.rs`) calls `conn.count_nodes("Entity")?`
(and three sibling table-touching queries: `count_relates_to_edges`, `get_latest_episode_time`,
`get_earliest_episode_time`) with hard `?` propagation. Any lbug error — including "table doesn't
exist" — turned the whole health-check handler into a JSON-RPC error, at exactly the moment an
operator needs it most.

Research (issue #325) ruled out both hypothesized root causes from the issue's original framing:
neither ADR-0046's clear-before-replay path (schema not recreated) nor a stale `ArcSwap<Db>`
handle is the cause, and `clear_db_for_rebuild` isn't even reachable by the failing test. A
bisect further showed this is not a regression at all — `mcp_real_corpus_admin_data_e2e`, added
the same day as an unrelated ADR-0046 fix, is simply the first test ever written that exercises
`knowledge_status` against a genuinely broken open database. The unguarded `?` predates it by a
large margin. See the issue thread for the full bisect evidence.

### The gap in ADR-0009's model

ADR-0009 established `AppState.db: ArcSwapOption<Db>` and a degraded-mode response shape for
exactly one failure axis: `db_opt.is_none()` — the database never opened at all (WAL corruption,
missing file, ...). That branch already has its own `degraded`/`reason`/`recovery_available`
shape and correctly returns a status object instead of erroring.

This issue is a **second, distinct** failure axis ADR-0009 doesn't cover: the `Db` handle is
valid and open (`Some`), but a specific table inside it is unqueryable. `connected` (an open
handle) and "every core table exists" are independent booleans — a 2×2, not the 1D model
ADR-0009's `degraded` flag alone can express.

## Decision

Extend the existing `Binder exception:` substring-classification pattern (`is_missing_index_error`,
`is_already_exists_error` in `crates/core/src/error.rs`) with a third classifier,
`is_missing_table_error`, matching lbug's `Table <X> does not exist` phrasing — textually
disjoint from the other two ("doesn't have an index with name", "already exists in table").

`handle_knowledge_status`'s blocking closure separates its table-touching queries
(`count_nodes("Entity")`, `count_nodes("Episodic")`, `count_relates_to_edges`,
`get_latest_episode_time`, `get_earliest_episode_time`) from the WAL-directory scan and the
in-memory name-index counters, which don't depend on any table and always succeed. On a
classified missing-table error from the table-touching group, the handler returns a
**not-queryable status response** instead of propagating. Any other error — a differently-worded
binder exception, a malformed column, anything not matching the specific phrasing — still
propagates via `?` (FR-006): the classifier is narrow by construction, so it cannot mask a
genuine, unrelated failure.

The response gains a new field, **`queryable: bool`**, present and accurate in all three status
states:

| State | `connected` | `queryable` | counts |
|---|---|---|---|
| Healthy (DB open, all tables present) | `true` | `true` | numeric (0 for a genuinely empty graph) |
| **New: open, core table missing** | `true` | `false` | `null` |
| DB never opened (ADR-0009) | `false` | `false` | absent/not applicable |

`entity_count`/`relationship_count`/`episode_count`/`last_index_time` are **`null`, not `0`**,
in the not-queryable state (FR-003). A boolean flag alone would be a regression risk the moment a
caller reads counts without checking it first; `null` vs. `0` is unambiguous even to a caller
that ignores `queryable` entirely — a broken graph can never be silently read as an empty one.
The not-queryable branch reuses the existing `reason` field name (already used by the
DB-not-open branch) rather than inventing a second field, so a caller already handling degraded
`reason` gets the new state for free.

`indices_built` is forced to a literal `false` in the not-queryable branch, rather than read from
the live atomic as in the healthy branch. `handle_build_indices` already stores the real
`build_result.is_ok()` into `state.indices_built` before propagating its own error, so in the
sequence this issue's repro exercises (a failed `knowledge_build_indices` call precedes
`knowledge_status`) the atomic already reads `false` on its own. But `knowledge_status` can be
called directly after a table breaks, without an intervening failed build call — if indices were
built successfully *before* the table went missing, the atomic would still read stale `true`. That
is the exact class of staleness bug #297 already fixed once for a different code path; forcing the
literal here (instead of trusting the atomic) closes it for this one too, since indices on a
missing table are meaningless regardless of what was last recorded.

Nothing is cached: like every other field this handler already reports, `queryable` is derived
per-request from the live query result, not stored in `AppState`. Renaming the table back and
calling `knowledge_status` again immediately reports `queryable: true` with no invalidation step
— required by the issue's edge case that a rename-back must not leave stale-broken state behind.

## Consequences

### Positive

- `knowledge_status` remains answerable exactly when ADR-0009's socket-before-database ordering
  says it should be — a broken table is now a status the caller can read, not an exception that
  crashes the health check.
- The `null`-vs-`0` contract makes "broken" and "empty" structurally distinguishable without
  relying on every caller remembering to check `queryable` first.
- The fix is narrowly scoped (one handler, one new classifier) and doesn't touch the
  already-correct `indices_built` tracking or the already-correct ADR-0009 DB-not-open branch.

### Negative / Residual risks

- **Scope is deliberately limited to `knowledge_status`.** Research's FR-005 audit found 10 other
  read-side handlers (`knowledge_find_entities`, `knowledge_find_relationships`,
  `knowledge_search_passages`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`,
  `knowledge_get_edges_by_uuids`, `knowledge_list_entities`, `knowledge_list_relationships`,
  `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source`) that also hard-error on a
  missing `Entity` table. Extending the same tolerance to all of them was judged out of scope for
  this issue (User Story 1 scopes the defect to the status endpoint specifically) and is left as
  documented, unfixed scope — see the PR body's audit table.
- **lbug wording drift**: if a future lbug version changes the missing-table binder exception
  text, `is_missing_table_error` silently stops matching and the bug resurfaces. Mitigated the
  same way as the other two classifiers: a regression test (`crates/core/src/db.rs`,
  `missing_table_error_tests`) pinned to the real induced error string.
- `queryable` is a new field client code must learn to check; existing callers that only read
  `entity_count` etc. are unaffected by its *addition*, but now see `null` instead of a value in
  the not-queryable case where they previously saw an exception — a strictly more informative
  failure mode, not a breaking one, since no caller could have depended on the old exception's
  shape.

## Alternatives Considered

### Reuse `degraded`/`connected` instead of a new `queryable` field

Rejected: `connected` already means "DB handle open" (true in this new state) and `degraded`
(ADR-0009) already means "DB never opened." Reusing either would conflate two independent
failure axes the design explicitly wants to keep distinguishable — a 2×2, collapsed into 1D,
loses information a caller might need (e.g. "is a reconnect worth trying?" vs. "is a schema
repair worth trying?").

### Blanket-catch every error in the closure, not just missing-table

Rejected: violates FR-006. A blanket catch would silently degrade genuine failures (a malformed
query, resource exhaustion, a real bug) into an innocuous-looking status response instead of
surfacing them as errors — exactly the "mask real errors" failure mode the spec calls out.

### Fix all 8 handlers found by the FR-005 audit in this PR

Rejected for this issue: an ~8-handler diff the spec doesn't require (FR-005 asks to audit and
state, not fix). Documented as explicit deferred scope instead of expanded silently mid-implementation.

## Related

- ADR-0009 — degraded-mode startup and recovery; the model this ADR extends with a second axis.
- ADR-0003 — `ArcSwap<Db>` hot-swap; ruled out as a contributing mechanism by Research.
- ADR-0025 / ADR-0036 — missing-*index* auto-heal, the adjacent-but-distinct failure mode (a
  different, textually disjoint binder-exception phrasing) this issue's classifier must not
  conflate with.
- `crates/core/src/error.rs` — `is_missing_table_error`, alongside `is_missing_index_error` and
  `is_already_exists_error`.
- `crates/core/src/handlers.rs` — `handle_knowledge_status`.
- `crates/core/tests/ipc_parity.rs` — fast synthetic-DB regression coverage for FR-001–003/006.
- `crates/service/tests/mcp_real_corpus_admin_data_e2e.rs` — User Story 6, the original repro.
- #297 — the prior `knowledge_status` truthfulness fix (`indices_built` staying stale after
  runtime recovery); same theme, different mechanism.
