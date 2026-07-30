# ADR-0283: Bounded Scan Fallback and Trust State for NameIndex Endpoint Resolution

**Status**: Accepted
**Date**: 2026-07-30
**Issue**: #283 (this fix); narrows ADR-0038's "no scan fallback" stance for two of its four
call sites; corrects a weakening ADR-0038 introduced in #219's `NameIndex` accelerator to
#218/#209's persisted-entity edge-endpoint fallback

## Context

ADR-0038 replaced `Conn::get_entity_by_name_ci`'s full-table-scan implementation with an
in-process `NameIndex` accelerator, verified against the database on every hit, with
**deliberately no scan fallback on a miss**. That trade-off is correct for the function's three
per-entity/per-edge call sites (Phase B dedup, Phase C's two per-edge lookups): a miss there
degrades to "slower" (an extra embedding comparison, or a dropped edge that would have been
dropped anyway if the entity genuinely doesn't exist), never "wrong."

It is not correct for the fourth call site, `episode.rs`'s Site 1 "global fallback" —
introduced by #218/#209 specifically to answer "does this entity exist anywhere in the
group," the authority question the O(edges × |Entity|) removal in #219 quietly downgraded to
"does the in-process index currently know about it." Those are different questions. A
`NameIndex` miss at this call site can happen for reasons that have nothing to do with whether
the entity exists:

- **Raw Cypher writes** through `handle_query_cypher` (`crates/core/src/handlers.rs`) — the MCP
  `cypher` scope's arbitrary-query escape hatch never touched `NameIndex`.
- **WAL replay whose post-replay `rebuild_name_index()` failed** — both
  `knowledge_rebuild_from_wal` branches treated that failure as non-fatal and only logged it.
- **A second process writing the same database file** (eval harness, migration, a CLI) — the
  running service's in-memory index never learns.
- **Verify-on-hit failure with no second chance** — `NameIndex::lookup` returned only the
  `BTreeSet` minimum; if that UUID failed verification, the call returned `None` without trying
  the other same-named candidates, even when one of them would have verified successfully.

None of these are exotic: `handle_query_cypher` is a documented, reachable escape hatch, and
WAL replay is the primary recovery path (ADR-0009/ADR-0026). ADR-0038's own "Alternatives
Considered" section rejected a scan fallback uniformly across all four call sites — this ADR
narrows that rejection, rather than reversing it, to the two call sites where "the entity might
exist but the index doesn't know it" is a correctness bug, not a performance question.

Investigation, resolved-open-question, and design-decision provenance for the narrowing below
lives in issue #283's Research and Plan stage comments, not duplicated here.

## Decision

### A separately-named method, not a change to `get_entity_by_name_ci` itself

`get_entity_by_name_ci_with_scan_fallback` (`crates/core/src/db.rs`) is the only place a scan
fallback exists. `get_entity_by_name_ci` itself keeps ADR-0038's exact shape — only extended per
FR-005 below — and Phase B's per-entity dedup call (`episode.rs`, line ~104) is untouched.
Embedding the fallback inside the shared function would have applied it uniformly to all four
call sites, reintroducing the O(edges × |Entity|) cost ADR-0038 removed; keeping it a distinct,
explicitly-named method makes the risk surface visible at each call site instead.

`get_entity_by_name_ci_with_scan_fallback` is used at Phase C, commit-time endpoint resolution
— the sole remaining DB-backed endpoint-authority call site in `episode.rs` (per-edge, but see
the self-heal bound below).

At the time this ADR was originally drafted, a second call site existed pre-lock — "Site 1", a
deduplicated global fallback that resolved a batch's off-list edge endpoints against the
persisted graph before Phase C ran, introduced by #218/#209. #281 (landed on `main` while this
issue was paused behind it, see Context) removed that DB-backed Site 1 entirely, replacing it
with a DB-independent embedding-similarity *salvage* step: an off-list endpoint name is
cosine-matched against the batch's own entity name embeddings and rewritten in place on a
match, with no database access and no `NameIndex` involvement. Anything that doesn't
salvage-match is passed through unresolved to Phase C, which is now the sole point that
resolves an endpoint against the persisted graph or finally drops the edge. This ADR's
implementation was rebased onto that restructuring; the "Site 1 self-heals ahead of Phase C"
bound described in the original design no longer applies, since there is no longer an earlier
DB-backed pass to do the warming. See the next section for the bound as it actually holds today.

### Self-healing bounds Phase C from within its own per-edge loop

`get_entity_by_name_ci_with_scan_fallback` inserts a scan hit into `NameIndex` before returning
it (self-healing). Because Phase C resolves every edge's endpoints in a single loop within one
`spawn_blocking` closure, the *first* edge in a batch naming a given missing entity triggers the
scan and warms the index; every subsequent edge in the same Phase C pass referencing that same
name lands on a plain index hit, not a fresh scan. A batch with many edges to one recurring hub
entity still costs exactly one scan for that name, not one per edge — satisfying FR-002's bound
without needing a separate deduplicated pre-pass or a name→UUID map threaded in from elsewhere.

Self-healing alone only bounds the *hit* case — there's nothing to insert into `NameIndex` for a
name that genuinely doesn't exist. A batch with several edges naming the same nonexistent entity
(a hallucinated extraction, a typo) would otherwise pay a full scan per edge, since each edge
resolves independently and none of them find anything to cache into `NameIndex`. Phase C's loop
therefore also keeps its own local `HashMap<normalized name, Option<uuid>>` memo, populated on
both outcomes, scoped to that single pass — a scan hit is still self-healed into `NameIndex` for
future requests, while a miss is remembered only for the remainder of this batch (there being
nothing durable to persist for an entity that isn't there). Together this bounds the loop to at
most one scan per unique unresolved name, hit or miss, matching FR-002's dedup requirement
without needing `episode.rs`'s standalone `missing_names` shape from the pre-#281 design.

Threading a resolved name→UUID map forward from an earlier pass (as the original Site 1-based
design would have required) was considered and rejected: it would replace Phase C's live
re-verification (a real `MATCH` at commit time) with a cached UUID from earlier in the request,
silently reintroducing the risk of inserting an edge against a since-deleted entity without any
of the existing "dropping edge" diagnostics. Self-healing through the index itself avoids this:
every resolution at Phase C, whether an index hit or a scan hit, still goes through the same
verify-on-hit/scan-hit path immediately before use.

### `NameIndex::lookup` becomes `lookup_candidates`: fall through, don't give up (FR-005)

`lookup` returned only the `BTreeSet`'s minimum element (the deterministic winner). Renamed to
`lookup_candidates`, it now returns every same-named UUID in winner-first order.
`get_entity_by_name_ci` iterates this list, verifying each candidate against the database until
one passes or the list is exhausted — so a winner that fails verify-on-hit (e.g. deleted
out-of-band) falls through to the next-best same-named candidate instead of degrading straight
to a miss. This is a small, self-contained change inside `NameIndex`/`get_entity_by_name_ci`;
no call site needed to change.

### `NameIndex::insert` upserts instead of appending, so self-healing can't leave stale candidates

Before this issue, `insert` was only ever called for a UUID that had never been indexed before
(`insert_entity`, for a brand-new entity) — a genuinely new `(created_at, uuid)` tuple. The
self-heal path this issue adds is the first caller that can call `insert` for a UUID that *was*
already tracked, just under a different key: a scan hit for an entity that was renamed or moved
groups out-of-band (bypassing the index) resolves under its *current* name, and self-healing
that result would otherwise leave the *old* `(group_id, lower_name)` key's stale entry for the
same UUID sitting in `by_key` forever, accumulating one extra stale/duplicate candidate per
missed rename. `insert` now checks `by_uuid` for a prior entry first and removes it from its old
key's set before inserting under the new key — the same reconciliation `update_created_at`
already did for the narrower "same key, new timestamp" case, generalized to "any key change."
This was accepted as a real (if narrow) gap rather than dismissed: FR-005's fall-through already
prevented it from returning a *wrong* entity, but left an unbounded, ever-growing set of stale
candidates behind every self-heal after a rename, which is worth closing directly rather than
relying on FR-005 to keep masking it.

### `resolve_via_scan`'s per-batch memo keys on the DB layer's own normalization, not `normalize_name`

The Phase C memo (previous section) originally keyed its cache by `prompts::normalize_name`
(control-char strip + trim + lowercase) — the same key `name_to_uuid` uses for batch-local
matches. But `get_entity_by_name_ci`/`scan_entity_by_name_ci` only ever match by
`trim().to_lowercase()`, without stripping control characters. Keying the memo by the stricter
`normalize_name` would conflate two names the DB layer treats as distinct — e.g. `"Apple"` and
`"A\u{0001}pple"` — letting a cached resolution (or cached miss) for one silently serve the
other. The memo now keys on `raw_name.trim().to_lowercase()`, matching the DB layer's own
normalization exactly.

### Trust state and fallback-scan counter live on `NameIndex`/`Db`, not `AppState` (FR-003, SC-004)

`NameIndex` gains an `AtomicBool` trust flag (default `true`) and an `AtomicU64` fallback-scan
counter. `rebuild()` unconditionally sets `trusted = true` on completion; `mark_untrusted()` is
called wherever a rebuild is attempted and known to have failed. Placing this state on
`NameIndex` (which lives on `Db`, per ADR-0038) rather than on `AppState` (the layer
`AppState.indices_built: Arc<AtomicBool>` uses for the analogous HNSW/FTS-build flag) matters
because `Db` is swapped wholesale, not mutated, on `clear_all`/recovery: a fresh `Db` gets a
fresh, vacuously-trusted `NameIndex` with no separate reset step. An `AppState`-level flag would
need every `Db`-swap site to remember to reset it, and a missed reset would leave a stale
"untrusted" mark attached to a brand-new, empty (and therefore trivially coherent) index.

`mark_untrusted()` is called from three places:

- Both non-fatal `rebuild_name_index()` failure arms inside `knowledge_rebuild_from_wal`
  (streaming and background-job branches, `handlers.rs`) — alongside their existing log line.
- `handle_query_cypher`, when its post-mutation rebuild (below) fails.

The trust flag is deliberately **not** a gate the scan fallback itself consults — the fallback
already always scans on a total index miss, trusted or not. It exists purely as an operator-
facing signal (surfaced via `knowledge_status`, SC-004) that something is degrading lookups,
independent of whether any particular lookup happened to need the fallback yet.

### `handle_query_cypher` rebuilds the index for mutation-shaped raw Cypher (FR-004)

Detecting "was this raw-Cypher statement an entity mutation" reliably is out of scope per issue
#170 FR-008 (raw Cypher gets no parsing/coercion). Rather than inventing new detection or paying
an unconditional rebuild on every call (including read-only administrative queries), this reuses
`WalWriter::log_mutation`'s existing mutation-keyword heuristic, extracted into a shared
`wal::looks_like_mutation()`: scan all tokens outside single-quoted literals for
`CREATE`/`MERGE`/`SET`/`DELETE`/`DETACH`/`DROP`/`REMOVE`. `handle_query_cypher` calls
`rebuild_name_index()` only when this heuristic fires, after the query executes; a rebuild
failure is non-fatal to the request (matching the existing posture at the two
`knowledge_rebuild_from_wal` sites) and marks the index untrusted instead. Reusing the WAL's own
heuristic keeps the two "does this look like a write" decisions (WAL logging vs. index rebuild)
from silently diverging into two different definitions of "mutation."

### The scan fallback reproduces the index's own resolution behavior, including `Merged` tombstones

`corrections::merge_entities` labels aliases `"Merged"` without removing them from the graph or
the index (see `name_index_coherence.rs`'s
`apply_same_as_label_mutation_does_not_affect_name_lookups`) — the index resolves through a
`Merged`-labelled alias to its own UUID rather than filtering it out. `scan_entity_by_name_ci`
(the private query backing the fallback) applies no `Merged`-label filter for the same reason:
a fallback that disagreed with the index on what "resolves" means for a merged-away alias would
itself be a new correctness bug, not a fix. `group_id` non-normalization is likewise preserved
— the scan filters by exact `group_id` string equality, matching the index's own key shape.

## Consequences

### Positive

- Edge-endpoint resolution (#218/#209's guarantee) no longer silently degrades based on how or
  when an entity was written — raw Cypher, WAL replay, or a coherent index all resolve
  correctly, closing the gap #219 opened.
- The common case (a coherent index, no misses) pays no additional cost: Phase B and the
  unmodified `get_entity_by_name_ci` are byte-for-byte the same shape as ADR-0038 left them,
  save for iterating a small `Vec` instead of unwrapping an `Option` — noise relative to the
  index lookup and verify-on-hit query that already dominate the call.
- Index desync is now an observable operational signal (`name_index_trusted`,
  `name_index_fallback_scans` on `knowledge_status`) instead of something only discoverable by
  reading source or reproducing the bug.
- `NameIndex::lookup_candidates`'s winner-first ordering means a verify-on-hit failure on the
  deterministic winner no longer discards a perfectly resolvable same-named entity.

### Negative / Residual risks

- **The FR-002 bound depends on Phase C's per-edge loop running to completion within a single
  `spawn_blocking` closure**, so a scan-fallback result (hit or memoized miss) for one edge is
  visible to every later edge in the same pass. True today by construction and flagged with a
  comment at the call site, but a future change that resolved edges outside a single sequential
  loop (e.g. parallelizing Phase C's edge resolution) would silently reintroduce per-edge scans
  without the local memo or the `NameIndex` self-heal warm-up. Not enforced by any assertion.
- **`looks_like_mutation`'s keyword heuristic can have false negatives** — an entity mutation
  that avoids all seven keywords would skip the proactive rebuild. This is the same risk already
  accepted for WAL logging via the same heuristic; FR-001's scan-fallback self-healing at the
  two endpoint-authority call sites still recovers correctness even if FR-004's proactive
  rebuild misses a mutation, just not as promptly.
- **`rebuild_name_index()` after every raw-Cypher mutation is a full `Entity` table scan, run
  while `handle_query_cypher` still holds the global `state.write_lock` write guard** —
  `handle_query_cypher` already held that lock for its `cypher_query`/WAL-flush before this
  issue; the rebuild extends how long every other writer is blocked by the scan's duration, on
  top of the scan cost itself. Accepted for the same reason as the scan cost: this is an
  administrative, not hot-ingest, path, so the added lock-hold time is expected to be rare and
  short-lived relative to ingest traffic. Would need revisiting (e.g. rebuilding after releasing
  the lock) if this path became a high-frequency or highly concurrent write channel.
- **The trust flag is coarse**: it is a single boolean for the entire index, not scoped to which
  names/groups might be affected. A single failed rebuild marks everything untrusted until the
  next successful rebuild, even though most of the index may still be coherent. This matches
  FR-003's requirement and keeps the mechanism simple; a finer-grained trust model was not
  pursued because the fallback's correctness doesn't depend on the flag (see above) — it exists
  for diagnosis, not gating.

## Alternatives Considered

### Scan fallback inside `get_entity_by_name_ci` itself, covering every call site uniformly

Rejected: reintroduces the exact O(edges × |Entity|) cost ADR-0038 removed for Phase B's
per-entity dedup lookup if the fallback applied unconditionally to every caller. Keeping the
fallback method distinct and used only at the endpoint-authority call site makes the bound
explicit and auditable rather than implicit in a shared function's behavior.

### Thread an earlier pass's resolved name→UUID map forward into Phase C instead of self-healing the index

Rejected: would replace Phase C's live commit-time re-verification with a value cached from
earlier in the request, silently reintroducing the risk of committing an edge against an entity
deleted between the earlier pass and Phase C's commit — without triggering any of the existing
"dropping edge" diagnostics that a fresh lookup provides. (This was evaluated against the
issue's original design, which had a separate DB-backed pre-lock pass; #281 removed that pass
in favor of a DB-independent embedding salvage step, but the rejection reasoning — don't cache
a UUID across a lock boundary in place of a fresh verified lookup — still applies to Phase C's
current single-pass self-healing.)

### Make the index authoritative by construction (every write path updates/invalidates it before considered complete)

Considered per the spec's FR-001 alternative framing. Rejected as the primary mechanism because
it would require hooking every current and future raw-Cypher / second-writer-process access
path, which is unenforceable by construction for genuinely external writers (a second process,
an eval harness, a migration script) — no in-process hook can observe those. A scan fallback is
the only mechanism that closes the gap regardless of *how* a write happened to bypass the index.
`handle_query_cypher`'s proactive rebuild (FR-004) is still added as a best-effort optimization
for the in-process raw-Cypher case specifically, but the scan fallback remains the backstop.

### Unconditional `rebuild_name_index()` on every `handle_query_cypher` call, regardless of statement shape

Rejected: would tax read-only administrative Cypher queries (presumably the common case on this
path) with a full `Entity` scan for no benefit. Reusing the WAL's existing mutation-keyword
heuristic avoids this without inventing new Cypher-parsing logic, which issue #170 FR-008 puts
out of scope for this escape hatch.

## Related

- **ADR-0038**: In-Process NameIndex Accelerator for Entity Name Lookup — the accelerator this
  ADR narrows the "no scan fallback" stance of, for two of its four call sites only.
- **ADR-0029**: Name-First Entity Resolution in add_episode Phase B — introduced
  `get_entity_by_name_ci`; unaffected by this ADR (Phase B remains on the unmodified method).
- Issue #209 / PR #218 (`04aacec`) — original persisted-entity fallback for edge-endpoint
  validation, whose guarantee issue #219 (`f0a8ed3`) silently narrowed and this issue restores.
- `crates/core/src/name_index.rs`: `NameIndex` — trust flag, fallback-scan counter,
  `lookup_candidates`.
- `crates/core/src/db.rs`: `get_entity_by_name_ci` (FR-005 multi-candidate retry),
  `get_entity_by_name_ci_with_scan_fallback`, `scan_entity_by_name_ci`.
- `crates/core/src/episode.rs`: Phase C's endpoint-resolution call sites, commented with the
  self-heal-bound dependency.
- **ADR-0051** (`0051-edge-endpoint-salvage-and-deferred-drop.md`): the #281 restructuring that
  removed episode.rs's original pre-lock, DB-backed Site 1 fallback in favor of a DB-independent
  embedding-salvage step, making Phase C the sole DB-backed endpoint-authority call site this
  ADR's fallback now backs.
- `crates/core/src/wal.rs`: `looks_like_mutation()`, extracted from `WalWriter::log_mutation`.
- `crates/core/src/handlers.rs`: `handle_query_cypher` (FR-004), the two
  `knowledge_rebuild_from_wal` `mark_name_index_untrusted()` call sites (FR-003),
  `handle_knowledge_status` (SC-004).
- `crates/core/tests/edge_endpoint_resolution.rs`,
  `crates/core/tests/name_index_coherence.rs`,
  `crates/core/tests/handlers_wal_admin.rs`: coverage for SC-001, SC-002, FR-004, FR-005, SC-004.
