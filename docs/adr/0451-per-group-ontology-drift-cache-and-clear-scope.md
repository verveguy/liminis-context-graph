# ADR-0451: Per-Group Ontology Drift — Cache Placement and Clear Scope

**Status**: Accepted
**Date**: 2026-08-23
**Issue**: #451
**Relates to**: ADR-0018 (ontology hash sidecar), ADR-0446 (per-group ontology resolution, Decision
1 and Decision 3)

## Context

ADR-0446 gave `liminis-context-graph` per-group ontology resolution but explicitly left drift
detection workspace-scoped (Decision 3), naming the gap as "a deliberate v1 limitation, not an
oversight, and a natural (but unscoped) follow-up." This issue is that follow-up: a group governed
only by a per-group ontology file (or one that falls back to the workspace ontology) gets a drift
signal, scoped to that group, computed on the same lazy first-use trigger `resolve_ontology`
already established.

Two design decisions in this issue are not obvious from reading the code alone and would be easy
for a future contributor to get wrong by analogy to adjacent, superficially similar code. Both are
recorded here.

## Decision 1: Fold drift into `group_ontologies`'s existing value type, not a new `AppState` field

**Chosen**: `AppState.group_ontologies`'s value type changes from `Option<Arc<Ontology>>` to a new
`GroupOntologyEntry { ontology: Option<Arc<Ontology>>, drift: GroupDriftStatus }`. The field itself
keeps its existing name and stays a `Arc<Mutex<HashMap<String, GroupOntologyEntry>>>`.

**Rejected**: a new sibling field, e.g. `AppState.group_drift: Arc<Mutex<HashMap<String,
GroupDriftStatus>>>`, computed and populated independently of `group_ontologies`.

**Rationale**: ADR-0446's Decision 1 already established the precedent of adding
`group_ontologies` as a new field, and paid the associated cost once — every `AppState { ... }`
literal construction site (54 at the time of this issue: tests, one bench, `wal_exec.rs`'s test
helper) needed a new line. A second new field would pay that cost again for no structural benefit:
per-group drift and per-group ontology resolution are computed together, at the same trigger point
(`resolve_ontology`'s first call for a `group_id`), under the same lock, with the same lifetime —
there is no scenario where one is populated without the other. Folding drift into the existing
entry type is a **zero-touch** change: every one of those 54 sites writes
`group_ontologies: Arc::new(Mutex::new(HashMap::new()))`, with the value type inferred entirely
from the field's declaration — none of them names `GroupOntologyEntry` (or the old
`Option<Arc<Ontology>>`) explicitly, so changing what the map's values *are* requires no edit at
any call site. This was verified directly: `cargo check --workspace --all-targets` succeeded with
zero changes to any `AppState` literal after this issue's `app_state.rs` edits.

The coupling this introduces — one cache entry now serves two concerns — is judged acceptable
specifically because those two concerns already share a lifecycle. A future feature that needs
per-group state computed on a *different* trigger, or with a materially different lifetime, should
not default to extending `GroupOntologyEntry` by the same reasoning; it should evaluate its own
tradeoff on its own terms.

## Decision 2: A remediation clears only the group it remediated, not every cached group

**Chosen**: the per-group clear added at all three existing clear-sites
(`handle_rebuild_from_wal`'s streaming and background-job paths in `handlers.rs`, and
`add_episode`'s post-ingest clear in `episode.rs`) is scoped to the one `group_id` each site
already operates on. `AppState::clear_group_drift(group_id)` resets exactly that group's cached
`GroupDriftStatus`; every other group's cached status is left untouched.

**Rejected**: clearing every group's cached drift status whenever any one group's rebuild or
ingest succeeds — mirroring the *pre-existing* behavior of the workspace-level clear at the same
three sites, which resets `AppState.ontology_drift` (a single, workspace-wide value with no
per-group dimension) unconditionally on every successful ingest or rebuild, regardless of which
group triggered it.

**Rationale**: this issue's own User Story 2 and FR-003 establish per-group isolation as a
first-class guarantee on the *setting* side — changing group A's ontology must never report drift
for group B. Clearing every cached group's drift on group A's remediation would violate the same
guarantee on the *clearing* side: a legitimately-drifted group B would have its drift silently
erased by an operator action that had nothing to do with B, with no record that anything happened
to it. The spec text ("clearing MUST extend to every group's per-group drift state, not the
workspace-level state alone") is compatible with either reading — "every group" versus "every
group actually remediated, as opposed to only the workspace-level flag" — and User Story 5's own
Independent Test only exercises a single group, so it cannot disambiguate between them from test
evidence alone. The workspace-level sites' existing all-groups-affected-by-one-group's-remediation
behavior is pre-existing and out of this issue's scope to fix (there is no per-group dimension to
narrow there — `ontology_drift` is a single value by construction), but it must not be copied
forward into the new per-group mechanism this issue adds, where a narrower, more correct scope is
directly available at no extra cost (`resolve_ontology`/`clear_group_drift` are already
group-scoped functions).

A future contributor extending one of these three clear-sites — or adding a fourth — should scope
any new per-group clear to the group actually being remediated, not iterate over every cached
group, even though the adjacent workspace-level clear at the same call site does exactly that.

## Consequences

- `AppState.group_ontologies`'s value type change is invisible to every existing caller of
  `resolve_ontology` (still returns `Option<Arc<Ontology>>`) and to every `AppState { ... }`
  construction site — the only code that reads or writes `GroupOntologyEntry`'s `drift` field
  directly is `resolve_ontology` and the three new accessor methods
  (`group_drift_status`, `all_group_drift_statuses`, `clear_group_drift`), all in `app_state.rs`.
- `knowledge_status`'s new `group_ontology_drift` array is sourced purely from
  `all_group_drift_statuses()` — an in-memory read of the same cache, never a disk scan — so a
  group this process has never resolved is simply absent from the array, distinguishable from a
  present entry with `drifted: false` (User Story 4, Scenario 2).
- Review/Validate should specifically check that the isolation tests in
  `crates/core/tests/per_group_ontology.rs` assert the negative case for Decision 2 (remediating
  group A does not clear group B's drift) — this is the one place this issue's behavior
  deliberately diverges from the adjacent, pre-existing workspace-level pattern at the same call
  sites, so it's the one place a future refactor is most likely to "fix" back to the wrong
  behavior by analogy.
