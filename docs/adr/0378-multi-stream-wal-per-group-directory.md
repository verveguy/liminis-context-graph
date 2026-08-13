# ADR-0378: Multi-Stream WAL — One WAL Directory Per Group

**Status**: Accepted
**Date**: 2026-08-13
**Issue**: #378

## Context

Before this issue, an `lcg` instance held exactly one WAL directory (`AppState.wal_dir`), one
`WalWriter` (`AppState.wal_writer: Arc<Mutex<Option<WalWriter>>>`), and one hardcoded singleton
`WalPosition` row (`{id: 'singleton'}`, `db.rs`'s `get_applied_seq`/`set_applied_seq`). `group_id`
already existed as a free-form partition label on every node and edge, defaulting to `"liminis"`
when a caller didn't supply one, but it carried no filesystem or WAL-stream meaning — every
group's mutations interleaved into the one shared directory, under one shared `global_seq`
numberline, tracked by that one shared position row.

Two prerequisites, both merged before this issue's Implement stage, made a genuinely multi-stream
model coherent where it wouldn't have been before:

- **#369** (ADR-0369) introduced resolvable semantic pointers for cross-group edges
  (`binding_state`: `bound`/`unbound`/`ambiguous`), so a cross-group reference is no longer a
  frozen UUID foreign key — it can be re-resolved after one group's stream replays independently
  of another's.
- **#371** (ADR-0371) stopped `corrections::merge_entities_inner` from writing to a group other
  than the one owning the merge. Before #371, a single `drain_mutations()` call could carry
  mutations belonging to more than one group through a path (`Conn::executed_mutations` →
  `drain_mutations` → `wal_exec::wal_flush_*`) that carries no group information at all, which
  would have forced this issue into mutation-level attribution instead of per-operation
  attribution (see FR-004 below).

This issue supersedes #360, which proposed the same directory-per-source topology but keyed it on
a separate "source identifier" and rested on two assumptions #369 had already retired by the time
#360 would have shipped: that groups are disjoint (no cross-group edges) and that only one source
is ever a write master. #360 never reached Specify and was closed rather than carried forward.

`LCG_WAL_DIR` also names more than `*.jsonl` files: `.checkpoints/` (#365, ADR-0365) and
`.wal-bounds.json` (#375, ADR-0375) live alongside them. Both had to migrate as a unit with the
WAL files themselves, or every pre-existing checkpoint would silently report `reachable: false`
against an emptied directory post-upgrade.

## Decision

### Directory topology: a WAL root, one subdirectory per group

`LCG_WAL_DIR`'s meaning changes from "the one WAL directory" to "the WAL **root**":

```
<wal_root>/
  liminis/     *.jsonl   .checkpoints/   .wal-bounds.json
  group-a/     *.jsonl   .checkpoints/   .wal-bounds.json
  group-b/     *.jsonl   .checkpoints/   .wal-bounds.json
```

`AppState.wal_dir: Option<PathBuf>` is renamed to `wal_root`; `AppState.wal_writer:
Arc<Mutex<Option<WalWriter>>>` is replaced with `wal_writers: Arc<Mutex<HashMap<String,
WalWriter>>>`, with lazy per-group creation centralized in `AppState::with_wal_writer` (FR-003) —
the first write to a group creates its directory and writer; no group needs to be pre-declared at
startup. `WalWriter`/`WalReplayer`/`checkpoint.rs`/the bounds-manifest functions in `wal.rs`
needed **no internal changes** — they were already `&Path`-parameterized; every change here is in
the orchestration layer that decides *which* directory to hand them.

### FR-005: `group_id` → directory name is self-name-if-safe, else a bijective percent-encoding

A `group_id` that already satisfies `checkpoint::validate_name` (ASCII alphanumeric plus `_`/`-`,
non-empty, ≤200 chars) is used as the directory name unchanged — this covers `"liminis"` and every
`group_id` chosen with this rule in mind, keeping the common case human-readable on disk. A
`group_id` outside that charset (e.g. one already in use as a free-form graph-data label before
this issue existed, containing `.`, `:`, or whitespace) is percent-encoded (RFC-3986-style,
uppercase hex `%XX`), escaping every byte outside the safe charset — deliberately including a
literal `%` if present, so a directory name containing `%` unambiguously means "decode me" and one
without it unambiguously means "this is the literal `group_id`." (`crates/core/src/wal_group.rs`,
`encode_group_dir_name`/`decode_group_dir_name`.)

Lossy sanitization (stripping/replacing unsafe characters) was rejected during Specify review
because it can collide two distinct `group_id` values onto one directory and decouples the on-disk
name from the stored value. A bijective encoding has neither problem: it cannot collide by
construction and is mechanically reversible, which is what lets `wal_group::list_group_wal_dirs`
recover every group's identity for `knowledge_status`'s per-group map (FR-007) without a side
table. Only an empty `group_id`, or one whose encoded name would exceed the 200-char bound, fails
loudly at first use — there is no meaningful directory name for those.

**Residual risk: the encoding is bijective at the string level only, not at the filesystem
level, on a case-insensitive filesystem.** `checkpoint::validate_name`'s charset — which FR-005
mandates an already-safe `group_id` pass through unchanged — is case-sensitive at the string
level (`is_ascii_alphanumeric()` accepts both cases), so two distinct `group_id`s differing only
by case (e.g. `"Acme"` and `"acme"`) both pass through unchanged. On a case-sensitive filesystem
(the Linux default this codebase mostly targets in production) that's fine — they resolve to two
distinct directories. On a case-insensitive one (the *default* for macOS APFS and Windows NTFS,
both plausible dev/deployment targets), they resolve to the *same* physical directory, silently
interleaving both groups' `*.jsonl`/`.checkpoints/`/`.wal-bounds.json` and corrupting both
streams' `global_seq` numbering. Changing the encoding to close this (e.g. always percent-encoding
uppercase letters) would contradict FR-005's explicit text, which is a Specify-stage decision, not
one this issue's Implement/Review stages can silently override. Instead,
`wal_group::check_no_case_insensitive_collision`, called from `AppState::with_wal_writer` the
first time a group's directory would be created, fails loudly if an existing sibling directory
matches case-insensitively but not byte-for-byte, rather than letting the two groups silently
share one directory. This closes the *silent data corruption* failure mode without touching
FR-005's naming rule; it does not make the encoding collision-free on such a filesystem in the way
FR-005's prose otherwise implies — that would require a Specify-stage amendment weighing
readability (the whole point of the "already-safe passes through unchanged" case) against
filesystem-level portability.

### FR-001: migration relocates the WAL directory's entire contents as a unit, crash-safely, with no marker file

`wal_group::migrate_wal_root_if_needed(wal_root)` runs before any per-group `WalWriter` is
constructed (`AppState::from_env`, and `crates/service/src/main.rs`'s independent
pre-`AppState` derivation). It scans `wal_root`'s top level for the specific legacy artifact
kinds a pre-378 flat directory could hold — `*.jsonl` files, `.checkpoints/`, `.wal-bounds.json`,
and that manifest's write-in-progress `.tmp` sibling (`is_legacy_top_level_wal_artifact`) — and,
if any are found, creates `<wal_root>/liminis/` and `fs::rename`s each one into it.

This is crash-safe and idempotent by construction, with no separate marker file:

- Each entry moves via one atomic `fs::rename`, so a crash mid-migration leaves some entries
  already inside `liminis/` (no longer visible to the top-level scan) and some still loose. The
  next call re-lists exactly the unmoved remainder and finishes the job.
- A second call over an already-fully-migrated root is a cheap no-op: the top-level scan finds
  nothing matching the legacy-artifact patterns (a real per-group subdirectory like `group-a/` is
  never mistaken for loose legacy content, since the scan only recognizes the specific artifact
  *kinds* above, not "any top-level entry").
- Once `liminis/` exists and the root holds no more loose top-level artifacts, the root is
  considered migrated permanently — nothing post-378 ever writes a loose top-level file again.

Covered by `wal_group.rs`'s own unit tests (round-trip relocation, idempotency, crash-resume, and
non-interference with a sibling group directory) and by
`crates/core/tests/wal_root_migration.rs`'s end-to-end regression test, which drives a synthetic
pre-378 flat directory (loose `.jsonl` + a hand-written `.checkpoints/<name>/g1.create.json`
fixture) through migration and then through real IPC dispatch (`knowledge_wal_mark_list`,
`knowledge_status`) to confirm the migration is invisible to a caller that only knows the pre-378
shape: applied position unchanged, the pre-existing checkpoint's `reachable` flag preserved
(SC-005).

**The filesystem relocation above is only half of FR-001's "position unchanged" guarantee — the
other half is a second, DB-side migration for the `WalPosition` row itself.** `wal_group`'s
migration only ever touches files; a pre-378 database's persisted position lived in a
`WalPosition {id: 'singleton'}` row (the hardcoded key `get_applied_seq`/`set_applied_seq` used
before this issue). Post-378, those functions key on `group_id` instead, so on its own the
filesystem migration would leave that row orphaned under a key nothing ever reads again —
`get_applied_seq("liminis")` would find no row, silently degrading an already-known,
durably-recorded position to `None` ("unknown") and forcing `backfill_applied_seq_if_absent` to
re-derive it from a WAL scan (or, in the worst case such as a UUID mismatch, fail to re-derive it
at all). Either outcome contradicts FR-009's "behaves exactly as it does in 0.12.2" and FR-001's
"no operator action required."

`Conn::migrate_legacy_singleton_wal_position(group_id)` (`db.rs`) closes this gap: reads the
legacy row's `applied_seq` if present, writes it forward to `group_id`'s own row only if that row
doesn't already exist (never overwriting a value already established under the new key), then
deletes the legacy row. It is called once at startup — in `crates/service/src/main.rs`'s
production boot path and in `Db::open_or_rebuild`'s library-API equivalent — immediately before
`backfill_applied_seq_if_absent`, so the legacy value is available to be found rather than
triggering a needless (or failing) re-derivation. No-op on a fresh install (no legacy row) or on
any boot after the first (idempotent). Covered by four unit tests in `db.rs`'s
`applied_seq_tests` module (value carried forward and legacy row removed; no-op with no legacy
row; an already-present group row wins over stale legacy data; idempotent on a second call), and
`wal_root_migration.rs`'s SC-005 test seeds the legacy `'singleton'` row directly (mirroring
exactly what pre-378 `set_applied_seq` wrote) rather than the post-378 key, so it exercises this
migration rather than assuming it away.

### Write-lock granularity stays global — no per-group DB lock

`state.write_lock: Arc<RwLock<()>>` continues to serialize every Cypher execution against the
single embedded lbug DB connection, unchanged by this issue. The DB itself is a single-writer
store regardless of how many WAL streams sit above it — relaxing that lock is orthogonal to this
issue and explicitly out of scope.

User Story 1 Acceptance Scenario 4 ("neither write is blocked by the other's write lock... neither
corrupts the other's `global_seq` sequencing") is satisfied at the WAL layer instead: each group
has its own `WalWriter` behind the `wal_writers` map, guarded by one `Mutex` held only for the
lookup-or-lazily-create-and-flush step (microseconds, not the DB write itself). Two groups' WAL
bookkeeping never contends with each other in any way that matters — the thing that actually needs
isolation (each group's own `global_seq` counter and on-disk stream) is isolated; the thing that
was never going to be isolated (the single embedded DB) stays exactly as serialized as it always
was.

### FR-004: call sites that genuinely span groups route to the default group, not mutation-level attribution

FR-004 forbids designing mutation-level attribution (tagging each individual mutation with its own
group as it flows through `Conn::executed_mutations` → `drain_mutations` →
`wal_exec::wal_flush_*`) — that path carries no group information today, and per-operation
attribution (naming one group at the flush site) is sufficient everywhere else because #371
already stopped merge from writing across groups within one `drain_mutations()` call.

At the time this issue shipped, four call sites didn't fit "exactly one group in scope at the
flush site," because they were themselves multi-group by design or took arbitrary input:

- `handle_delete_by_group` (#361/ADR-0361) — its forced rebind pass can write into a *foreign,
  non-purged* owning group's `RelatesToNode_` rows in the same `drain_mutations()` call.
  ADR-0361 itself flagged this and deferred the resolution to this issue.
- `backfill.rs`, `canonicalize.rs` — two maintenance passes that select `RelatesToNode_`
  candidates database-wide, with no `group_id` filter. (`reprocess_relations.rs`'s
  `reprocess_relation_types` was originally implemented alongside these two, copying their
  rationale — but its own Phase A already scopes candidate selection to `params.group_id` via
  `list_edges_for_scope`, so it does *not* belong here; Review corrected it to route to that group
  directly instead. It was never one of the four.)
- `handle_query_cypher` — an arbitrary-Cypher escape hatch with no group attribution at all by
  design.

**Decision (as originally shipped): all four route their WAL flush through `DEFAULT_GROUP_ID`'s
writer unconditionally**, regardless of which group(s) the mutations actually touched, each with
an inline comment citing this rationale. This mirrored ADR-0361's already-shipped precedent
(`"applied_seq_reset": false`, explicitly deferred to this issue) rather than inventing a new
pattern. The DB write itself was unaffected — it already committed via the normal Cypher path
before this decision was even consulted; only that mutation's WAL-replay coverage landed in the
default group's stream rather than a stream matching its actual data.

**Correction (#385, 2026-08-13): per-operation attribution turned out to be insufficient for
handlers that span groups by design, not merely an accepted documented limitation for all four
sites indefinitely.** `handle_delete_by_group` and `knowledge_rebind_pointers` (plus
`clear_group_for_rebuild`, sharing `handle_delete_by_group`'s underlying `purge_groups` call —
see [ADR-0385](0385-per-group-mutation-attribution-for-multi-group-writers.md)) each mutate a
small, fully-known set of specific, identifiable non-default groups per call — the group(s) named
in the request plus whichever owning group(s) a forced or standalone rebind touches. Routing
their entire flush to the default group meant a stream never contained its own group's
deletions or rebind writes: replaying a purged group's own stream in isolation resurrected what
had been purged, and `liminis/` accumulated other groups' mutations as a side effect of
operations that never legitimately wrote to the default group. #385 fixed this by draining each
mutation at the point its owning group is already known and flushing per group — see ADR-0385 for
the mechanism. Also corrected here: the inline comment at `handle_rebind_pointers`'s pre-#385
call site cited this FR-004 rationale, but this section's original "four call sites" enumeration
never actually named `handle_rebind_pointers`/`knowledge_rebind_pointers` among them — that
citation referenced a rationale that had never been extended to cover it in writing.

**The remaining three call sites keep routing to the default group under this section's original
rationale, unchanged by #385**: `backfill.rs` and `canonicalize.rs`'s database-wide maintenance
passes (no `group_id` filter at all — there is no small known set of groups to attribute to), and
`handle_query_cypher`'s arbitrary-Cypher escape hatch (no group attribution possible by design).
This is accepted as a documented limitation for these three, not treated as a defect.

### FR-006: `knowledge_rebuild_from_wal`'s `force_clear` stops wiping the whole DB file

The pre-378 `force_clear` path (`clear_db_for_rebuild`) deleted and reopened the entire lbug DB —
incompatible with FR-006 ("target exactly one group without disturbing any other group's
`WalPosition` row") the moment a second group exists. The group-scoped replacement,
`clear_group_for_rebuild`, instead calls `group_purge::purge_groups(&conn, &[group_id], &ts,
false)` (already transactional, already scoped, already handles forced rebind of foreign pointers
into the cleared group — reused as-is from #361) followed by `conn.set_applied_seq(group_id, 0)`,
without swapping `state.db`. The non-empty-database pre-check ahead of a `from_seq: 0` replay is
correspondingly group-scoped, via `count_entities_by_group_ids`/`count_episodics_by_group_ids`/
`count_relates_to_by_group_ids` (the same primitives `group_purge.rs` already used) instead of a
whole-DB `count_nodes`. `clear_db_for_rebuild` itself is kept, unchanged, for `knowledge_clear_all`
— which is intentionally DB-wide and out of scope for this narrowing.

### The whole-instance recovery paths (`knowledge_recover`, `knowledge_recover_full`, startup self-heal) must restore every group, not only the default one

`knowledge_recover {strategy: "rebuild_from_workspace_wal"}`
(`recover_rebuild_from_workspace_wal`) and the autonomous corruption self-heal
(`recovery::run_full_recovery_sequence`'s fallback branch, reached both from
`knowledge_recover_full` and from startup when the initial DB open fails recoverably) both delete
and reopen the *entire* embedded DB file — which holds every group's graph data, not just one
group's. Before per-group WAL directories existed this was safe: the single shared WAL directory
covered every group, so a whole-DB wipe-and-replay reconstructed everything. Found during Review:
an earlier version of this PR passed these functions only the default group's own resolved WAL
directory, so on a multi-group instance the DB wipe destroyed every non-default group's data from
the live graph while only ever replaying the default group's back in — a correctness regression
introduced specifically by the per-group split, not present pre-378. (The non-default groups' own
`*.jsonl` files were never deleted by this bug, so the data was recoverable via an explicit
`knowledge_rebuild_from_wal` per group — but silently vanishing from the live, queryable graph on
an autonomous self-heal, with no indication anything was lost, is a correctness bug regardless of
whether the underlying bytes survive on disk.)

Both functions now take the WAL **root**, not one group's own subdirectory, and — specifically in
the branches that actually wipe the DB (`recover_rebuild_from_workspace_wal` always; 
`run_full_recovery_sequence` only in its `full_rebuild` fallback, not its non-destructive
checkpoint-drop primary path, which never deletes anything and only needs the default group's own
tail) — enumerate every group via `wal_group::list_group_wal_dirs(wal_root)` and replay each one
back in, persisting each group's own `applied_seq` as it goes. The checkpoint-drop primary path
(step 1 of `run_full_recovery_sequence`) is left scoped to the default group, since it never
deletes existing data and only catches up a possible tail after a torn-WAL reopen — a narrower,
lower-severity gap than the DB-wipe case, and not part of what Review found.

### FR-011: the re-bind staleness gate uses the *target* group's own applied position

`cross_group::resolve_side` and `rebind_pointers_impl`'s staleness gate previously read one shared
`applied_seq` (the pre-378 singleton) and reused it for both endpoints of an edge. Both now call
`conn.get_applied_seq(source_group_id)` — `source_group_id` being the function's own parameter, the
group a pointer is pointing *into* — since a single edge's two endpoints can point into two
different foreign groups, and the group whose replay should drive re-binding is the group being
replayed, never any other. Regression-tested end to end by
`crates/core/tests/cross_group_incremental_replay.rs`: a layer group's own `applied_seq` is
deliberately set far ahead of the target group's, and the test confirms the re-bound pointer's
`bound_at_seq` reflects the target group's position, not the layer group's (SC-006).

### `knowledge_clear_all` clears the whole `wal_writers` map, doesn't eagerly recreate anything

`knowledge_clear_all` remains DB-wide (it already clears every group's data). Its `!preserve_wal`
branch now clears the entire `wal_writers` map instead of `take()`-ing a single `Option`, and does
**not** eagerly recreate any writer — the next write to any group creates its writer and directory
on demand (FR-003), the same as a fresh install. `set_applied_seq(0)` after clear applies only to
`DEFAULT_GROUP_ID`, preserving FR-009 parity; other groups' `WalPosition` rows are simply absent
post-clear, which is the correct "unknown/no stream yet" state, not "known-empty."

### Startup backfills only the default group; other groups backfill lazily on the `knowledge_status` read path

`main.rs`'s startup calls `backfill_applied_seq_if_absent` for `DEFAULT_GROUP_ID` only, matching
FR-009's "single-group instance behaves exactly as pre-378." `handle_knowledge_status`'s per-group
loop (FR-007) runs the same group-scoped backfill for each group directory it discovers via
`wal_group::list_group_wal_dirs`, before reading that group's `applied_seq`/`max_seq` — this is the
one place a non-default group's position gets backfilled, lazily, on the read path that actually
needs the value, rather than an up-front cost that scales with however many groups exist.

**The backfill pass runs under the write lock, in a phase separate from the rest of the status
computation.** Found during Review: `backfill_applied_seq_if_absent` can issue a real
`set_applied_seq` write (a `MERGE ... SET`) the first time a group is queried, but
`handle_knowledge_status` otherwise only ever takes `state.write_lock.read()` — a shared lock every
other write in this codebase deliberately avoids taking a write under. Two concurrent
`knowledge_status` calls racing to backfill the same not-yet-backfilled group could therefore issue
concurrent write transactions against the single-writer embedded DB with no application-level
serialization — a real regression `knowledge_status` never risked pre-378, since its old singleton
backfill ran once, single-threaded, at `Db::open` time before request serving began. The fix splits
the handler into a short **Phase 0** (write lock: backfill every discoverable group whose position
isn't known yet) followed by the existing **read-locked** phase (every other field, including
reading — never writing — each group's now-guaranteed-backfilled `applied_seq`). Phase 0 is a
no-op read-then-skip for every group after its first-ever backfill (matching
`backfill_applied_seq_if_absent`'s own early return), so only the first status call per group pays
the write-lock cost; every later call is read-lock-only, same as before this fix. Regression-tested
by firing several concurrent `knowledge_status` calls at a freshly-seeded, never-yet-queried group
(`ipc_parity.rs::knowledge_status_concurrent_first_backfill_does_not_race`) and asserting every
call reports the same, correctly-backfilled position.

## Consequences

### Positive

- Every group's WAL stream, position, and checkpoint store is now genuinely independent — SC-001
  through SC-007 (two groups hydrating independently, overlapping `seq` ranges not colliding,
  incremental replay of one group leaving another byte-identical, per-group checkpoints) all hold
  by construction, not by discipline.
- The lower-level WAL machinery required zero internal changes; the entire diff is orchestration
  (`AppState`, `handlers.rs`, `db.rs`'s two parameterized queries, `recovery.rs`'s scoped
  lookups), which kept the change mechanical and type-checker-verified — a missed call site fails
  to compile rather than silently misrouting.
- FR-009 (single-group parity) is not a separate code path — it is what every multi-group
  mechanism above degenerates to when there is exactly one group, `"liminis"`. There is no
  "legacy mode" to keep in sync with the general case.
- Migration has no operator-facing step and no marker file to get out of sync with reality; its
  own idempotency check *is* "is there anything left to move," which cannot drift from the truth.

### Negative / Residual risks

- The FR-004-exempted call sites (`backfill.rs`'s and `canonicalize.rs`'s database-wide
  `RelatesToNode_` passes, and raw Cypher via `handle_query_cypher`) have WAL-replay coverage that
  doesn't match their actual data's group. This is a deliberate, documented gap (the DB write
  itself is never affected), but it means a full rebuild of a non-default group from its own WAL
  directory alone will not reproduce a maintenance-pass mutation that happened to touch that
  group's data — only a rebuild that includes the default group's stream will. Found during
  Review: `reprocess_relation_types` (`reprocess_relations.rs`) was originally routed through
  `DEFAULT_GROUP_ID` alongside these three, but unlike them it is genuinely single-group scoped —
  its Phase A candidate selection already filters by `params.group_id` via
  `list_edges_for_scope` — so it was corrected to route to that group directly; it does not belong
  on this list. (Group-scoped purge's foreign-group rebind was originally on this list too, but
  #385/[ADR-0385](0385-per-group-mutation-attribution-for-multi-group-writers.md) corrected its
  routing — see this ADR's FR-004 section as amended.)
- The non-fallback ("checkpoint-drop") branch of `run_full_recovery_sequence` only catches up the
  default group's own app-WAL tail. `attempt_checkpoint_drop` renames aside lbug's own
  engine-level `.wal` file, which is shared across every group's not-yet-durable writes, not just
  the default group's — if that discarded engine WAL held a pending write for a non-default group
  at the moment of corruption, this path never replays it back in. That data is still recoverable
  via an explicit per-group `knowledge_rebuild_from_wal` (the group's own `*.jsonl` files are
  untouched), but the live graph can silently diverge from the WAL in the meantime, with no error
  surfaced. Fixing this fully would require deriving a per-group cursor and replaying every
  group's tail on this path too, not only the fallback's full-rebuild branch (which this PR's
  Review did fix, see the multi-group recovery section above) — flagged here as a known,
  accepted gap rather than expanded scope for this issue.
- `migrate_wal_root_if_needed`'s per-entry `if dest.exists() { continue }` check (used for
  `.checkpoints/`'s single-rename relocation) cannot distinguish "a prior partial migration run
  already moved this" from "the default group's own directory now legitimately has its own
  `.checkpoints/` for an unrelated reason." Both callers treat a migration failure as non-fatal,
  so if migration fails after relocating the `*.jsonl` files but before `.checkpoints/`, and the
  service starts up and creates a checkpoint under the (now correctly-resolving) default group
  before migration is retried, the retry will see `<root>/liminis/.checkpoints/` already exists
  and skip merging in the true legacy history — which then stays orphaned at the WAL root's top
  level forever (never migrated, never surfaced as a group by `list_group_wal_dirs`'s round-trip
  filter either). A full fix would need to merge two `.checkpoints/` trees rather than skip
  wholesale; accepted as a narrow, compound-failure-required edge case rather than expanded scope.
- The bijective percent-encoding for unsafe `group_id`s is new surface area with no prior
  precedent in this codebase (unlike `checkpoint::validate_name`, which #365 already established).
  A `group_id` that needs encoding produces a directory name a human can't read at a glance
  (`source%2Edoc%3Av1`), a readability cost accepted in exchange for collision-freedom and
  reversibility.
- Migration is a one-time, hard-to-exercise-in-CI path. Unit tests
  (`wal_group.rs`) and the end-to-end regression test (`wal_root_migration.rs`) are the only
  coverage; a subtle idempotency bug would only surface against a real pre-378 deployment. The
  per-entry-rename design was chosen specifically to make partial-failure recovery mechanical
  (retry just re-lists what's still loose) rather than requiring a transactional multi-file move.
- `knowledge_wal_mark_list` with no `group_id` still returns only the default group's checkpoints
  (FR-012) — there is no "list checkpoints across every active group" aggregate. An operator
  inspecting a multi-group instance's checkpoints must call `_list` once per group they care
  about, matching how `_create`/`_delete` already operate on one target at a time.

## Alternatives Considered

### Mutation-level group attribution (tagging each mutation as it flows through `drain_mutations`)

Rejected per FR-004: `Conn::executed_mutations` carries no group information today, and adding it
would be substantially more invasive than per-operation attribution, which is sufficient at every
site except the four FR-004-exempted ones — and those four are inherently multi-group in a way no
per-mutation tag would resolve cleanly anyway (e.g. arbitrary Cypher has no group at all until the
query is inspected).

### Per-group write locks instead of a global `state.write_lock`

Rejected: the embedded lbug DB is a single-writer store below the WAL layer regardless of how many
streams exist above it — a per-group lock would create the *appearance* of concurrency without the
underlying DB actually supporting concurrent writers, adding complexity for a guarantee the storage
layer can't back up.

### Lossy sanitization of an unsafe `group_id` into a directory name

Rejected (Specify-stage decision, reaffirmed here): can silently collide two distinct `group_id`
values onto one directory, and decouples the on-disk name from the stored value. A bijective
encoding has neither problem.

### Fail loudly on any `group_id` outside `checkpoint::validate_name`'s charset, with no encoding at all

Considered and rejected during Specify review: `group_id` was already a free-form graph-data label
with no filesystem meaning before this issue, so a deployment that had chosen a filesystem-unsafe
`group_id` before this issue shipped would become permanently unable to add new WAL-backed writes
to that group, with no migration path (this issue explicitly declines to migrate stored `group_id`
values — see Out of Scope in the spec). The bijective encoding gives every existing `group_id` a
usable directory without that trap.

## Related

- `crates/core/src/wal_group.rs` — `encode_group_dir_name`/`decode_group_dir_name`,
  `group_wal_dir`, `list_group_wal_dirs`, `migrate_wal_root_if_needed`, `DEFAULT_GROUP_ID`.
- `crates/core/src/app_state.rs` — `wal_root`, `wal_writers`, `with_wal_writer`.
- `crates/core/src/db.rs` — `get_applied_seq`/`set_applied_seq`/`get_latest_episode_uuid`, now
  `group_id`-parameterized via bound `$id`/`$group_id` params.
- `crates/core/src/wal_exec.rs` — `wal_flush_chunk`, `wal_flush_ungrouped`,
  `resync_global_seq_after_rebuild`, `GlobalSeqResyncGuard`.
- `crates/core/src/cross_group.rs` — `resolve_side`, `rebind_pointers_impl`'s per-target-group
  staleness gate (FR-011).
- `crates/core/src/handlers.rs` — `resolve_group_wal_dir`, `group_id_param`,
  `handle_knowledge_status`'s per-group `wal_groups` map (FR-007), `clear_group_for_rebuild`
  (FR-006), the three `handle_wal_mark_*` functions (FR-012).
- `crates/core/src/backfill.rs`, `canonicalize.rs`, `reprocess_relations.rs` — the FR-004
  default-group WAL-flush routing.
- `crates/service/src/mcp/tools.rs` — `group_id` schema additions on the three checkpoint tools
  and `knowledge_rebuild_from_wal`.
- `crates/core/tests/wal_root_migration.rs` — SC-005 end-to-end migration regression test.
- `crates/core/tests/cross_group_incremental_replay.rs` — SC-006 end-to-end incremental-replay
  regression test.
- `crates/core/tests/wal_population.rs` —
  `add_episode_to_two_groups_never_crosses_wal_streams` (SC-004).
- [ADR-0353](0353-persist-and-expose-applied-wal-seq.md) — the singleton `applied_seq` design this
  issue replaces with one row per group.
- [ADR-0361](0361-group-scoped-purge.md) — explicitly deferred per-group `applied_seq` reset and
  the FR-004 foreign-group-write tension to this issue; `purge_groups` is reused as-is here.
- [ADR-0385](0385-per-group-mutation-attribution-for-multi-group-writers.md) — corrects this
  ADR's FR-004 decision for `handle_delete_by_group`, `knowledge_rebind_pointers`, and
  `clear_group_for_rebuild`: each now attributes its mutations to the specific group(s) it
  actually modifies instead of routing everything to `DEFAULT_GROUP_ID`.
- [ADR-0365](0365-wal-checkpoints-directory-per-name-store.md) — the directory-per-name checkpoint
  store this issue's migration relocates as a unit and whose IPC methods gain `group_id` (FR-012).
- [ADR-0369](0369-resolvable-cross-group-pointers.md) — defines `binding_state`/`bound_at_seq`,
  the mechanism FR-011 extends to ordinary incremental replay (not only purge-and-rehydrate).
- [ADR-0371](0371-merge-never-writes-foreign-group-data.md) — the merge fix this issue's FR-004
  "per-operation attribution is sufficient" claim depends on.
- [ADR-0375](0375-wal-max-seq-bounds-manifest.md) — the seq-bounds manifest this issue's migration
  relocates alongside `.checkpoints/` and the `.jsonl` files.
- #360 — superseded (closed); same directory-per-source topology, a different key, and two
  assumptions this issue's prerequisites (#369, #371) had already retired.
- `specs/378-multi-stream-wal-one/spec.md` — this issue's spec.
