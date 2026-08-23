# ADR-0038: In-Process NameIndex Accelerator for Entity Name Lookup

**Status**: Accepted
**Date**: 2026-07-25
**Issue**: #219 (this fix); corrects ADR-0029's residual-risk framing; field reports #202/#203,
discussion #207; amplified by #209; follow-on chain #220 (lbug 0.18.x upgrade), #221
(materialized-column drop-in replacement)

## Context

`Conn::get_entity_by_name_ci` resolves an entity by group-scoped, case-insensitive exact name
match. Its implementation issued `lower(e.name) = $lower_name AND e.group_id = $gid` in Cypher.
Because `lower(e.name)` is a scalar-function expression rather than a bare property expression,
lbug's filter push-down optimizer cannot route it through any index — every call performed a
full `Entity` table scan. This function is called from four production sites in `episode.rs`
(Phase B entity dedup, the edge-validation fallback, and commit-time endpoint resolution ×2),
making ingest cost O(edges × |Entity|) per episode — a regression against the Python `graphiti`
implementation being replaced, which never issued a name-equality query at all.

ADR-0029 (which introduced `get_entity_by_name_ci`) asserted that a `name_lower` stored column
plus a standard Kuzu `CREATE INDEX` would give O(1) lookups, deferred as a follow-up. That
remedy does not work on the current lbug 0.17.0 pin: `CREATE INDEX` on a table that already has
a primary key produces a catalog entry with no physical structure, and the index-scan rewrite
lbug's optimizer applies is PK-only. It would also not work on *any* lbug version for this
specific predicate, since no functional/expression indexes or case-insensitive collation exist —
the lowercased value would need to be materialized as a column and matched with a bare `=`
regardless of index availability. ADR-0029 is amended alongside this ADR to strike that claim.

A real fix requires either (a) a schema change (materialized `name_lower` column + a real
secondary index) that lbug 0.17.0 cannot support at all (no non-PK secondary index type exists),
or (b) an access path that doesn't route through lbug's query optimizer for the lookup itself.
Upgrading lbug is tracked separately (#220) and deliberately out of scope here — this fix must
work on the current pin.

## Decision

Replace `get_entity_by_name_ci`'s implementation with an in-process `NameIndex` accelerator
(`crates/core/src/name_index.rs`) that serves the group-scoped, case-insensitive lookup without
touching the database, verified against the database on every hit before being trusted.

### Structure and placement

`NameIndex` is an `RwLock`-guarded map keyed by `(group_id, lower(name))`, where each key holds
the full ordered set of matching entities' `(created_at, uuid)` pairs (`BTreeSet`, so the
minimum element is always the current deterministic winner). A secondary `uuid -> (group_id,
lower_name, created_at)` map lets a `created_at` change relocate an entry within its ordered set
in O(log n) without a scan. String-tuple ordering is lexicographic, and every `created_at` is
normalized to the canonical `"YYYY-MM-DD HH:MM:SS"` form via `db::canonical_created_at_for_index`
at every entry point (`insert`, `update_created_at`, `rebuild`) before it reaches the `BTreeSet`,
so this reproduces the database's `ORDER BY created_at ASC, uuid ASC LIMIT 1` winner-selection
rule exactly, not just approximately — verify-on-hit alone (below) cannot catch a
*valid-but-not-the-winner* entry (a real UUID that matches, but isn't the one the DB's `ORDER BY`
would have picked), so the index has to reproduce the ordering rule itself, not merely track "a"
candidate per key. This normalization is load-bearing, not defensive: `insert_entity`'s callers
(e.g. `episode.rs`) pass `created_at` as whatever the caller supplied — typically RFC-3339 — while
`rebuild_name_index()` always produces the space form read back from the database, so without
normalization the two formats would coexist in the same `BTreeSet` and the `'T'`/`' '` separator
byte would dominate the comparison ahead of the actual time-of-day (caught in review by
`@copilot-pull-request-reviewer`; regression-tested by
`mixed_rfc3339_and_space_format_created_at_still_orders_correctly` in `name_index.rs`).

The index lives on `Db` (`crates/core/src/db.rs`), not `Conn`: `Conn<'db>` is created fresh per
request inside `spawn_blocking` and is too short-lived to own state that must survive across
requests, while `Db` is the long-lived handle already stored in `AppState.db: ArcSwapOption<Db>`
and swapped wholesale (not mutated) on `clear_all` and every recovery path — so a fresh `Db`
naturally starts with an empty index, with no separate reset step needed. `Conn::name_index:
&'db NameIndex` is a borrow set at `Db::connect()`, mirroring the existing `lbug::Connection<'db>`
field.

### Verify-on-hit: the safety property

A lookup that finds a candidate UUID in the index always re-verifies it against the database via
the existing PK-indexed `get_entity_by_uuid` before returning it, confirming both the `group_id`
and the lowercased `name` still match. If verification fails — the UUID no longer exists, or no
longer matches this name/group — the call returns `Ok(None)`, exactly as if nothing had been
found. **There is deliberately no scan fallback on a miss or a failed verification.** This is
what bounds every failure mode of index staleness to "slower" (a spurious miss, falling through
to the embedding-based dedup path or a dropped edge, both pre-existing behaviors) rather than
"wrong" (a different entity than the one that should have matched) — and it's also what makes
the fix actually solve the regression: a "verify, then scan-fallback" design would reintroduce
the full O(edges × |Entity|) cost on every stale entry.

`get_entity_by_name_ci`'s signature, parameters, and return semantics are unchanged. All four
`episode.rs` call sites required zero modification.

## Invalidation Contract

The index is kept coherent with the database via two mechanisms:

### 1. Incremental hooks on the two typed mutation methods that can affect a name→uuid mapping

- **`Conn::insert_entity`** — the only production code path that ever creates an `Entity` node
  — calls `NameIndex::insert` after a successful `CREATE`.
- **`Conn::update_entity_created_at`** — called only from `corrections::merge_entities`, to pull
  the canonical entity's `created_at` back to the earliest value across all merged aliases —
  calls `NameIndex::update_created_at` after a successful `SET`. This is the one non-obvious
  index-affecting mutation: it doesn't add or remove a row, but it can change which entity is
  the deterministic winner for its `(group_id, lower_name)` key.

No other mutation path needs a hook. `merge_entities`, `apply_same_as`, and
`apply_entity_type_labels` only add/change labels via `update_entity_labels`, which never
touches `name` or `group_id`. No code path in the repository deletes an `Entity` node —
`remove_episode`, `remove_episodes_by_source`, and `remove_episodes_by_chunk_id` all
`DETACH DELETE` only `Episodic` nodes, confirmed by a repo-wide grep and now documented with a
cross-reference to this ADR at each call site. If a future change adds an `Entity`-deletion
path, or any other `SET e.name` / `SET e.group_id` / `SET e.created_at`, it must also update
`NameIndex` — this is the one category verify-on-hit structurally cannot catch, since a
wrong-but-still-real UUID passes verification.

### 2. Full rebuild via `Conn::rebuild_name_index()` at every path that populates Entity rows without going through the typed methods above

`WalReplayer::replay` executes raw recorded `(cypher_template, params)` pairs directly — it
never calls the typed `insert_entity`/`update_entity_created_at` Rust methods — so any
WAL-replay-based path is invisible to the incremental hooks. `Conn::rebuild_name_index()` (one
`MATCH (e:Entity) RETURN uuid, name, group_id, created_at` scan, replacing the index's entire
state) closes this gap, and is called at every site that can populate `Entity` rows this way:

- **Startup** (`crates/service/src/main.rs`), immediately after the eager
  `build_indices_and_constraints()` call, fatal via `?` — same posture as its neighbor.
- **`Db::open_or_rebuild`** (`crates/core/src/db.rs`), after `WalReplayer::replay`.
- **`run_full_recovery_sequence`** (`crates/core/src/recovery.rs`), after
  `build_indices_and_constraints()` — covers both startup self-recovery and
  `knowledge_recover_full`.
- **`handle_clear_all`** (`crates/core/src/handlers.rs`), after `init_schema` on the fresh empty
  table — a no-op today, kept for uniformity with every other Entity-population path rather than
  as a special-cased exception.
- **Both `!dry_run` branches of `knowledge_rebuild_from_wal`** (streaming and background job,
  `handlers.rs`), alongside `build_indices_and_constraints()`, non-fatal (`eprintln!`-logged) —
  matching the existing posture for index-build failures at those sites.
- **All three `recover_*` degraded-mode recovery strategies** (`recover_drop_lbug_wal`,
  `recover_rebuild_from_workspace_wal`, `recover_restore_from_backup`, `handlers.rs`) — each
  reopens or restores a DB file that may already contain entities the current process never
  inserted via the typed methods.

Rebuild is eager everywhere, including on an empty post-`clear_all` table: a full `Entity` scan
is negligible next to the HNSW/FTS build it already runs alongside, and eager avoids reopening a
"must the caller fall back to scan or block until built" question the existing lazy
`indices_built` pattern has to answer for HNSW/FTS.

## Consequences

### Positive

- Entity name lookup during ingest (dedup and edge-endpoint resolution) no longer scans the
  `Entity` table — confirmed by the `name_lookup_scan_baseline_10k` vs. `name_lookup_indexed_10k`
  benchmarks in `crates/core/benches/name_lookup.rs` (SC-001/SC-002/FR-011).
- A stale or missing index entry can only ever produce a miss, never a wrong entity — exercised
  directly by `crates/core/tests/name_index_coherence.rs`'s
  `stale_index_entry_after_out_of_band_delete_degrades_to_miss` test (SC-004).
- `get_entity_by_name_ci`'s signature is unchanged, so #221 (the planned materialized-column +
  ART-index replacement, once #220's lbug upgrade lands) can swap this implementation in as a
  drop-in without touching any call site.

### Negative / Residual risks

- **Missed invalidation site risk remains structural**: any future mutation that changes
  `name`/`group_id`/`created_at` on an existing `Entity` row without going through
  `insert_entity`/`update_entity_created_at` (or a corresponding `rebuild_name_index()` call)
  would silently desync the index. Verify-on-hit does not catch a valid-but-wrong-winner case,
  only a fully-stale one. Reviewers of future changes touching `Entity` mutation paths should
  check whether `NameIndex` needs a new hook.
- **`rebuild_name_index()` failing at a non-fatal call site** (the two `knowledge_rebuild_from_wal`
  branches) leaves the index at its pre-rebuild state — safe (degrades to misses, never wrong
  answers) but could look like an unexplained ingest slowdown until diagnosed from logs.
- **This is explicitly an interim fix**, not the long-term design. It exists because lbug 0.17.0
  cannot support any form of indexed access for this predicate. #220/#221 track the eventual
  replacement with a materialized `name_lower` column and a real secondary index once lbug is
  upgraded.
- **The pre-existing TOCTOU race documented in ADR-0029** (Phase B/edge-validation reads run
  without the write lock) is unchanged by this fix — not worsened, not closed. Out of scope per
  the issue's spec.

## Alternatives Considered

### Materialized `name_lower` column + standard Kuzu `CREATE INDEX`

Rejected for the reason ADR-0029 is amended to state: doesn't work on lbug 0.17.0 (PK-only
index-scan rewrite) and wouldn't work on any version for a `lower()` predicate specifically
(no functional/expression indexes). This is the eventual right answer once lbug is upgraded
(#220/#221), not something achievable today.

### Scan-fallback on a stale/missing index entry

Rejected: reintroduces the exact O(edges × |Entity|) cost this fix exists to remove, on every
cache miss. The whole point of an accelerator-with-verify design is that a miss is cheap.

**Narrowed by ADR-0283** (#283): this rejection held uniformly across all four call sites, but
didn't distinguish the two endpoint-authority sites (#218/#209's "does this entity exist
anywhere in the group" check) from the two per-entity/per-edge accelerator sites this ADR was
actually written to fix. ADR-0283 adds a bounded, self-healing scan fallback scoped only to the
former — see that ADR for why the distinction matters and how it stays bounded.

### `Arc<NameIndex>` clone per `Conn::connect()`

Rejected in favor of a `&'db NameIndex` borrow: unnecessary atomic-refcount overhead per request
when a borrow works and involves no extra lifetime machinery, given `Conn<'db>` already borrows
`Db` for its `lbug::Connection<'db>` field.

### Defensive invalidation hooks at `remove_episode`/`remove_episodes_by_source`/`remove_episodes_by_chunk_id`

Rejected as dead code: no path in the repository deletes an `Entity` node today. Documented
instead with a doc-comment cross-reference to this ADR at each call site, so a future
contributor who *does* add entity deletion knows what else needs updating.

## Related

- ADR-0029: Name-First Entity Resolution in add_episode Phase B — introduced
  `get_entity_by_name_ci`; amended alongside this ADR to strike the disproven property-index
  remedy.
- `crates/core/src/name_index.rs`: `NameIndex` implementation and unit tests.
- `crates/core/tests/name_index_coherence.rs`: FR-005/FR-008 coherence tests across every
  mutation path (insert, merge-driven reorder, label-only corrections, episode deletion,
  WAL replay, `open_or_rebuild`) and the stale-entry-degrades-to-miss property.
- `crates/core/benches/name_lookup.rs`: `bench_name_lookup_scan_baseline_10k` /
  `bench_name_lookup_indexed_10k` — before/after measurement (FR-011/SC-002).
- **#220**: lbug 0.18.x upgrade, which adds non-PK secondary ART indexes.
- **#221**: follow-on issue tracking the drop-in replacement of this fix's `NameIndex` with a
  materialized column + ART index, once #220 lands.

## Amendment (2026-08-16, issue #398)

The lbug pin moved from `0.17.0` to `0.19.1`, so the constraint this ADR is built on no longer
holds at the engine level. Read "the current lbug 0.17.0 pin" (Context) and "lbug 0.17.0 cannot
support at all (no non-PK secondary index type exists)" as statements about 0.17.0, which was
current when this ADR was written. 0.18.0 added secondary ART indexes upstream
(`LadybugDB/ladybug#582`).

**This does not change the decision, and `NameIndex` is still the live implementation.** #398
bumped the dependency and adopted none of the new capabilities — the "Related" note above
anticipated exactly this sequencing. #220 was superseded by #398 rather than implemented; the
drop-in replacement stays tracked by **#221**, whose blocker is now cleared.
