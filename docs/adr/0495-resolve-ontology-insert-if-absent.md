# ADR-0495: Close the `resolve_ontology` Stale-Drift-Insert Race via Insert-If-Absent

**Status**: Accepted
**Date**: 2026-09-01
**Issue**: #495
**Supersedes**: the `// Known race (issue #451 review)` comment previously at the
`group_ontologies` insert site in `app_state.rs`, and the acceptance decision it recorded
**Relates to**: ADR-0446 (per-group ontology resolution), ADR-0451 (per-group ontology drift —
cache placement and clear scope)

## Context

`AppState::resolve_ontology` is the sole place a group's ontology and drift status are first
computed and cached, on that group's first resolution in a process (ADR-0446, ADR-0451). It reads
that group's `.lcg/ontology-hash.json` sidecar, computes drift, and inserts a `GroupOntologyEntry`
into `AppState.group_ontologies` — without holding that cache's lock across the read-compute-insert
sequence.

The #451 review identified that a concurrent remediation for the same never-before-resolved
group — a WAL rebuild, or a successful `add_episode` (both of which write a fresh sidecar and call
`AppState::clear_group_drift` on success) — could run in the window between `resolve_ontology`'s
sidecar read and its cache insert. If it did, its clear would be silently overwritten by
`resolve_ontology`'s subsequent insert, which was computed against the pre-remediation sidecar and
carries a stale `drifted: true`. That review deliberately left the race open, reasoning that
closing it correctly would require either holding the lock across a DB round trip (new contention
for every group sharing the process) or a versioned sidecar read, and that the race was
self-bounded (FR-009 unconditionally clears drift on that group's next successful `add_episode`
regardless). The decision was recorded only in a source comment at the insert site.

That comment was carried forward unmodified through the `0.13.4` release and the #494 forward-port
to `main`; both an automated reviewer and `handarbeit-pruefer` re-flagged the race on #494 and
confirmed it matched the documented, accepted account. This issue exists to give the race a proper
resolution.

## Decision

Close the race by giving `group_ontologies` a third state, not just two. Its value type becomes
`GroupOntologyCacheState { Resolving, Resolved(GroupOntologyEntry) }` instead of bare
`GroupOntologyEntry`, so a group can be **absent** (nothing has touched it), **`Resolving`** (a
first resolution is in flight right now), or **`Resolved`** (a real answer exists). All changes
are confined to `HashMap` operations already performed while `group_ontologies`'s lock is held,
with no new I/O brought under that lock.

An earlier version of this fix used a plain two-state insert-if-absent / upsert-on-absent scheme:
`resolve_ontology`'s insert became insert-if-absent, and `clear_group_drift` became an
unconditional upsert-on-absence. That version closed FR-001 but broke FR-002/SC-003: with
`clear_group_drift` upserting unconditionally, a WAL rebuild of a group nothing had ever resolved
would populate the drift cache purely as a side effect of the remediation — exactly the behavior
`peek_or_load_ontology` (issue #494) was introduced to prevent, and exactly what
`per_group_ontology.rs`'s `wal_rebuild_of_never_resolved_group_does_not_populate_drift_cache`
regression test exists to catch. That test caught it. The three-state design below is what
actually ships.

1. `resolve_ontology`'s first-resolution path marks the group `Resolving` (via
   `mark_group_resolving`, `guard.entry(group_id).or_insert(Resolving)` — a no-op if the group is
   already `Resolving` or `Resolved`) *before* reading the sidecar, then calls
   `compute_group_ontology_entry` (sidecar read + drift computation, unchanged in logic, no lock
   held), then `insert_group_ontology_entry_if_not_resolved` — inserts `Resolved(entry)` unless the
   slot is already `Resolved` (by a remediation's upsert, or a racing first-resolution that got
   there first), in which case it backs off.
2. `clear_group_drift` now matches on the slot's current state: if `Resolved`, mutate `drift` in
   place exactly as before #495; if `Resolving`, **upsert** — replace the marker with
   `Resolved(GroupOntologyEntry { ontology, drift: GroupDriftStatus::default() })`, seeded with the
   `ontology: Option<Arc<Ontology>>` parameter the caller already has in hand (all three call
   sites — `episode.rs`'s post-ingest clear, and both `handle_rebuild_from_wal` paths in
   `handlers.rs` — resolve the group's ontology immediately before calling it); if **absent**, stay
   a no-op, preserving FR-007/FR-002 exactly as before #495.
3. `peek_or_load_ontology`, `group_drift_status`, and `all_group_drift_statuses` all treat
   `Resolving` the same as absent — it isn't a real answer, so it must never be observable as one.

Together: whichever writer reaches `group_ontologies` first for a never-before-resolved group that
has a first resolution genuinely in flight determines the cached state. If a remediation's clear
runs first (during the race window — which by construction only exists once `resolve_ontology` has
already marked the group `Resolving`), it creates a correct, non-drifted `Resolved` entry that
`resolve_ontology`'s later, stale insert can no longer overwrite, since
`insert_group_ontology_entry_if_not_resolved` finds the slot already `Resolved`. If no first
resolution is in flight at all, `clear_group_drift` stays a no-op, so a remediation running alone
never populates the cache — preserving the pre-#495 guarantee. If `resolve_ontology`'s insert runs
first (no race, or the remediation runs entirely before or after it), behavior is unchanged from
today: the entry it computed is inserted, and a later `clear_group_drift` call still mutates it in
place.

This issue's Background describes two symptoms of the race: the stale cached `drifted: true`, and
a spurious "drift detected — recommend Recreate + re-ingest" stderr line. Closing only the cached
state would leave the second symptom in place — `compute_group_ontology_entry` observes drift
against the pre-remediation sidecar regardless of which writer ultimately wins the race, so
printing at compute time would still warn during the exact window this fix closes. The stderr
warning is therefore emitted by `resolve_ontology` itself, after
`insert_group_ontology_entry_if_not_resolved` returns, and only when this call's own computation
actually won the race and became the cached state — closing both symptoms together.

## Rejected alternatives

- **Two-state insert-if-absent / upsert-on-absence** (this ADR's first draft): described above —
  rejected because it reintroduces the exact bug `per_group_ontology.rs`'s
  `wal_rebuild_of_never_resolved_group_does_not_populate_drift_cache` regression-tests against. A
  third `Resolving` state is the minimum needed to let a race-window clear win without letting a
  standalone remediation clear populate the cache.
- **Widen `group_ontologies`'s lock across the sidecar read (or a DB round trip)**: this is the
  approach the #451 review explicitly rejected, and this issue's own FR-003 restates that
  rejection as a hard requirement — no unrelated group's cache access may incur new lock-wait
  time. The `Resolving`-marker design achieves correctness without ever locking around I/O.
- **A versioned sidecar read** (e.g. an mtime or generation stamp `resolve_ontology` could use to
  detect it read stale data and re-check before inserting): would also close the race, but adds a
  new on-disk contract to the sidecar format and a re-check step, for no benefit over the
  in-memory marker once Research established that the race is confined to a group's first
  resolution only (every later call for that `group_id` returns straight from the cache — see
  `resolve_ontology`'s cache-hit fast path — so there is no scenario after the first resolution
  where re-validating a read against a version stamp would matter).
- **Accept the race permanently (this issue's FR-004(b))**: superseded once a fix meeting
  correctness (FR-001), the no-side-effect-caching guarantee (FR-002/FR-007), and
  no-new-contention (FR-003) simultaneously was shown achievable at low cost. Recorded here only as
  the option not taken.

## Consequences

- `AppState.group_ontologies`'s value type changes from `GroupOntologyEntry` to
  `GroupOntologyCacheState` (`Resolving` | `Resolved(GroupOntologyEntry)`). Only `app_state.rs`
  constructs or matches on this type directly; every external construction site (tests, a bench,
  `wal_exec.rs`'s helper) builds the field via `HashMap::new()` with the value type inferred from
  the field's declaration, so none needed an edit (mirrors ADR-0451 Decision 1's zero-touch
  rationale).
- `clear_group_drift`'s signature changes from `(&self, group_id: &str)` to `(&self, group_id:
  &str, ontology: Option<Arc<Ontology>>)`. All three call sites were updated; none needed new
  computation, since each already had the resolved ontology in hand.
- `clear_group_drift` is no longer *always* a no-op when the group has no cache entry — it upserts
  only when a first resolution is genuinely `Resolving`, and stays a no-op when the group is
  wholly absent. A caller relying on the old always-no-op-on-absence behavior (none currently
  exist) would need to account for the `Resolving` case.
- A wrong `ontology` value passed into `clear_group_drift`'s upsert path would be cached for the
  life of the process, since a cache hit short-circuits `resolve_ontology` forever after — this is
  guarded by a dedicated test
  (`clear_group_drift_upsert_during_in_flight_resolution_records_the_given_ontology` in
  `app_state.rs`).
- The race is covered by a deterministic unit test
  (`concurrent_remediation_clear_survives_a_stale_first_resolution_insert` in `app_state.rs`) that
  drives the exact interleaving via `mark_group_resolving` +
  `compute_group_ontology_entry` / `insert_group_ontology_entry_if_not_resolved`, rather than a
  real multi-threaded test — the race window is too narrow for a thread-based test to force
  reliably without being flaky (see this codebase's existing `Barrier`-based concurrency tests in
  `checkpoint.rs`/`wal_generation.rs`, which rely on genuine OS-level atomicity to force a
  deterministic outcome; that doesn't transfer here).
- The no-side-effect-caching guarantee (FR-002/FR-007) is covered by a dedicated unit test
  (`clear_group_drift_is_a_noop_when_no_resolution_is_in_flight` in `app_state.rs`), in addition to
  the pre-existing `per_group_ontology.rs` integration test that caught the two-state design's
  regression.
- The source comment this ADR supersedes has been rewritten at both changed sites to describe the
  fix rather than the accepted gap.
