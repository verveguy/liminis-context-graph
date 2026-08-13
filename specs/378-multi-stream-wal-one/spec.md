# Feature Specification: Multi-stream WAL: one logical graph, one group_id, one WAL directory

**Feature Branch**: `fabrik/issue-378`
**Created**: 2026-08-12
**Status**: Specified
**Input**: User description: "Make the WAL a multi-stream structure: one logical graph, one group_id, one WAL directory. Supersedes #360, which was filed for the same topology but decided the opposite key (a source identifier explicitly not group_id) and rests on two assumptions that no longer hold. #360 never entered Specify, so it is closed rather than amended — a spec whose body argues against its own decision is a hazard for the stage that reads it."

## Background

Today an `lcg` instance has exactly one WAL directory and one `WalWriter`, and `WalPosition` is a hardcoded singleton row (`id: 'singleton'`) — see `crates/core/src/db.rs:1755` (`get_applied_seq`) and `:1768` (`set_applied_seq`). `group_id` already exists as a free-form partition label on every node and edge, defaulting to `"liminis"` when a caller doesn't supply one (`crates/core/src/handlers.rs`), but it carries no filesystem or WAL-stream meaning — every group's mutations are interleaved into the same single WAL directory and tracked by the same single position.

This issue makes `group_id` mean something more specific: a **stream identity**. One logical graph, one `group_id`, one WAL directory, one `WalPosition` row. A single instance may still write to several groups — it just holds N independent writers instead of one, each with its own directory, its own `global_seq` counter, and its own applied position. Two prerequisites now make this coherent where they didn't before:

- **#369 (merged)** introduced resolvable semantic pointers for cross-group edges (`binding_state`: `bound`/`unbound`/`ambiguous`), so a cross-group reference is no longer a frozen UUID FK — it can be re-resolved after one group's stream replays independently of another's.
- **#371 (merged, PR #377)** stopped `corrections::merge_entities_inner` from writing to a group other than the one owning the merge. Before #371, a single `drain_mutations` call could carry mutations belonging to more than one group through a path (`Conn::executed_mutations` → `drain_mutations` → `wal_exec::wal_flush_*`) that carries no group information at all — which would have forced this issue into mutation-level attribution instead of the much simpler per-operation attribution FR-004 relies on. With #371 landed, every write handler already names exactly one group at its flush site (e.g. `episode.rs`'s `add_episode` has `gid_owned` in scope — `crates/core/src/episode.rs:538`), so routing a mutation to its writer is a lookup, not a new tracking mechanism.

This issue **supersedes #360**, which proposed the same directory-per-source topology but keyed it on a separate "source identifier" rather than `group_id`, and rested on two assumptions #369 has since retired: that groups are disjoint (no cross-group edges) and that only one source is ever a write master (replicas are read-only). #360 never reached the Specify stage, so it was closed outright rather than carried forward — a spec that argues against its own conclusion is exactly the kind of artifact a downstream stage should never have to untangle.

Two structural blockers, both still accurate as of this writing (verified against the current `main` branch):

1. **`WalPosition` is a hardcoded singleton.** Two Cypher queries (`db.rs:1758` reads, `db.rs:1772` writes) address `{id: 'singleton'}` explicitly. `WalPosition.id` is already `STRING PRIMARY KEY`, so the table itself already supports N distinct rows — only these two call sites need to stop hardcoding the key.
2. **One WAL directory and one writer per instance.** `crates/core/src/app_state.rs:45` (`pub wal_dir: Option<PathBuf>`) and `:49` (`pub wal_writer: Arc<Mutex<Option<WalWriter>>>`) are both singular. Making this multi-stream is a structural change to `AppState`, not a config toggle.

A single WAL directory today also holds more than WAL files: `.checkpoints/` (#365, merged) and `.wal-bounds.json` (#375, merged) live alongside the `*.jsonl` files, and the three checkpoint IPC methods (`knowledge_wal_mark_create`/`_list`/`_delete`, `crates/core/src/handlers.rs:1525`/`:1580`/`:1590`) currently resolve their target via `require_wal_dir(&state)` (`handlers.rs:1514`) — a single directory, with no notion of which stream it belongs to.

**Why seq spaces must never cross streams.** Every `WalWriter` derives `global_seq` from its own directory via `scan_max_seq` and counts up from there, so two independently-published WAL streams both legitimately contain `seq: 1, 2, 3…`. Comparing or writing a seq from one stream against another group's `WalPosition` is a category error, not just a bug — with one directory per group, this is enforced by construction (there is no shared numberline to collide on) rather than left to code-review discipline.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - An instance writes to two groups without their mutations crossing streams (Priority: P1)

An operator points one running instance at two different `group_id`s (e.g. two different source documents being ingested concurrently). Each mutation lands in the WAL directory belonging to the group it names, and the two streams never interleave.

**Why this priority**: This is the structural core of the issue — everything else (per-group replay, per-group position, per-group checkpoints) only makes sense once writes are actually isolated by stream. Without this, the rest of the feature has nothing to build on.

**Independent Test**: Start one instance, write episodes to `group_id: "A"` and `group_id: "B"` in an interleaved sequence, then inspect the two WAL directories' file contents directly and confirm every line in A's directory names group A and every line in B's directory names group B.

**Acceptance Scenarios**:

1. **Given** an instance with no prior writes to either group, **When** an episode is added under `group_id: "A"` and no directory yet exists for A, **Then** A's WAL directory and writer are created lazily on that first write, without disturbing any other group's state.
2. **Given** an instance already writing to group A, **When** an episode is added under a new `group_id: "B"`, **Then** a second, independent writer and directory are created for B, and A's writer, directory, and `global_seq` counter are unaffected.
3. **Given** writes interleaved between groups A and B from one instance, **When** the resulting WAL directories are inspected, **Then** every mutation in A's directory belongs to group A and every mutation in B's directory belongs to group B — no mutation crosses a stream boundary (SC-004).
4. **Given** concurrent write requests targeting different groups, **When** both are in flight at once, **Then** neither is blocked by the other's write lock, and neither corrupts the other's `global_seq` sequencing (see Edge Cases — write-lock granularity).

---

### User Story 2 - Two groups hydrate and replay independently in one database (Priority: P1)

An operator hydrates a database from two groups' WAL directories. Each group reports its own `applied_seq`, matching its own `max_seq`. Later, group B receives new WAL content and is replayed incrementally; group A's position does not move.

**Why this priority**: This is the other structural half of the feature — writing in isolation is only useful if replay and position tracking are equally isolated. This is also where the two structural blockers (singleton `WalPosition`, singular `wal_dir`/`wal_writer`) are actually exercised end to end.

**Independent Test**: Seed two WAL directories for groups A and B (including a case where both start their `seq` numbering at 0), hydrate a fresh database from both, confirm each group's reported `applied_seq` equals its own `max_seq`; then append new WAL content to B only, replay B, and confirm A's `WalPosition` row is byte-identical before and after.

**Acceptance Scenarios**:

1. **Given** two WAL directories for groups A and B, each independently produced, **When** both are hydrated into one fresh database, **Then** each group's `WalPosition` row reports an `applied_seq` equal to that group's own `max_seq` (SC-001).
2. **Given** a database already hydrated from groups A and B, **When** group B's WAL directory gains new content and `knowledge_rebuild_from_wal` is run targeting only group B, **Then** group A's `WalPosition` row is unchanged — not reset, not advanced (SC-002).
3. **Given** two groups A and B whose `seq` ranges both start at 0 and overlap numerically, **When** both are hydrated into the same database, **Then** neither group's position is corrupted by the other's overlapping sequence numbers (SC-003) — a `seq` from one group is never compared against or written to another group's `WalPosition` row (FR-008).
4. **Given** a group that has never been written to, **When** its position is queried, **Then** it is reported distinctly from a group that has been written to and is at position 0 (`None` vs `Some(0)`, per the discipline already established by #362/#365).

---

### User Story 3 - A single-group instance behaves exactly as it did before this issue (Priority: P1)

An operator running a 0.12.2-era single-stream deployment upgrades. Nothing about that deployment's observable behavior changes: the same WAL directory layout resolves (via a defined migration path), the same position is reported, the same episode-cursor backfill still works, and every existing checkpoint remains reachable.

**Why this priority**: Single-group is the common case and the master topology this codebase has run in production until now (FR-009). A regression here is worse than a missing feature — it breaks every existing deployment, not just ones opting into multi-stream.

**Independent Test**: Open a pre-existing 0.12.2 database and its single WAL directory (unchanged, not pre-migrated) — including its `.checkpoints/` store and `.wal-bounds.json` manifest — with the upgraded binary; confirm it reports the same position it reported before upgrade, without operator intervention, and every existing checkpoint still reports its correct reachability.

**Acceptance Scenarios**:

1. **Given** a 0.12.2 database and its single, pre-existing WAL directory, **When** it is opened with the upgraded binary, **Then** it resolves via the defined `LCG_WAL_DIR` migration path (FR-001) and reports its position unchanged (SC-005).
2. **Given** a single-group instance with no `group_id` ever specified by callers (defaulting to `"liminis"`), **When** it writes, replays, or reports status, **Then** its behavior is indistinguishable from a pre-378 instance (FR-009).
3. **Given** a single-group database whose `WalPosition` row has never been backfilled, **When** the episode-cursor backfill (#353 FR-007) runs, **Then** it still derives the correct position from the one WAL directory and the graph's most recent episode, exactly as before (FR-010).
4. **Given** a 0.12.2 deployment with existing named checkpoints under `LCG_WAL_DIR/.checkpoints/` and a `.wal-bounds.json` manifest, **When** it is opened with the upgraded binary, **Then** every existing checkpoint's reachability is unchanged post-migration, and the seq-bounds manifest either still correctly describes the migrated directory's file set or triggers its documented full-scan fallback — it never reports stale or wrong bounds (FR-001).

---

### User Story 4 - Cross-group pointers re-bind correctly after a per-group replay (Priority: P2)

A layer graph holds cross-group edges into group B (per #369). Group B is incrementally replayed on its own. Afterward, the layer's pointers into B can be re-bound and land in the correct state.

**Why this priority**: This is where multi-stream and #369's pointer mechanism meet. Without a correct per-group applied position, #369's re-bind staleness check has no precise signal to work from for anything short of a full purge-and-rehydrate — this issue is what makes an *incremental* per-group replay an equally well-defined re-bind trigger.

**Independent Test**: Build a layer graph with an edge from group L into group B, replay group B incrementally with a content change affecting the pointer's target, run #369's re-bind pass, and confirm the pointer lands in the correct state.

**Acceptance Scenarios**:

1. **Given** a cross-group edge from group L into group B, **When** group B is incrementally replayed (not purged) and the re-bind pass is run afterward, **Then** the pointer resolves correctly against B's post-replay state (SC-006).
2. **Given** group B's per-group applied position has just advanced due to a replay, **When** the re-bind pass evaluates staleness, **Then** it uses B's own applied position, not any other group's.

---

### User Story 5 - Per-group checkpoints are independent, and the checkpoint tools operate on a named group (Priority: P2)

An operator creates a named checkpoint (#365) in group A's stream by naming group A explicitly. Nothing happening in group B's stream — writes, replays, or its own checkpoints — affects A's checkpoint or its reachability. Listing checkpoints without naming a group shows the default group's checkpoints, matching today's single-stream behavior exactly.

**Why this priority**: #365 checkpoints already exist, and the storage layout becomes per-stream automatically once directories are per-group — but the checkpoint IPC methods themselves currently resolve a single, ungrouped directory (`require_wal_dir`) and have no way to name which stream they target. Without a `group_id` parameter, all three checkpoint methods become unusable the moment a second stream exists, not merely un-migrated.

**Independent Test**: Create a checkpoint naming group A, then perform writes and create a separate checkpoint naming group B, and confirm A's checkpoint and its `wal_min_seq`/`wal_max_seq` reachability bounds are unaffected by B's activity. Separately, call the checkpoint-list method with no group specified and confirm it returns only the default group's checkpoints.

**Acceptance Scenarios**:

1. **Given** a checkpoint created in group A's stream, **When** group B receives new writes and its own checkpoint, **Then** group A's checkpoint and its reachability bounds are unaffected (SC-007).
2. **Given** `knowledge_wal_mark_create`/`_list`/`_delete` are called with an explicit `group_id`, **When** they execute, **Then** each resolves and operates against that group's own WAL directory (FR-012).
3. **Given** `knowledge_wal_mark_list` is called with no `group_id`, **When** it executes on a multi-group instance, **Then** it returns only the default group's (`"liminis"`) checkpoints, matching a pre-378 caller's expectation exactly — it does not aggregate checkpoints across every active group (FR-012).

---

### User Story 6 - `knowledge_status` reports per-group position without breaking existing consumers (Priority: P2)

An operator or automated consumer (including orac, which depends on today's flat `wal.applied_seq`/`wal.max_seq`) calls `knowledge_status` on a multi-group instance and can see each group's position, while a caller that only knows about the old flat shape keeps working unchanged.

**Why this priority**: Observability is what makes multi-stream operable rather than just correct. It's P2 rather than P1 because it depends on User Stories 1–2 being in place first, and because an existing external consumer's compatibility must be preserved deliberately rather than incidentally.

**Independent Test**: Call `knowledge_status` against an instance with two active groups; confirm a per-group breakdown is present and correct, and confirm the existing flat `wal.applied_seq`/`wal.max_seq` fields are still present and report the default group's position exactly as a pre-378 caller would have seen. Separately, call it against an instance that has never written to the default group at all, and confirm the flat fields report absence rather than an error.

**Acceptance Scenarios**:

1. **Given** a multi-group instance, **When** `knowledge_status` is called, **Then** the response includes a per-group breakdown of `applied_seq` and `max_seq` for every group that has a WAL directory (FR-007).
2. **Given** a group with no WAL directory yet, **When** `knowledge_status` is called, **Then** that group (if reported at all, e.g. because it has graph content but no writes) shows `applied_seq: null`, distinct from a group at position 0.
3. **Given** an existing consumer (e.g. orac) that only reads the flat `wal.applied_seq`/`wal.max_seq` fields, **When** it calls `knowledge_status` against an upgraded instance, **Then** those fields are still present and report the default group's position unchanged — no consumer-side code change is required for the single-group case (FR-009, FR-007).
4. **Given** a pure-replica instance that has only ever hydrated groups other than the default `"liminis"` group (which therefore has no WAL directory), **When** `knowledge_status` is called, **Then** the flat fields report `null`/absent rather than an error — a documented signal that this instance has no default group, not a symptom of a broken or un-hydrated instance (FR-007).

---

### Edge Cases

- **The layer group is a stream whose correctness depends on other streams.** Its own WAL is self-contained and independently replayable, but its *state* is not: its edges' endpoints live in other groups, so replaying the layer group into a fresh database yields `unbound` edges (per #369) until those other groups are present and a re-bind runs. This is the one place "independent streams" is not literally true end to end, and it is documented here rather than left to be discovered.
- **A group with no writes yet has no directory.** `knowledge_status` and replay must treat "no stream" (directory absent) distinctly from "stream at position 0" (directory present, `WalPosition.applied_seq = 0`) — the same `None`/`Some(0)` discipline #362 and #365 already established, now applied per group instead of once globally.
- **Concurrent writes to different groups from one instance.** N writers, each with its own `global_seq`, must not serialize on each other's write lock merely because they share an instance — only same-group writes need to serialize against each other.
- **A `group_id` that does not already satisfy `checkpoint::validate_name`'s rule.** Since `group_id` becomes (indirectly, via FR-005's mapping) a filesystem path component, a value containing `.`, `:`, whitespace, or other filesystem-unsafe characters must still resolve to a usable, collision-free directory — including for a `group_id` that was already in use as a free-form graph-data label before this issue existed, which Out of Scope declines to migrate (see FR-005).
- **An instance with no default (`"liminis"`) group at all.** A pure replica that only ever hydrates non-default groups has no WAL directory for `"liminis"`. `knowledge_status`'s flat fields must report this as absence, not an error (see User Story 6, Acceptance Scenario 4).
- **Episode-cursor backfill in a multi-group database.** Today's backfill (`recovery.rs`'s `derive_episode_cursor` / `get_latest_episode_uuid`) finds the single most-recently-created `Episodic` node across the *entire* database, with no `group_id` filter. In a multi-group database this node may belong to a different group than the one being backfilled, which would derive the wrong position or fail to find a match at all. This must be scoped per group (see FR-010).
- **Migrating more than the WAL files themselves.** `LCG_WAL_DIR` holds `.checkpoints/` (#365) and `.wal-bounds.json` (#375) alongside `*.jsonl` files. A migration that moves only the `*.jsonl` files leaves every existing checkpoint's reachability computed against an emptied directory, silently reporting `reachable: false` for artifacts whose entire purpose is surviving exactly this kind of loss (see FR-001).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A WAL **root** directory contains one subdirectory per `group_id`. `LCG_WAL_DIR`'s current meaning (a single stream) MUST migrate to this layout with a defined, automatic path for an existing single-stream workspace — no operator action required to preserve a 0.12.2 deployment's behavior (User Story 3). Migration MUST relocate the existing WAL directory's **entire contents** into the default group's subdirectory as a unit — not only its `*.jsonl` WAL files, but also `.checkpoints/` (#365) and `.wal-bounds.json` (#375) — so that every existing checkpoint's reachability and the seq-bounds manifest remain correct relative to their WAL files' new location. Moving the `*.jsonl` files alone would leave every existing checkpoint reporting `reachable: false` (its `wal_min_seq`/`wal_max_seq` computed against an emptied directory) with no error raised — silent loss of exactly the artifact `.checkpoints/` exists to survive.
- **FR-002**: There is one `WalPosition` row per group, keyed by `group_id`. The `'singleton'` literal in `db.rs:1758`/`:1772` MUST stop being hardcoded.
- **FR-003**: `AppState` holds a per-group `WalWriter` map with lazy creation — the first write to a group creates its directory and writer, without requiring every group to be known or pre-declared at startup.
- **FR-004**: Mutations are routed to the writer of the group they belong to, using **per-operation attribution**: every write handler already names exactly one group in scope at its flush site (e.g. `episode.rs`'s `add_episode` has `gid_owned`). This is sufficient *only* because #371 (merged) stopped `corrections::merge_entities_inner` from writing across groups within a single `drain_mutations` call — mutation-level attribution (tagging each individual mutation with its group as it flows through `Conn::executed_mutations` → `drain_mutations` → `wal_exec::wal_flush_*`) is explicitly not required and MUST NOT be designed, since that path carries no group information today and adding it would be substantially more invasive than this issue needs.
- **FR-005**: `group_id` MUST be mapped to a WAL directory name via a transformation that is both **reversible** and **collision-free** before it is used to create or open a WAL directory. Specifically:
  - A `group_id` that already satisfies `checkpoint::validate_name`'s rule (`crates/core/src/checkpoint.rs:75` — ASCII alphanumeric plus `_`/`-`, non-empty, ≤200 chars) MUST use itself as the directory name, unchanged. This covers the default `"liminis"` group and every `group_id` chosen with this rule in mind, and keeps the common case human-readable on disk with no transformation cost.
  - A `group_id` that does **not** satisfy that rule (e.g. one already in use as a free-form graph-data label before this issue existed, containing `.`, `:`, whitespace, or similar) MUST still be usable to create and open a WAL directory: it MUST be mapped to a distinct directory name via a **bijective encoding**, not rejected outright and not lossily sanitized. Lossy sanitization (stripping or replacing unsafe characters) was rejected because it can collide two distinct `group_id` values onto one directory and decouples the on-disk name from the stored value; a bijective encoding has neither problem, since it is a total, invertible function of the input. The specific encoding scheme (e.g. percent-encoding of exactly the unsafe characters) is left to Plan.
  - Only a `group_id` that is empty, or whose length (before or after transformation) exceeds `validate_name`'s bound, MUST still fail loudly at the point it is first used to create or open a WAL directory — there is no meaningful directory name for those.
- **FR-006**: `knowledge_rebuild_from_wal` MUST target exactly one group without disturbing any other group's `WalPosition` row. Replaying group B MUST NOT reset or advance group A's position (User Story 2, Acceptance Scenario 2).
- **FR-007**: `knowledge_status` MUST report per-group `applied_seq`/`max_seq` for every group that has a WAL directory, via a new field (e.g. a map keyed by `group_id`, each value shaped like today's `{applied_seq, max_seq}`) additive to the response. The existing flat top-level `wal.applied_seq`/`wal.max_seq` fields (`handlers.rs` around line 422) MUST remain present and MUST continue to report the default group's (`"liminis"`) position exactly as a pre-378 single-group instance would — so an existing consumer that only reads the flat fields (e.g. orac) requires no change. The flat fields are not deprecated or repurposed to mean "whichever group was written to most recently" — they are pinned to the default group specifically, which keeps their meaning unambiguous regardless of how many other groups are active. If the default group has no WAL directory at all (e.g. a pure replica that has only ever hydrated non-default groups), the flat fields MUST report absence (`null`), not an error — a documented signal that this instance has no default group, distinguishable from a broken or un-hydrated instance.
- **FR-008**: A `seq` from one group MUST NEVER be compared against, or written to, another group's `WalPosition` row.
- **FR-009**: A single-group instance MUST behave exactly as it does in 0.12.2. This is the common case and the master topology, and every other requirement in this spec is additive to it, not a replacement for it.
- **FR-010**: #353's episode-cursor backfill (its FR-007) MUST remain correct once multiple groups can share one database. `derive_episode_cursor`'s lookup of the most-recently-created `Episodic` node (today unscoped, `db.rs`'s `get_latest_episode_uuid`) MUST be scoped to the target group when backfilling that group's position, so a backfill for group A cannot select an episode belonging to group B. If a group's own episode set is empty, that group's backfill degrades explicitly (leaves the position `None`/unknown, consistent with the existing "backfill failed, full rebuild required" signal) rather than falling back to a different group's episode.
- **FR-011**: After a per-group replay (not only a full purge-and-rehydrate), cross-group pointers (#369) *into* that group MUST be re-bindable, using that group's own newly-advanced applied position as the staleness signal for #369's re-bind pass. #369's FR-007 framed re-bind around the purge-and-rehydrate cycle specifically; this issue extends the same mechanism to ordinary incremental replay, which the per-group applied position this issue introduces is what makes precise (User Story 4).
- **FR-012**: `knowledge_wal_mark_create`, `knowledge_wal_mark_list`, and `knowledge_wal_mark_delete` (#365) MUST accept a `group_id` parameter, defaulting to `"liminis"` like every other group-scoped operation (preserving FR-009's single-group parity with zero required caller change). Each MUST resolve and operate against that group's own WAL directory instead of `require_wal_dir`'s current single-directory assumption (`handlers.rs:1514`, used by all three at `:1535`/`:1581`/`:1597`). `knowledge_wal_mark_list`, when called without an explicit `group_id`, MUST return only the default group's checkpoints — matching today's single-stream behavior exactly — rather than aggregating across every active group. This issue does not introduce a "list checkpoints across all groups" aggregate mode; an operator inspects one group's checkpoints at a time, the same way `knowledge_wal_mark_create`/`_delete` already operate on one target.

### Key Entities *(if the feature involves data)*

- **WAL root directory**: the top-level directory (replacing today's single `LCG_WAL_DIR` meaning) that contains one subdirectory per `group_id` — each subdirectory is a self-contained, independently-sequenced WAL stream, including its own `.checkpoints/` store and `.wal-bounds.json` manifest.
- **Per-group WAL directory / stream**: one group's WAL files, with `global_seq` numbering that starts at 0 (or resumes from `scan_max_seq`) independently of every other group's directory. Two groups legitimately reuse the same `seq` values; they are never comparable across streams.
- **`WalPosition` row**: one row per `group_id` (primary key already supports this), holding that group's `applied_seq` — the position that group's stream has been replayed up to.
- **Per-group `WalWriter`**: one writer instance per group, held in a map on `AppState`, created lazily on first write to that group.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Two groups hydrate into one database from two WAL directories; each reports its own `applied_seq`, and both match their respective `max_seq`.
- **SC-002**: An incremental replay of group B leaves group A's position byte-identical.
- **SC-003**: Two groups whose seq ranges overlap (both starting at 0) hydrate without either position being corrupted by the other.
- **SC-004**: An instance accepts writes to two different `group_id`s and each mutation lands in its own group's WAL — asserted against WAL file content, not only against graph state.
- **SC-005**: A 0.12.2 database and its single WAL directory (including its `.checkpoints/` store and `.wal-bounds.json` manifest) open and report position unchanged after upgrade, with every existing checkpoint's reachability preserved.
- **SC-006**: A cross-group edge from group L into group B survives an incremental replay of B and re-binds correctly afterward.
- **SC-007**: Per-group checkpoints (#365), created and listed via an explicit `group_id`, work independently — a checkpoint in group A's stream is unaffected by anything happening in group B's.

## Assumptions

- **Groups are not necessarily disjoint.** Cross-group edges exist by design (#369). This replaces #360's assumption of "no cross-source edges, no shared entities."
- **Replicas are not necessarily read-only.** A single instance may accept writes to several groups. This replaces #360's assumption that writes happen only against a single-source master.
- **Entity UUIDs remain `Uuid::new_v4()` and per-instance**, so two groups never collide on identity — they produce distinct nodes for the same real-world entity, and that duplication is accepted; cross-group pointers (#369) are how a relationship between them is expressed.
- **FR-005's directory-naming policy** (self-name when already path-safe; bijective encoding otherwise; fail loudly only for empty or over-length values) is a revised Specify-stage judgment call. An earlier draft of this spec rejected sanitization and, on the same reasoning, rejected any transformation at all — but the objections to lossy sanitization (collision risk, decoupling the on-disk name from the stored value) do not apply to a bijective encoding, which cannot collide and is fully invertible by construction. Pure fail-loud-on-invalid was also considered and rejected on review: `group_id` was already a free-form graph-data label with no filesystem meaning before this issue, so a deployment that chose a filesystem-unsafe `group_id` before this issue shipped would otherwise become permanently unable to add new WAL-backed writes to that group, with no migration path (Out of Scope explicitly declines to migrate stored `group_id` values). The specific encoding scheme is left to Plan.
- **FR-007's response shape** (additive per-group map, flat fields pinned to the default group, absence reported as `null` when the default group doesn't exist) is chosen specifically to make orac's existing integration require zero changes, per the issue's own note that orac depends on today's flat shape. The alternative — replacing the flat fields with a list/map and requiring every consumer to update — was rejected as an unnecessary breaking change when an additive shape achieves the same observability. Also a judgment call the issue left open, and open to revision the same way.
- **FR-010's backfill scoping** (filter the latest-episode lookup by target group) is the direct consequence of introducing per-group `WalPosition` rows — the alternative (leaving the lookup unscoped) is not a legitimate design option, since it would make the backfilled position depend on which group happened to write most recently, which is exactly the cross-group leakage FR-008 forbids elsewhere. This is stated as a requirement (FR-010) rather than left as an open question because it has only one correct answer given the rest of this spec.
- **FR-012's checkpoint-tool scoping** (require an explicit or defaulted `group_id`, no cross-group aggregate listing) mirrors FR-007's "preserve the default-group behavior for a caller that doesn't know about groups" philosophy, but does not add an aggregate "all groups" mode the way `knowledge_status` does — checkpoint operations are already single-target by nature (`create`/`delete` each name one checkpoint), so `list` follows that same single-target shape rather than introducing an asymmetric aggregate.

## Out of Scope

- **Group-scoped purge / per-stream reset** — #361.
- **Merge's cross-group write behaviour** — already delivered by #371 (merged); this issue depends on it (FR-004) but does not re-implement it.
- **Cheap WAL seq bounds** — #375.
- **The "channel" concept** (mapping a git repo to a logical knowledge stream) stays in orac/zen. Nothing here introduces channel vocabulary into lcg.
- **Mutation-level (as opposed to per-operation) group attribution.** Explicitly rejected as unnecessary — see FR-004.
- **Changing how `group_id` values already stored on graph nodes/edges are represented.** This issue only adds a (possibly encoded) filesystem-path mapping at the point a `group_id` is first used to create or open a WAL directory; it does not migrate or revalidate `group_id` values already persisted as graph data.
- **A "list checkpoints across all groups" aggregate mode.** `knowledge_wal_mark_list` operates on one group at a time (explicit or defaulted); see FR-012.

## Source References

- `crates/core/src/db.rs:1755`–`:1772` (`get_applied_seq`/`set_applied_seq`, the hardcoded `'singleton'` `WalPosition` key)
- `crates/core/src/app_state.rs:45` (`wal_dir: Option<PathBuf>`), `:49` (`wal_writer: Arc<Mutex<Option<WalWriter>>>`)
- `crates/core/src/episode.rs:538` (`gid_owned`, the per-operation group attribution FR-004 relies on)
- `crates/core/src/checkpoint.rs:75` (`validate_name`, the path-safety precedent FR-005 reuses for the "already safe" case)
- `crates/core/src/recovery.rs:56` (`derive_episode_cursor`), `:155` (`backfill_applied_seq_if_absent`); `crates/core/src/db.rs:1741` (`get_latest_episode_uuid`, today unscoped by group — see FR-010)
- `crates/core/src/handlers.rs` (`knowledge_status`'s flat `wal.applied_seq`/`wal.max_seq` fields, ~line 422; `group_id` defaulting to `"liminis"`; `:1514` `require_wal_dir`; `:1525`/`:1580`/`:1590` the three `handle_wal_mark_*` functions — see FR-012)
- `.wal-bounds.json` (#375, seq-bounds manifest) and `.checkpoints/` (#365) as siblings of `*.jsonl` files inside a WAL directory — see FR-001
- #360 — superseded (closed; same topology, opposite key, two retired assumptions)
- #369 — resolvable semantic pointers for cross-graph references (merged; re-bind mechanism FR-011 extends to incremental replay)
- #365 — WAL checkpoints (merged; becomes per-stream for free at the storage layer once directories are per-group, but the IPC methods need FR-012's explicit `group_id`)
- #368 — merge duplicate-detection scoping (merged)
- #371 / PR #377 — merge stops writing across groups (merged; hard dependency for FR-004)
- #353 — persisted `applied_seq` and episode-cursor backfill (FR-010 extends its FR-007 to be group-scoped)
- #362 — bounded WAL replay, `None`/`Some(0)` position discipline
- #361 — group-scoped purge (out of scope here, but consumes this issue's per-group `WalPosition`)
- #373 — per-group `knowledge_rebuild_from_wal` targeting (FR-006's origin)
- #375 — cheap WAL seq bounds (out of scope; its `.wal-bounds.json` manifest is one of the artifacts FR-001's migration must relocate)
