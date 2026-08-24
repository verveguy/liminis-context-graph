# Feature Specification: Per-Group Ontology Drift Detection

**Feature Branch**: `fabrik/issue-451`
**Created**: 2026-08-23
**Status**: Specified
**Input**: User description: "Ontology drift detection does not cover per-group ontologies"

## Background

Drift detection (#83) exists to catch one specific hazard: the ontology changed while the database
still holds data ingested under the previous vocabulary, so the graph's entity and relation types no
longer match the rules that produced them. On startup, `AppState::from_env` hashes the loaded
ontology and compares it against `.lcg/ontology-hash.json`; a mismatch warns and recommends
Recreate + re-ingest (`crates/core/src/app_state.rs`, `crates/core/src/ontology_sidecar.rs`).

#446 added per-group ontology resolution: `.lcg/ontology/<encoded_group_id>.yaml` when present,
falling back to the workspace-wide `ontology.yaml`. It deliberately did not change drift detection
— ADR-0446's Decision 3 states this explicitly: `ontology_sidecar.rs` and `AppState.ontology_drift`
stay workspace-scoped, and "a group governed only by a per-group ontology file gets no drift warning
today if that file changes across a restart." ADR-0446 calls this "a deliberate v1 limitation, not
an oversight, and a natural (but unscoped) follow-up." This issue is that follow-up.

The result today:

- **Workspace ontology changes** → drift detected, as before.
- **Per-group ontology changes** → **no drift signal**. The workspace hash is unchanged, so nothing
  compares, warns, or records anything. Group A's data can be arbitrarily stale relative to A's own
  vocabulary with no indication.

Before #446 the drift check covered the entire ontology surface. It now covers only the fallback.
Nothing regressed — the check is exactly as correct as it was — but the surface it guards grew and
it did not grow with it.

#446's own spec argued for per-group *files* (over in-file multi-group scoping) partly on this
basis: per-group files give per-group drift hashes naturally, as an affordance. That affordance was
never turned into an implementation — this issue implements it.

**Why it matters**: the failure is silent and only surfaces later as inexplicable data. An operator
edits `.lcg/ontology/channel-a.yaml`, restarts, re-ingests, and gets a graph mixing entities typed
under two different vocabularies with no warning at any point — the exact scenario drift detection
was built to prevent. The per-group case is if anything more likely than the workspace one:
per-group ontologies exist precisely so they can be tuned per channel, so they will be edited more
often than a single shared file.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Per-group drift is detected and reported (Priority: P1)

An operator edits a group's per-group ontology file, or the workspace ontology it falls back to,
between service restarts. The next time that group's ontology is used, a drift signal identifies
that specific group as stale.

**Why this priority**: This is the core capability the issue exists to deliver — without it, the
per-group ontology surface added by #446 remains completely unmonitored for drift.

**Independent Test**: Configure group A with a per-group ontology file, start the service, use group
A (so its ontology is resolved), stop the service, edit A's per-group file, restart, use group A
again, and verify a drift signal names group A specifically.

**Acceptance Scenarios**:

1. **Given** group A has a per-group ontology file that changes between restarts, **When** group A's
   ontology is next resolved (e.g. on its next `knowledge_add_episode`), **Then** drift is detected
   and reported for group A specifically.
2. **Given** group B has no per-group file and falls back to the workspace ontology, and the
   workspace `ontology.yaml` changes between restarts, **When** group B's ontology is next resolved,
   **Then** drift is detected and reported for group B.

---

### User Story 2 - Drift isolation across groups (Priority: P1)

Changing one group's per-group ontology never produces a drift signal for an unrelated group.

**Why this priority**: Per-group isolation is the entire point of #446's per-group ontology model —
a drift mechanism that leaks across groups would misdirect operators toward the wrong group's data
and undermine the isolation #446 built.

**Independent Test**: Configure per-group files for groups A and B; change only A's file between
restarts; use both groups; verify only A shows drift.

**Acceptance Scenarios**:

1. **Given** per-group files exist for groups A and B, **When** only A's file changes and both
   groups are used after restart, **Then** drift is reported for A only; B shows no drift.
2. **Given** group C falls back to the workspace ontology and group A has its own per-group file,
   **When** A's per-group file changes but the workspace ontology does not, **Then** C shows no
   drift.

---

### User Story 3 - Existing single-ontology workspaces are unaffected (Priority: P1)

A workspace that has not adopted per-group ontologies (no `.lcg/ontology/` directory) behaves
identically to today: one workspace-level drift signal, unchanged sidecar shape.

**Why this priority**: Backward compatibility is required — this is the majority of existing
deployments, and FR-004 makes a regression here a blocking bug, not a nice-to-have.

**Independent Test**: Run the existing single-ontology drift tests unchanged; confirm
`.lcg/ontology-hash.json`'s content/shape and `knowledge_status`'s `ontology` field are unchanged
from pre-#451 behavior.

**Acceptance Scenarios**:

1. **Given** a workspace with only `.lcg/ontology.yaml` and no `.lcg/ontology/` directory, **When**
   the ontology changes between restarts, **Then** behavior (warning text, sidecar file shape,
   `knowledge_status.ontology` fields) is identical to pre-#451 behavior.

---

### User Story 4 - Operator can see which groups are stale via `knowledge_status` (Priority: P2)

An operator queries `knowledge_status` and sees, for each group that has been used in the running
process, whether that group's resolved ontology has drifted.

**Why this priority**: FR-002 requires per-group reporting, not just a per-group stderr line an
operator may not have been watching for. `knowledge_status` already reports comparable per-group
state (WAL positions), so this is the natural place to make drift queryable and durable within the
process's lifetime.

**Independent Test**: Use two groups, drift one of them, query `knowledge_status`, and verify its
response distinguishes the two by `group_id`.

**Acceptance Scenarios**:

1. **Given** groups A (drifted) and B (not drifted) have both been used in the current process,
   **When** `knowledge_status` is queried, **Then** its response distinguishes A as drifted and B as
   not, by `group_id`.
2. **Given** group C has not yet been used in the current process, **When** `knowledge_status` is
   queried, **Then** C is not falsely reported as "not drifted" — its status is reported as
   not-yet-computed rather than a false negative.

---

### User Story 5 - Drift clears after remediation (Priority: P2)

After an operator follows the recommended remediation (Recreate + re-ingest) for a drifted group,
that group's drift no longer shows as drifted.

**Why this priority**: Without this, a resolved problem keeps warning, training operators to ignore
the signal — undermining the same trust User Story 1 depends on.

**Independent Test**: Drift group A, run the existing WAL rebuild/replay remediation, and verify A's
drift clears afterward.

**Acceptance Scenarios**:

1. **Given** group A is reported as drifted, **When** the recommended remediation completes
   successfully, **Then** A's drift status clears and is no longer reported as drifted.

---

### Edge Cases

- No `.lcg/ontology/` directory exists at all (no group has ever had a per-group file) — behaves
  exactly as pre-#451 (User Story 3).
- A group's per-group file is *deleted*, causing that group to fall back to the workspace ontology —
  this is a change to that group's resolved ontology (FR-001) and MUST be detected as drift for that
  group, even though the group's own per-group file didn't change content — it started resolving
  through a different path entirely.
- Two groups both fall back to the workspace ontology, and the workspace ontology changes — both
  groups MUST show drift, not just one (FR-002/FR-003 together: one changed source, every group that
  resolves through it is reported).
- A group is used for the first time under this feature, but the DB already contains data for it
  from before per-group drift tracking existed — mirrors the existing `has_prior_data` pre-#98
  migration case `compute_drift` already handles for the workspace ontology; needs an analogous
  per-group answer so upgrading doesn't silently skip a legitimately-stale group.
- A group's per-group ontology file is malformed or unreadable — per #446 this already falls back to
  the workspace ontology at resolution time. Drift detection MUST follow whatever ontology was
  actually resolved (the fallback), not the broken file, consistent with FR-001's "resolved
  ontology" framing.
- Drift is computed lazily and cached for the process's lifetime (FR-007), mirroring
  `AppState::resolve_ontology`'s own caching — a group's drift status reflects the ontology at that
  group's *first* use since the last restart, and does not change again mid-process even if the file
  is edited again while the service keeps running.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A change to a group's resolved ontology MUST be detected as drift for that group,
  whether the change is to its own per-group file, to the workspace file it falls back to, or to
  which of the two it resolves through (e.g. its per-group file is added or removed).
- **FR-002**: Drift MUST be reported per group: (a) as a per-group entry in `knowledge_status`'s
  response distinguishing drifted groups from non-drifted ones by `group_id`, and (b) as a
  stderr warning at the point drift is computed for that group, naming the group — mirroring the
  existing workspace-level warning.
- **FR-003**: A change to one group's per-group ontology MUST NOT be reported as drift for any other
  group, including groups that fall back to the workspace ontology and are unaffected by that
  change.
- **FR-004**: Existing single-ontology workspaces (no `.lcg/ontology/` directory) MUST behave exactly
  as they do today: identical `.lcg/ontology-hash.json` content and shape, identical
  `knowledge_status.ontology` top-level fields, identical stderr warning text for the workspace-level
  case. Upgrading to a version with per-group drift detection MUST NOT produce a spurious drift
  warning for such a workspace.
- **FR-005**: The drift storage/tracking mechanism MUST accommodate an arbitrary number of groups.
  The storage shape (a single map, a file per group, or another structure) is a design decision left
  to Research/Plan.
- **FR-006**: `docs/ontology.md` MUST state what drift detection covers: that it is computed per
  group, covers both dedicated per-group files and workspace-fallback groups, and describe when a
  given group's drift status becomes available (see FR-007), so the coverage gap this issue closes
  is not rediscovered by a future reader relying on a warning that never came for an unused group.
- **FR-007**: A group's drift MUST be computed lazily — the first time that group's ontology is
  resolved in the running process (the same trigger point as `AppState::resolve_ontology`'s
  cache-populate step), not via an eager startup scan of `.lcg/ontology/*.yaml`. A group not yet
  resolved in the current process has no drift status ("not yet computed"), which MUST be
  distinguishable from "not drifted" (User Story 4, Scenario 2).
- **FR-008**: The workspace-level drift computation that exists today — computed eagerly at startup,
  independent of any group — MUST remain unchanged in timing and behavior. FR-007's lazy model
  applies only to the new per-group drift tracking layered on top of it.
- **FR-009**: When drift is cleared by the existing remediation path (a successful WAL rebuild/replay,
  which today clears the workspace-level drift state), that clearing MUST extend to every group's
  per-group drift state, not the workspace-level state alone.
- **FR-010**: A group whose resolved ontology has never had a recorded hash before (no prior
  per-group or workspace-fallback record for it), but whose data already exists in the DB, MUST be
  treated as drifted once an ontology is resolved for it — the per-group generalization of the
  existing `has_prior_data` handling `compute_drift` already applies at the workspace level.

### Key Entities *(if the feature involves data)*

- **Per-group drift record**: the persisted state (whether a per-group entry within a shared
  structure or a separate per-group artifact — FR-005) recording the hash, mode, entity types, and
  relation types of the ontology last known to be reflected in a group's ingested data. The
  per-group generalization of today's single-valued `.lcg/ontology-hash.json` / `OntologySidecar`.
- **Group drift status**: the runtime classification for one group at a point in time —
  not-yet-computed, not drifted, or drifted (with a summary) — reported via `knowledge_status` and
  stderr (FR-002).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Editing only group A's per-group ontology file and restarting produces a drift signal
  (stderr + `knowledge_status`) scoped to group A the next time A is used, with zero drift signal
  for any other co-resident group.
- **SC-002**: Editing the workspace `ontology.yaml` produces a drift signal for every group that
  falls back to it, and none for groups with their own unaffected per-group file.
- **SC-003**: A workspace with no `.lcg/ontology/` directory shows no behavioral difference from
  pre-#451 — existing drift tests for the workspace-only case pass unchanged, and
  `.lcg/ontology-hash.json`'s content is unchanged.
- **SC-004**: Following the documented remediation (Recreate + re-ingest / WAL rebuild) for a
  drifted group clears that group's drift status, verified via `knowledge_status`.
- **SC-005**: `docs/ontology.md` contains an explicit statement of per-group drift coverage and its
  lazy, use-triggered timing, so a reader does not have to infer it from source.

## Assumptions

- **Lazy, use-triggered per-group drift computation was chosen over an eager startup scan of
  `.lcg/ontology/*.yaml`.** The issue's own Open Questions section named this as "the crux of the
  issue" to be settled in Specify. Lazy computation mirrors `AppState::resolve_ontology`'s existing
  cache-on-first-use pattern (#446), avoids paying a scan cost for groups a process never touches
  (the same cost #446 explicitly declined to pay for resolution itself), and still surfaces drift
  before that group's next extraction commits under the (possibly changed) resolved ontology.
  Trade-off accepted: a group's drift status is unavailable until that group is first used in the
  running process (FR-007, User Story 4 Scenario 2) — this is the same staleness window #446's own
  lazy resolution already accepts for ontology loading itself.
- **`knowledge_status` is extended with a per-group drift breakdown.** The issue's second open
  question asked whether drift should be reported through `knowledge_status`; this is accepted as
  in-scope response-shape growth. ADR-0446 Decision 3 explicitly deferred this ("a
  `knowledge_status` IPC protocol change this issue's Plan explicitly chose not to make") as a
  natural but unscoped follow-up — this issue is that follow-up, and `knowledge_status` already
  reports comparable per-group state (`wal_group_positions`), so extending it is precedented rather
  than novel.
- **The exact on-disk storage shape for N groups' drift hashes is a Research/Plan decision (FR-005),
  not decided here.** Only the requirement that it accommodate N groups without breaking the
  single-ontology case (FR-004) is fixed at this stage.
- Groups never resolved during a process's lifetime have no drift status ("not yet computed," not
  "not drifted") — consistent with the lazy model (FR-007). An operator wanting a complete drift
  picture across all groups must ensure each group is exercised in the running process, or query
  after normal use has touched them.

## Out of Scope

- An eager startup scan of `.lcg/ontology/*.yaml` for drift purposes — explicitly rejected in favor
  of the lazy, use-triggered model (FR-007).
- A dedicated IPC method to force drift computation for a group that hasn't been resolved/used yet.
  Per-group drift availability follows the same lazy trigger as ontology resolution itself.
- Runtime hot-reload of ontology files — already out of scope per #446 and unaffected by this issue;
  both workspace and per-group ontologies still require a restart to pick up changes.
- Changes to per-group ontology *resolution* behavior itself (file location, fallback rules, the
  known case-insensitive filename collision limitation documented in `docs/ontology.md`) — this
  issue only adds drift detection on top of the existing #446 resolution model; it does not modify
  resolution.

## Source References

- #83 — workspace-scoped ontology and the original drift sidecar.
- #446 / ADR-0446 (`docs/adr/0446-per-group-ontology-resolution.md`, Decision 3) — per-group
  ontology resolution; the feature this issue's gap accompanies, and the ADR that explicitly names
  this gap as a deliberate, unscoped follow-up.
- `crates/core/src/ontology_sidecar.rs` — the single-valued drift sidecar this issue must
  generalise (`compute_drift`, `read_sidecar`, `write_sidecar`).
- `crates/core/src/app_state.rs` — `AppState::resolve_ontology` (the lazy per-group resolution
  pattern FR-007's timing mirrors) and `AppState::from_env` (today's eager workspace-level drift
  computation, FR-008).
- `crates/core/src/handlers.rs` — `handle_knowledge_status` (today's workspace-only `ontology`
  field, and the existing per-group `wal_group_positions` precedent for FR-002) and the two
  WAL-replay drift-sidecar refresh sites that clear drift after a successful rebuild (FR-009).
- `docs/ontology.md` — the "Per-group ontologies" and "`knowledge_status` summary" sections to be
  updated per FR-006.
