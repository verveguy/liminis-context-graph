# ADR-0221: Secondary ART Index for Entity Name Lookup (Replaces In-Process NameIndex)

**Status**: Accepted
**Date**: 2026-08-23
**Issue**: #221; supersedes ADR-0038 (issue #219) and its narrowing amendment ADR-0283
(issue #283); enabled by #190 (lbug 0.19.1 upgrade, closed), building on `LadybugDB/ladybug#582`
(non-PK secondary ART indexes, landed in lbug 0.18.0)

## Context

`Conn::get_entity_by_name_ci` answers a group-scoped, case-insensitive exact name match. Since
ADR-0038, it has been served by `NameIndex`, an in-process `HashMap<(group_id, name_lower),
BTreeSet<(created_at, uuid)>>` accelerator, because lbug 0.17.0 had no secondary index type for a
non-PK column and `lower(e.name) = $x` is a scalar-function predicate lbug's optimizer can never
route through an index on any version. That design worked, but it carries costs the database
should be bearing instead:

- **Invalidation surface.** The map must be updated on every mutation path that can change a
  name→uuid mapping. ADR-0038 enumerated four hook sites; by the time this issue was specced, the
  surface had grown further — #378 made WAL streams per-group, #385 fixed cross-group mutation
  attribution, and #361 added `knowledge_delete_by_group` as a new `Entity`-deleting path (the
  one *deliberate* exception ADR-0038's design required manual invalidation for). A missed site
  risks silently corrupting dedup.
- **Startup cost.** A full `Entity` table scan to rebuild the map on every service start and
  after every WAL rebuild/recovery strategy.
- **Memory.** Proportional to entity count — modest today, unbounded as graphs grow.
- **Duplicated state.** An index of the database, maintained by hand outside the database.

lbug 0.18.0 (`LadybugDB/ladybug#582`) added non-PK secondary ART indexes: physical creation on a
table that already has a primary key, planner push-down via `popSecondaryARTEqualityComparison`,
non-unique secondary leaves via `lookupAll`, and WAL-logged index builds. The upgrade that
delivers this to the crate (originally #220, retargeted and closed as #190 to also pick up
0.19.0's checkpoint lock-file and read-only-open fixes) landed with `Cargo.toml` pinned to
`lbug = "=0.19.1"`. ADR-0038 already named this issue as its own planned successor once that
blocker cleared.

## Decision

Replace `NameIndex` with a materialized `Entity.lookup_key` column — the composite key
`group_id + '\x1f' + lower(name)` (`\x1f`, the ASCII unit separator, chosen as a delimiter
vanishingly unlikely to appear in an LLM-extracted entity name or an operator-supplied
`group_id`; this is a documented collision assumption, not a mechanical guarantee) — plus a
secondary `CREATE ART INDEX` on that column. `get_entity_by_name_ci` becomes a single
`WHERE e.lookup_key = $key ORDER BY e.created_at ASC, e.uuid ASC LIMIT 1` query, with `$key`
always computed host-side in Rust before the query runs, never via Cypher `lower()` — reusing
`lower()` in Cypher would silently defeat the index push-down this change exists to gain, the
same reason the old predicate was unindexable in the first place.

### `compute_lookup_key`: one Rust function, called everywhere

`db::compute_lookup_key(group_id, name) -> String` is the sole place the key is computed —
by every writer (`insert_entity`'s `CREATE`, `update_entity_core`'s `SET`) and by the one-shot
migration backfill (`schema::backfill_entity_lookup_keys`) alike. This is a deliberate
correctness choice, not just DRY: `str::to_lowercase()` performs full Unicode case-folding, and
there is no guarantee lbug's Cypher `lower()` folds identically for non-ASCII names. Computing
the backfill's keys via a bulk Cypher `SET ... = e.group_id + $sep + lower(e.name)` statement
would be faster, but risks a non-ASCII entity name silently getting two different `lookup_key`
values depending on whether it was backfilled or live-written — a latent, hard-to-detect miss.
The one-shot backfill's per-row Rust round trip is an acceptable, deliberate trade for guaranteed
consistency, given it already runs as a blocking, non-trivial-duration migration step (see below).

### No more "candidates + verify" — the database is the sole source of truth

`NameIndex`'s design required every hit to be re-verified against the database via
`get_entity_by_uuid` before being trusted, since the in-process copy could be stale. An ART
index is a live database structure, not a copy: a query against it cannot return a row that
doesn't currently exist with that key, so `get_entity_by_name_ci` is now a single indexed query,
full stop. This removes an entire layer of complexity the old design needed, not just swaps its
backing store.

### No invalidation hooks anywhere

Secondary ART leaves are non-unique and lbug maintains them automatically across
insert/update/delete/checkpoint/reload. Concretely, this means:

- `insert_entity`'s `CREATE` and `update_entity_core`'s `SET` write `lookup_key` in the same
  statement that writes `name`/`group_id` — no separate hook call.
- `crate::group_purge`'s `DETACH DELETE` of `Entity` rows — ADR-0038's *sole documented
  exception* requiring manual invalidation — now needs no invalidation step at all: the ART
  index simply no longer has an entry for a deleted row, maintained by lbug itself.
- No code path needs updating when a *new* `Entity`-deleting or `Entity`-mutating path is added,
  the structural risk ADR-0038's Consequences section flagged as its main residual risk.

### The persisted-staleness problem and its resolution (FR-010/FR-011)

`NameIndex` was process-local and rebuilt at startup and after every WAL rebuild, so any
staleness from an out-of-band write (raw Cypher via the `cypher` MCP scope, or a second process
writing the DB file directly) was bounded by process lifetime. `lookup_key` is a **persisted**
column: a row written out-of-band stays wrong across restarts and rebuilds, indefinitely, until
something recomputes it. This makes the new design *more* exposed to this failure class than the
one it replaces, not less — a trade-off worth stating plainly rather than inheriting unexamined.

Mitigation follows the doctrine ADR-0038 and ADR-0283 already established: mitigate exactly where
a stale/missing `lookup_key` produces a **wrong** answer, not merely a **slow** one.

- **Three call sites where a miss only degrades performance** (`episode.rs` Phase B dedup, and
  Phase C's two per-edge lookups): "unfindable by name until recomputed" is an accepted,
  documented limitation. No mitigation.
- **The one authority call site** (`episode.rs`'s Phase C `get_entity_by_name_ci_with_scan_fallback`
  — ADR-0283's Site 1, "does this entity exist anywhere in the group"): a miss here is wrong, not
  slow, so an equivalent guarantee to ADR-0283's bounded scan fallback is preserved. On an
  indexed miss, this falls back to `scan_entity_by_name_ci`, which resolves through `Merged`
  tombstones exactly as before (see below) — and on a scan hit, self-heals by writing
  `SET e.lookup_key = $key` on that row so every subsequent lookup, at any call site, hits the
  index directly. Because `lookup_key` is derived purely from `group_id` and `name` (both already
  in hand at the point of the scan hit), this is a cheap, targeted correction, not a full scan —
  and it is what turns "permanently wrong" back into "self-healing" for the specific rows that
  pass through this call site.

  **This fallback is not a cold path** — it fires for every name an extractor mentions but never
  emitted as an entity, which is routine LLM output, not an edge case. The PR's human review
  caught that `scan_entity_by_name_ci`'s `RETURN` list originally still carried
  `name_embedding`/`summary`/`attributes`, so every fallback scan transferred and sorted *every
  row in the group*, vectors included, to select at most one. Fixed by narrowing the `RETURN` to
  the scalar columns needed to find the winner (`uuid`, `name`, `group_id`, `created_at`) and
  hydrating only the single winning row via `get_entity_by_uuid` — one extra point lookup on a
  hit, nothing on the (common) miss. Measured on a 10k-entity, 768-dim (bge-base-en-v1.5) graph:
  ~275ms → ~26ms (hit, self-heal path included), ~269ms → ~7ms (miss) — see
  `crates/core/benches/name_lookup.rs`'s `bench_name_lookup_scan_fallback_10k`.

### Resolution semantics preserved exactly (FR-007)

`scan_entity_by_name_ci` deliberately applies no label filter — a `Merged`-tombstoned row that
would win `ORDER BY created_at ASC, uuid ASC` is still returned as the winner, matching
`NameIndex`'s own pass-through behavior (ADR-0283). The new indexed query inherits this by
construction: it is the same `ORDER BY ... LIMIT 1` shape with no label predicate. Cross-group
pointer resolution (#369) depends on this exact behavior; nothing about the index swap changes it.

### `knowledge_status`'s `name_index_trusted`/`name_index_fallback_scans` (FR-012)

Both JSON field names stay on the wire — no IPC/Python-side break — but are re-backed by a new
`Db`-level `LookupKeyStatus { migrated: AtomicBool, fallback_scans: AtomicU64 }`, replacing
`NameIndex` in the same borrow shape (`Conn`'s `&'db LookupKeyStatus`, mirroring the old `&'db
NameIndex`). Semantics narrow and repoint, deliberately:

- `name_index_trusted` now means "the one-shot `lookup_key` backfill migration
  (`schema::migrate`) completed without error" — an accurate migration-health signal, computed
  once, not a per-write trust flag with nothing left for it to track (the database is the source
  of truth now; there's no separate copy to distrust).
- `name_index_fallback_scans` keeps its exact prior meaning: every time the Phase C authority
  site fell back past the indexed lookup. Its meaning is arguably sharper under this design — a
  nonzero count now specifically signals out-of-band write staleness at that site, rather than
  routine in-process-index churn.

### Migration (FR-005/FR-006, User Story 2)

Follows the exact shape ADR-0470 (`summary_embedding`) established: `schema::migrate` probes for
`Entity.lookup_key` (a zero-row property read; a Binder exception means absent), and on a
pre-existing workspace, `ALTER TABLE Entity ADD lookup_key STRING` followed immediately by
`schema::backfill_entity_lookup_keys` — a `MATCH (e:Entity) WHERE e.lookup_key IS NULL` scan,
computing and `SET`-ing each row's key in Rust. The same backfill function is called at every
WAL-rebuild/recovery site (`Db::open_or_rebuild`, both `knowledge_rebuild_from_wal` arms, and the
degraded-mode recovery strategies) after replay, since `WalReplayer::replay` executes raw
recorded Cypher verbatim and never calls `insert_entity` — mirroring `zero_fill_null_entity_
summary_embeddings`'s dual-call-site shape exactly. `dump.rs`'s `ENTITY_CYPHER` WAL-dump template
is deliberately left unchanged (matching the `summary_embedding` precedent, which never touched
it either): a dump→replay round trip produces `lookup_key: NULL` rows, healed by the same
backfill pass every rebuild site already needs.

`CREATE ART INDEX entity_lookup_key_idx FOR (e:Entity) ON (e.lookup_key)` runs from
`Db::build_indices_and_constraints`, after the backfill, mirroring `create_entity_summary_
embedding_index`'s idempotent-creation shape. An explicit `ART` type is required — `CREATE INDEX`
with no type is rejected for a non-PK column ("indexes are currently supported only on node
primary keys").

**A repeat `CREATE ART INDEX` does not use `is_already_exists_error`'s wording.** The Plan stage
flagged this as unverified, and the verification found a genuine mismatch: `CREATE_VECTOR_INDEX`/
`CREATE_FTS_INDEX` report a conflict as `"<name> already exists in table <table>"`, but a repeat
`CREATE ART INDEX` reports `"entity_lookup_key_idx already exists in catalog"` — the same
`"already exists in catalog"` wording lbug uses for a duplicate *node table* creation (e.g.
`"Entity already exists in catalog"`), which `is_already_exists_error` deliberately does **not**
match, specifically so a genuine duplicate-table failure still propagates. Blindly widening
`is_already_exists_error` to also match `"in catalog"` would have silently swallowed that case
too. The fix is `error::is_named_catalog_entry_already_exists_error(err, name)`, which only
classifies as already-exists when the *specific named entry* in the message matches — safe
because the ART index's explicit, non-default name (FR-002) can never collide with a table name.
`create_entity_lookup_key_index` checks both classifiers.

**This is a genuinely blocking, non-trivial-duration one-shot step for a large existing graph**,
not a fast/no-op upgrade path. `CREATE ART INDEX` writes the full serialized ART tree into lbug's
own internal WAL as a single record for indexes under a 256 MiB threshold
(`LBUG_CREATE_INDEX_WAL_THRESHOLD`); above that threshold it switches to a blocking checkpoint
instead. Either way, the build is synchronous. Migration duration at realistic production scale
is unmeasured by this issue — flagged here rather than assumed low-risk.

### Retrying a failed backfill without an O(N) scan on every clean startup

The Review stage's dependency-injection review and the PR's human review both caught the same
gap: `migrate`'s original design decides whether to run the `ALTER`+backfill purely from whether
`Entity.lookup_key` *exists as a column*. If `ALTER` succeeds but the backfill that follows it
then fails, the column is already present on the next open — the probe succeeds, the whole `if`
block is skipped, and the backfill never retries. Worse, `LookupKeyStatus::migrated` is an
in-process `AtomicBool` that resets to its `true` default on every fresh `Db`, so a restart after
a failed backfill silently makes `knowledge_status` report `name_index_trusted: true` while rows
are still missing `lookup_key`. That is a real dedup-corruption path for the three FR-011 sites
(Phase B dedup, Phase C's two per-edge lookups), not merely stale observability — those sites have
no scan fallback by design (FR-011), so a `NULL`/missing `lookup_key` there creates a duplicate
entity rather than degrading to a slower lookup.

The constraint on the fix was firm: it must not reintroduce an O(N) `Entity` scan on the clean
(already-migrated) startup path — eliminating that scan is a large part of why this issue exists.
A `WHERE lookup_key IS NULL … LIMIT 1` probe on every open would scan the whole table on a healthy
database just to confirm there's nothing to do.

The fix adds a minimal, generic migration-state table, `SchemaState (key STRING PRIMARY KEY,
status STRING)`, and persists the backfill's outcome there under the key
`entity_lookup_key_backfill` instead of re-deriving it from column presence:

- **Column absent** (pre-#221 database): unchanged — `ALTER`, then backfill, then record
  `"complete"` or `"failed"`.
- **Column present, marker `"complete"`**: nothing to do. One `SchemaState` point lookup by
  primary key — O(1), not a scan. This is the steady-state path every healthy restart takes.
- **Column present, marker `"failed"`**: retry the backfill (still bounded to `WHERE lookup_key
  IS NULL`, so only the still-`NULL` rows are touched) instead of trusting a stale success signal
  forever.
- **Column present, no marker**: either a genuinely fresh database (its `Entity` table is empty,
  so the backfill's scan costs nothing) or a database migrated by a pre-`SchemaState` build of
  this feature. Either way the backfill runs once — free for the fresh-DB case, a one-time cost
  for the pre-marker case, paid on exactly one startup and never again once the marker is
  written.

`CREATE NODE TABLE IF NOT EXISTS SchemaState` is called unconditionally at the top of every
`migrate()` run — a catalog check, not a scan, so it costs nothing to make idempotent on every
open regardless of database age. `SchemaState` is itself a Rust-only bookkeeping table (like
`WalPosition`), not something graphiti's `kuzu_driver.py` needs to mirror — it exists purely to
give this migration a persisted memory, not as domain data.

## Consequences

### Positive

- No invalidation surface anywhere: `NameIndex`'s entire hook-and-rebuild contract (ADR-0038's
  "Invalidation Contract" section) is gone, not replaced by a smaller one.
- No startup scan: the database's own catalog carries the index across restarts; nothing needs
  rebuilding.
- No duplicated, unbounded-with-graph-size in-process memory.
- `get_entity_by_name_ci`'s signature and every call site's behavior are unchanged (FR-003/FR-008)
  — a drop-in swap, confirmed by the same three `episode.rs`/`corrections.rs` call sites this ADR
  audited requiring zero modification.
- Simpler implementation: no "candidates + verify" loop, no `BTreeSet` winner-ordering
  reproduction, no timestamp-format normalization (`canonical_created_at_for_index`, deleted
  along with `NameIndex`) — the database's own `ORDER BY` does this natively.

### Negative / Residual risks

- **Persisted staleness is a strictly worse failure mode than `NameIndex`'s** for the three
  degrade-to-slower call sites: a `NameIndex` miss self-healed on the next restart or rebuild; a
  `lookup_key` miss from an out-of-band write does not, until something recomputes it. Accepted
  per FR-011 as a documented limitation, deliberately not mitigated — see Context.
- **Migration duration at scale is unmeasured.** The Edge Cases section of the spec already
  assumes a blocking, non-trivial migration; this ADR does not resolve that uncertainty, only
  documents it.
- **The `\x1f` delimiter's collision-freedom is an assumption, not a guarantee.** Two distinct
  `(group_id, name)` pairs must never produce the same composite key. Vanishingly unlikely for
  LLM-extracted names or operator-chosen group IDs, but not mechanically enforced.
- **`CREATE ART INDEX`'s conflict wording genuinely differs from `CREATE_VECTOR_INDEX`/
  `CREATE_FTS_INDEX`'s**, as the Plan stage anticipated might be the case — see the dedicated
  paragraph under Decision above. Caught by `lookup_key_index_ddl_tests::double_create_is_
  idempotent` (`db.rs`) before it could make every non-fresh-DB startup after the first fail
  fatally.

## Alternatives Considered

### Keep `NameIndex`, close this issue without merging

The spec's own Assumptions section names this as the correct outcome if SC-002 (ART index at
least as fast as `NameIndex` on a representative graph) fails to hold. Measured via
`crates/core/benches/name_lookup.rs` on a 10k-entity graph (`cargo bench -p lcg-core --bench
name_lookup`, quick mode): the ART-indexed path (`bench_name_lookup_art_indexed_10k`) runs a hit
in ~0.72 ms and a miss in ~0.62 ms, against the unindexed-scan baseline
(`bench_name_lookup_scan_baseline_10k`)'s ~1.74 ms and ~1.75 ms respectively — roughly 2.4-2.8×
faster than a full table scan, and well within the sub-millisecond range `NameIndex`'s in-process
map delivered (its own benchmark numbers are no longer directly reproducible now that `NameIndex`
is deleted, but a single indexed database round trip landing under a millisecond is not a
regression against an in-process `HashMap` lookup plus one PK-indexed verify read). SC-002 holds,
so this alternative wasn't taken.

### Trust-flag-driven "degrade the whole site to scan" for FR-010, instead of scoped self-heal

Rejected in favor of the narrower per-row self-heal: only the specific missed row pays the scan
cost, not every call at the authority site after a single staleness event. `scan_entity_by_name_ci`
already correctly implements `Merged`-tombstone pass-through, so reusing it verbatim for the
fallback costs nothing extra to preserve FR-007 at this call site.

### Cypher-side `lower()` for the migration backfill

Rejected: risks Unicode-casing drift against every live writer's Rust-side `to_lowercase()` for
non-ASCII names, a latent correctness gap with no test that would reliably catch it. See
`compute_lookup_key` above.

## Related

- ADR-0038: In-Process `NameIndex` Accelerator for Entity Name Lookup — the design this ADR
  replaces; already named this issue as its own planned successor.
- ADR-0283: Bounded Scan Fallback and Trust State for `NameIndex` Endpoint Resolution — the
  authority-site (Site 1) design this ADR re-points at `lookup_key`, preserving the same guarantee.
- ADR-0470: Entity Summary Embedding for Semantic Search — the direct precedent for this ADR's
  migration mechanics (probe→ALTER→backfill, idempotent index creation, post-replay backfill at
  every rebuild/recovery site).
- `crates/core/src/db.rs`: `compute_lookup_key`, `LookupKeyStatus`, `create_entity_lookup_key_index`,
  `get_entity_by_name_ci`/`get_entity_by_name_ci_with_scan_fallback`/`scan_entity_by_name_ci`.
- `crates/core/src/schema.rs`: `migrate`'s `lookup_key` probe/ALTER/backfill block,
  `backfill_entity_lookup_keys`.
- `crates/core/tests/lookup_key_index.rs`: coherence tests, superseding
  `name_index_coherence.rs` (deleted).
- `crates/core/benches/name_lookup.rs`: `bench_name_lookup_scan_baseline_10k` /
  `bench_name_lookup_art_indexed_10k` — SC-002's before/after measurement.
- **#190**: lbug 0.19.1 upgrade (closed) — cleared this issue's blocker.
- **#369**: Resolvable cross-group semantic pointers — depends on the tombstone pass-through
  behavior this ADR preserves exactly.
