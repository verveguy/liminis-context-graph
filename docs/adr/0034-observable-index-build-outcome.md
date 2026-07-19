# ADR-0034: Observable Index-Build Outcome — Fixing ADR-0025's Dead-Code Failure Path

**Status**: Accepted
**Date**: 2026-07-16
**Issues**: #192 (this fix); builds on #146 and #58 via ADR-0025

## Context

ADR-0025 documents that `handle_rebuild_from_wal` owns the FTS/HNSW index-DDL lifecycle for WAL
reload: drop FTS before replay, bulk-replay with no indexes present, then call
`Conn::build_indices_and_constraints` once after replay to rebuild both FTS and HNSW. Its stated
consequence: "Post-reload searches are immediately available — no lazy-build stall on the first
query." Both rebuild-handler code paths (streaming and background-job) implement this by treating
a build failure as non-fatal: `if let Err(e) = ... { eprintln!(...) } else { indices_built.store(true) }`.

In practice (#192), a production rebuild (113 WAL files, 5,565 mutations) reported `success: true`
with correct replayed counts, yet `knowledge_find_entities`/`knowledge_find_relationships`
returned empty until `knowledge_build_indices` was called by hand — the exact trap ADR-0025 was
meant to prevent.

The root cause was structural, not incidental: `Db::create_vector_indexes` (`db.rs`) and
`schema::create_fts_indexes` (`schema.rs`) wrapped every `CALL CREATE_VECTOR_INDEX`/
`CALL CREATE_FTS_INDEX` in `let _ = self.raw_query(...)` and unconditionally returned `Ok(())`,
with a comment claiming this was to suppress "already exists" errors for idempotency. In reality
it suppressed *every* error — there was no error inspection at all. This made
`Conn::build_indices_and_constraints()` structurally incapable of ever returning `Err`, for any
reason. As a direct consequence:

1. The `else { indices_built.store(true) }` branch in both rebuild paths ran unconditionally —
   ADR-0025's documented `eprintln!`-and-stay-`false` failure branch was **dead code**.
2. Once `indices_built` was (possibly incorrectly) `true`, the auto-heal path in
   `handle_find_entities`/`handle_find_relationships` — which only fires when `indices_built` is
   currently `false` — was permanently disabled for that DB lifecycle. A genuine missing-index
   binder exception then surfaced as an explicit `MISSING_INDEX_USER_MSG` IPC error instead of
   triggering rebuild-and-retry.

The exact lbug/Kuzu-level condition that triggered a genuine (non-"already exists")
`CREATE_VECTOR_INDEX`/`CREATE_FTS_INDEX` failure in the field report was not reproduced during
Research (lbug ships as a prebuilt binary; its FTS/HNSW extension internals aren't inspectable as
plain source). The blanket-suppression defect is independently sufficient to explain the observed
behavior and is the fix target regardless of the field trigger's exact identity.

A second, related gap in the same code region: FTS indexes are dropped before replay
(`schema::drop_fts_indexes`) so the post-replay `CREATE_FTS_INDEX` is a true from-scratch rebuild,
but HNSW vector indexes were never dropped before replay. On a rebuild against a workspace with
pre-existing HNSW indexes, `CREATE_VECTOR_INDEX` would fail with "already exists" — which, once
that case stops being blanket-swallowed-as-success-and-move-on, needed a corresponding drop step
or it would leave a **stale**, pre-rebuild HNSW index in place, never refreshed to reflect the
newly replayed graph.

## Decisions

### 1. Index-DDL functions return real outcomes

`Db::create_vector_indexes` and `schema::create_fts_indexes` now propagate any error that isn't
classified as "index already exists," instead of swallowing everything. Classification is
string-matching against lbug's error text, mirroring the existing `is_missing_index_error`
precedent from ADR-0025/#146:

```
Binder exception: Index <name> already exists in table <table>.
```

matched via `s.contains("Binder exception:") && s.contains("already exists in table")`. This text
was captured empirically (a scratch integration test calling `CREATE_VECTOR_INDEX`/
`CREATE_FTS_INDEX` twice against lbug 0.17 and inspecting the resulting error), not guessed — the
same discipline ADR-0025 used for `is_missing_index_error`.

Both matchers (`is_missing_index_error`, `is_already_exists_error`) now live in `error.rs` rather
than `handlers.rs`, since `db.rs`/`schema.rs` are lower-level than `handlers.rs` and need them too
(`handlers.rs` still imports `is_missing_index_error` for the search auto-heal path).

### 2. `drop_vector_indexes` closes the HNSW staleness gap

A new `Db::drop_vector_indexes` (idempotent, void-returning, mirroring `drop_fts_indexes`) is
called alongside the existing `drop_fts_indexes` in both `handle_rebuild_from_wal` code paths
(streaming and background-job), pre-replay. This ensures the post-replay `CREATE_VECTOR_INDEX`
is always a from-scratch build over the newly replayed data, never silently short-circuited by a
stale pre-existing index.

`drop_vector_indexes` is scoped to the two rebuild-handler call sites only, not `recovery.rs`'s
WAL-corruption recovery path — that path starts from a corrupted/missing DB reopen, where HNSW
indices practically never pre-exist, so the staleness gap doesn't apply there.

### 3. `indices_built` reflects the real, current outcome — unconditionally

Both rebuild paths and `handle_build_indices` now capture the actual `Result` of
`build_indices_and_constraints()` and store it into `state.indices_built` **unconditionally** —
not "store `true` on success, leave untouched on failure." This is the change that actually
closes the silent-staleness trap: without it, a prior successful build's `true` could survive a
later failed rebuild's index build, which is the same "flag says ready, reality says not" failure
mode as the original bug, just relocated. `build_indices_once` (the auto-heal builder) already had
correct unconditional-outcome semantics and required no change — it was only ever reachable via a
genuine `Err`, and that path already worked once `build_indices_and_constraints` could return one.

Per ADR-0025/Assumption A3 (preserved, not overturned): a build failure remains **non-fatal to the
replay's own reported outcome**. A successful multi-thousand-mutation replay is not retroactively
reported as failed merely because the trailing index build failed. `success: true` in the rebuild
result continues to mean "the WAL replay succeeded"; the new `indices_built` field is the
independent signal for "and search is ready."

### 4. `indices_built` is surfaced on every relevant response

An explicit `indices_built: bool` field now appears on:

- `knowledge_rebuild_from_wal`'s result (streaming and non-streaming-dry-run paths) — **omitted**
  (not `false`) on dry-run results, since dry-run never touches indices and neither `true` nor
  `false` would accurately describe "as of this rebuild."
- `knowledge_rebuild_status`'s `result` field, for the background-job path (same omit-on-dry-run
  rule).
- `handle_build_indices`'s success response (`knowledge_build_indices` always reports `true` on
  success, since it errors out via `?` — and stores the real outcome first — on failure).
- `knowledge_status`, in both the degraded and healthy response branches, so a caller can check
  index readiness without a rebuild result in hand or a search attempt.

### 5. `recovery.rs` is explicitly out of scope

The WAL-corruption recovery path (`knowledge_recover`) already propagates
`build_indices_and_constraints()` errors fatally via `?`, unlike the rebuild handlers'
intentionally non-fatal design — it doesn't share the silent-failure problem this issue addresses.
It also never sets `indices_built = true` on success, which is a separate, pre-existing,
functionally harmless inconsistency (a subsequent search succeeds directly, since the indices
genuinely exist and never trip the auto-heal path) — flagged here as a candidate follow-up, not
fixed in this change.

## Consequences

- A rebuild's `success: true` no longer implies "search is definitely ready" — callers that want
  that guarantee check `indices_built` (on the rebuild result or via `knowledge_status`), or rely
  on the auto-heal path as the fallback safety net (unchanged, still functional).
- `create_vector_indexes`/`create_fts_indexes` can now genuinely fail. Any *new*, not-yet-observed
  lbug error text that means "benign, already done" but doesn't match `is_already_exists_error`
  would surface as a false failure — mitigated by an idempotency unit test (double-create returns
  `Ok(())` twice) as a regression guard, but this is a real precision/recall tradeoff inherent to
  string-matching an external error format, same as `is_missing_index_error` before it.
- A rebuild against a workspace with pre-existing HNSW indexes now always gets a fresh index
  reflecting the rebuilt graph, rather than potentially serving stale HNSW results forever.
- WAL-replay fidelity accounting (`mutations_replayed`, `failed_lines`, `unparseable_lines`,
  `fidelity_warning`) is unchanged — this fix touches only index-build observability/reliability.

## Related

- ADR-0025: Auto-Heal Index Build and Bulk-Load Reload Pattern — the design this fix restores
  the intended failure-observability behavior of, without changing its core shape.
- `crates/core/tests/handlers_wal_admin.rs`:
  `test_reload_builds_all_indexes` (strengthened to assert non-empty/expected search results, not
  just error-free calls — closing a coverage gap that let #192 ship undetected),
  `test_production_scale_rebuild_leaves_search_immediately_queryable` (SC-001/SC-002 at ~360
  mutations across 3 WAL files via the background-job path),
  `test_rebuild_reports_indices_built_false_on_genuine_build_failure` (SC-003 — forces a genuine
  build failure via a dropped column and asserts `indices_built: false` on both the rebuild result
  and `knowledge_status`),
  `test_interrupted_reload_auto_heals` (unchanged, confirmed still passing — FR-008).
- `crates/core/src/db.rs` / `schema.rs` `#[cfg(test)]` modules: unit tests for
  `create_vector_indexes`/`create_fts_indexes` idempotency and genuine-failure classification.
