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

Close the race with **insert-if-absent** on `resolve_ontology`'s side and **upsert-on-absent** on
`clear_group_drift`'s side — both changes confined to `HashMap` operations already performed while
`group_ontologies`'s lock is held, with no new I/O brought under that lock.

1. `resolve_ontology`'s first-resolution path is split into `compute_group_ontology_entry`
   (sidecar read + drift computation, unchanged in logic, performed with no lock held) and
   `insert_group_ontology_entry_if_absent` (`guard.entry(group_id).or_insert(entry)`, replacing
   the previous unconditional `guard.insert(...)`).
2. `clear_group_drift` changes from a no-op-on-absence `get_mut` to an upsert:
   `guard.entry(group_id).and_modify(|e| e.drift = GroupDriftStatus::default()).or_insert_with(...)`,
   seeded with an `ontology: Option<Arc<Ontology>>` parameter the caller already has in hand (all
   three call sites — `episode.rs`'s post-ingest clear, and both `handle_rebuild_from_wal` paths in
   `handlers.rs` — resolve the group's ontology immediately before calling it).

Together: whichever writer reaches `group_ontologies` first for a never-before-resolved group
determines the cached state. If a remediation's clear runs first (during the race window), it
creates a correct, non-drifted entry that `resolve_ontology`'s later, stale insert can no longer
overwrite, since insert-if-absent finds the slot already occupied. If `resolve_ontology`'s insert
runs first (no race, or the remediation runs entirely before or after it), behavior is unchanged
from today: the entry it computed is inserted, and a later `clear_group_drift` call still mutates
it in place via `and_modify`.

## Rejected alternatives

- **Widen `group_ontologies`'s lock across the sidecar read (or a DB round trip)**: this is the
  approach the #451 review explicitly rejected, and this issue's own FR-003 restates that
  rejection as a hard requirement — no unrelated group's cache access may incur new lock-wait
  time. Insert-if-absent achieves correctness without ever locking around I/O.
- **A versioned sidecar read** (e.g. an mtime or generation stamp `resolve_ontology` could use to
  detect it read stale data and re-check before inserting): would also close the race, but adds a
  new on-disk contract to the sidecar format and a re-check step, for no benefit over
  insert-if-absent once Research established that the race is confined to a group's first
  resolution only (every later call for that `group_id` returns straight from the cache — see
  `resolve_ontology`'s cache-hit fast path — so there is no scenario after the first resolution
  where re-validating a read against a version stamp would matter).
- **Accept the race permanently (this issue's FR-004(b))**: superseded once Research's static
  analysis showed a fix meeting both correctness (FR-001) and no-new-contention (FR-003) was
  achievable at low cost. Recorded here only as the option not taken.

## Consequences

- `clear_group_drift`'s signature changes from `(&self, group_id: &str)` to `(&self, group_id:
  &str, ontology: Option<Arc<Ontology>>)`. All three call sites were updated; none needed new
  computation, since each already had the resolved ontology in hand.
- `clear_group_drift` is no longer a no-op when the group has no cache entry — it now creates one.
  A caller relying on the old no-op behavior (none currently exist) would need to account for this.
- A wrong `ontology` value passed into `clear_group_drift`'s upsert path would be cached for the
  life of the process, since a cache hit short-circuits `resolve_ontology` forever after — this is
  guarded by a dedicated test (`clear_group_drift_upsert_records_the_given_ontology` in
  `app_state.rs`).
- The race is covered by a deterministic unit test
  (`concurrent_remediation_clear_survives_a_stale_first_resolution_insert` in `app_state.rs`) that
  drives the exact interleaving via the new private `compute_group_ontology_entry` /
  `insert_group_ontology_entry_if_absent` split, rather than a real multi-threaded test — the race
  window is too narrow for a thread-based test to force reliably without being flaky (see this
  codebase's existing `Barrier`-based concurrency tests in `checkpoint.rs`/`wal_generation.rs`,
  which rely on genuine OS-level atomicity to force a deterministic outcome; that doesn't transfer
  here).
- The source comment this ADR supersedes has been rewritten at both changed sites to describe the
  fix rather than the accepted gap.
