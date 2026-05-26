# Feature Specification: Ontology Drift Detection — Notify When the Ontology Has Changed Since the Last Ingestion

**Feature Branch**: `fabrik/issue-98`
**Created**: 2026-05-26
**Status**: Draft
**Input**: User observation 2026-05-26 — "If the user changes the ontology, we should recommend recreating their graph (expensive)." Today's behavior: liminis-graph silently loads whatever ontology.yaml is on disk at startup and applies it to subsequent ingestions, but existing entities/edges in the DB were extracted under whatever ontology was in effect when each chunk was ingested. There's no detection, no notification, no recommendation — the user has no way to know their graph is mixed-vocabulary.

## Background

Ontology support landed in liminis-graph#83 / PR. The current flow:

- liminis-graph loads `<workspace>/.lcg/ontology.yaml` (or `.graphiti/ontology.yaml`) at startup.
- All subsequent ingestions use this ontology to constrain or guide the LLM.
- Existing entities and edges in the DB are NOT re-classified.
- Restart is required to pick up ontology edits (hot-reload was deferred per FR-007 of #83).

The bug-shaped gap: editing `ontology.yaml`, restarting, and continuing to ingest produces a **mixed-vocabulary graph** — some entities/edges typed by the old ontology, some by the new one. This is silent. Users have no signal that:

1. Their on-disk ontology differs from what's already in the graph.
2. Re-ingestion will only partially fix the inconsistency (new chunks land under the new ontology; previously-ingested chunks stay as-is).
3. A full Recreate-and-re-ingest is the only way to bring the entire graph under the current ontology.

Recreate-and-re-ingest is **expensive** — full LLM re-extraction of every chunk (potentially hours, real Anthropic spend). So we want the user to know when it's needed, not perform it speculatively.

### Why this matters

- **Graph integrity.** A mixed-vocabulary graph defeats the point of having an ontology. Queries against the declared vocabulary miss entities/edges that pre-date the change.
- **User trust.** Silent vocabulary drift erodes confidence in the graph. Users edit ontology.yaml expecting their graph to reflect it, see nothing visibly change, and lose faith in the system.
- **Cost transparency.** Recreate is real LLM money. The user should opt into it knowingly, with context about why it's recommended.
- **Onboarding.** New users dropping in an ontology after initial ingestion currently get the worst of both worlds — they think they have a typed graph, but it's only partially typed.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — The Graph Backend Detects Ontology Drift (Priority: P1)

When the liminis-graph service starts (or hot-reloads the ontology, if implemented later), it MUST detect whether the on-disk ontology differs from the ontology that was in effect during the last ingestion event. The detection MUST survive across restarts and across the absence of the file.

**Why this priority**: this is the load-bearing detection; everything else depends on it.

**Independent Test**: With a clean workspace, ingest 5 chunks. Stop the service. Edit `ontology.yaml` (add a new entity type). Restart. Call `knowledge_status`. Assert the response includes an indicator that the ontology has drifted from what the graph was ingested under.

**Acceptance Scenarios**:

1. **Given** a workspace that has ingested under ontology A, **When** the user edits ontology.yaml to ontology B and restarts, **Then** the service detects the drift and reports it via the IPC status surface.
2. **Given** a workspace with no ontology when ingestion happened and now an ontology.yaml exists, **When** the service starts, **Then** drift is reported (added ontology counts as a kind of drift — the existing graph has no types from this ontology).
3. **Given** a workspace that had an ontology when ingesting and now has no ontology.yaml, **When** the service starts, **Then** drift is reported (removed ontology — graph types may no longer match anything declared).
4. **Given** a workspace where ontology.yaml is unchanged since last ingest, **When** the service starts, **Then** no drift is reported.
5. **Given** a workspace where ontology.yaml was edited but only in whitespace / comments (semantically unchanged), **When** the service starts, **Then** no drift is reported (the detection ignores cosmetic edits).

---

### User Story 2 — `knowledge_status` Surfaces Drift Details (Priority: P1)

The `knowledge_status` IPC response MUST include a structured `ontology` field that captures both the currently-loaded ontology summary and the drift status.

**Why this priority**: this is how liminis-app and any other client surface the condition to the user. Without it, the UI can't show anything.

**Acceptance Scenarios**:

1. **Given** the service is running with no drift, **When** `knowledge_status` is called, **Then** the `ontology` field reports `loaded: true|false`, `mode: "open"|"strict"`, counts, and `drifted: false`.
2. **Given** the service is running with drift, **When** `knowledge_status` is called, **Then** `drifted: true` is present along with a summary string identifying what changed (e.g. "entity types added: [Equipment, Reagent]; relation types removed: [PRESENTED]").

---

### User Story 3 — UI Shows a Drift Banner and Recommends Recreate (Priority: P1)

When the liminis-app Knowledge Graph panel renders status, if drift is reported, the UI MUST show a visible banner indicating the ontology has changed and that the graph won't fully reflect the new vocabulary until Recreate + re-ingest.

**Why this priority**: this is the user-facing surface. Detection without UI is invisible.

**Acceptance Scenarios**:

1. **Given** drift is reported, **When** the user opens the Knowledge Graph panel, **Then** a clear banner appears explaining the drift and offering an action (Recreate, Dismiss).
2. **Given** the user clicks Recreate from the banner, **When** the recreate workflow proceeds, **Then** it includes a confirmation dialog mentioning the ontology change (per liminis#824's User Story 3 about Recreate confirmation).
3. **Given** the user dismisses the banner, **When** they next open the panel, **Then** the banner does not reappear within the same session (but does reappear on next service restart while drift is still present).

---

### User Story 4 — Drift Indicator Clears After Recreate Under the New Ontology (Priority: P1)

After the user runs Recreate + re-ingest under the new ontology, the drift indicator MUST clear (because the graph is now under the new vocabulary).

**Acceptance Scenarios**:

1. **Given** drift was reported, **When** the user runs Recreate followed by re-ingestion of all source content, **Then** subsequent `knowledge_status` calls report `drifted: false`.
2. **Given** drift was reported and re-ingestion is partially complete, **When** `knowledge_status` is called mid-stream, **Then** drift MAY still be reported (best-effort — exact semantics about partial re-ingest are a Stage 2 concern).

---

### User Story 5 — Cost-Conscious Action Path (Priority: P2)

The recreate-under-new-ontology path is expensive (full LLM re-extraction). The UI SHOULD give the user enough context to make an informed choice — show the rough scale (number of chunks, estimated time / Anthropic spend if known).

**Why this priority**: P2 because the P1 path is functional without this; cost guidance is polish that becomes critical at scale (10k-chunk workspaces).

**Acceptance Scenarios**:

1. **Given** drift is reported and the user opens the Recreate confirmation dialog, **When** the dialog renders, **Then** it shows the count of chunks that will be re-extracted and a rough order-of-magnitude time estimate.
2. **Given** the workspace has fewer than 100 chunks, **When** the dialog renders, **Then** the time estimate is informative but does not block; the dialog encourages action.
3. **Given** the workspace has more than 1,000 chunks, **When** the dialog renders, **Then** the dialog presents the cost prominently and may suggest waiting until off-hours.

## Requirements *(mandatory)*

- **FR-001.** liminis-graph MUST compute a stable, semantic content hash of the loaded ontology — based on its parsed Rust representation, not the raw file bytes. The hash MUST be unchanged by comment-only or whitespace-only edits but MUST change for any addition / removal / rename / description change of entity types or relation types, or mode changes.
- **FR-002.** When ingestion writes occur (any handler that calls into the extractor), liminis-graph MUST persist the ontology hash of the ontology in effect at the time of the write. Storage location: a single `ontology_hash` row in the DB (or `.graphiti/ontology-hash` sidecar file — implementation choice).
- **FR-003.** On service start AND on any subsequent ontology-affecting event (currently: only restart; future: hot-reload), liminis-graph MUST compare the current ontology hash against the persisted last-ingest hash and compute a `drifted: bool` flag.
- **FR-004.** When drift is detected, the service MUST compute a summary of what changed — added/removed entity types, added/removed relation types, mode change. This summary is exposed via `knowledge_status`'s `ontology.drift_summary` field as a human-readable string (and optionally a structured object for future UI use).
- **FR-005.** `knowledge_status` response MUST include an `ontology` object containing at minimum: `loaded: bool`, `mode: "open"|"strict"|null`, `entity_type_count: int`, `relation_type_count: int`, `drifted: bool`, `drift_summary: string|null`. Format follows the envelope convention from liminis-graph#96 (when it lands).
- **FR-006.** The persisted ontology hash MUST be updated atomically when re-ingestion brings the graph under a new ontology. Two viable triggers: (a) on `knowledge_clear_all{preserve_wal: false}` followed by any new write, (b) at the start of any fresh ingestion stream. Implementation decides; either works.
- **FR-007.** Drift MUST be reported as `false` when: there is no ontology now and there was no ontology at last ingest; the on-disk ontology semantically matches the persisted hash; the workspace has never been ingested into (no prior ontology to compare against).
- **FR-008.** A regression test MUST cover: detect drift after entity-type addition, detect drift after relation-type rename, ignore cosmetic edits, no false drift on identical reloads, drift clears after recreate-and-re-ingest.

## Success Criteria *(mandatory)*

- **SC-001.** With drift in place, `knowledge_status` returns `ontology.drifted: true` and a non-empty `drift_summary`. Verifiable by editing ontology.yaml, restarting, and inspecting the IPC response.
- **SC-002.** With no drift (file unchanged or only cosmetic edits since last ingest), `knowledge_status` returns `ontology.drifted: false`. No false positives.
- **SC-003.** The liminis-app Knowledge Graph panel shows a visible drift banner with a Recreate action when `drifted: true`. Verifiable by visual inspection.
- **SC-004.** After Recreate + re-ingest under the new ontology, the next `knowledge_status` reports `drifted: false`. The banner disappears.
- **SC-005.** A workspace that has never had an ontology change reports `drifted: false` on every status call.
- **SC-006.** Regression tests pass; existing tests pass unchanged.

## Edge Cases

- **First ingestion ever on a workspace.** No persisted hash exists. Treat current ontology (loaded or empty) as the baseline; persist on first write. No drift reported.
- **Workspace was ingested before ontology support existed.** Same as above — first start after upgrade has no persisted hash; record current ontology's hash. Drift is `false` (we can't retroactively claim drift against an undefined prior state).
- **Ontology file becomes malformed mid-life.** Per #83's graceful degradation (FR-003): service falls back to no-ontology mode. For drift purposes: treat "no ontology loaded due to error" as a special state — `loaded: false, error: "..."` — and do NOT report drift against the malformed file. Report drift only against valid loads.
- **Mode flip only** (open ↔ strict). Counts as drift (FR-001) — the mode change materially affects future extractions.
- **Description-only edit** (entity_type name unchanged, description updated). Counts as drift (FR-001) — the description influences LLM behavior per #83.
- **Adding a single description without changing any names.** Counts as drift, but the summary string makes it clear: "descriptions updated for: [Person, Organization]". User can dismiss if they consider the impact small.
- **User dismisses the banner**, makes more edits, then restarts. New session — banner reappears (per User Story 3 acceptance scenario 3). Dismissal is session-scoped, not permanent.
- **WAL replay path.** `knowledge_rebuild_from_wal` ingests against the currently-loaded ontology. If the WAL was written under a different ontology, drift would already be reported. After replay, the persisted hash is the currently-loaded ontology — no further drift indication.

## Assumptions

- **A1.** Content hashing the parsed Ontology is straightforward — sort entity-type names, sort relation-type-by-signature tuples, hash the canonical serialization. Standard SHA-256 is fine.
- **A2.** A small `OntologyHash` table (one row) is acceptable in lbug. If lbug schema migration is painful, a sidecar file at `<workspace>/.graphiti/ontology-hash` (or `.lcg/`) is equivalently fine.
- **A3.** The `ontology` object on `knowledge_status` is additive to the existing response — no client breaks.
- **A4.** The liminis-app UI banner is the right user-facing surface (vs a system notification, a modal, etc.). Banner is least-intrusive and matches the existing panel paradigm.
- **A5.** Recreate-then-re-ingest is the only correct way to bring an existing graph under a new ontology in v1. Partial re-classification is a future feature.
- **A6.** Comment-only / whitespace edits to ontology.yaml are common during dev — semantic hashing (not byte hashing) is important to avoid false-positive drift reports.

## Out of Scope

- Hot-reload of ontology (still deferred per #83). Drift detection still works — on restart.
- Cost estimation (User Story 5's P2 path) — separate follow-up issue if/when needed.
- Partial migration / re-classification (re-classify existing entities against new ontology without re-extracting). Genuinely useful but architecturally larger; separate issue.
- Versioning the ontology file (e.g. `version: 2` in the YAML). Not needed for v1 — the content hash captures change.
- Cross-workspace ontology sharing / inheritance. v2 work; out of scope.
- Detecting drift on individual entities/edges (e.g. "this entity's type is no longer in the ontology"). Too granular for v1; the binary "drift exists" signal is enough.

## Source References

- **liminis-graph#83 (merged):** introduced ontology support. This issue is the natural follow-on — making changes to the ontology actionable.
- **liminis-graph#96 (in flight):** standardizes IPC response envelope shape. The `ontology` object on `knowledge_status` should follow whatever convention #96 establishes.
- **liminis#824 (in flight):** fixes the Recreate UI handler to actually delete WAL. The drift-banner Recreate action invokes the same handler — coordinate so the confirmation dialog updates for both reasons (WAL deletion + ontology drift) are coherent.
- **Cutover plan:** `ideas/cutover-plan.md` Stage 3 — workspace migration. Drift detection helps users understand when a Python-era workspace needs re-ingestion under a Rust-era ontology declaration.
- **OSS launch:** `ideas/oss-launch-architecture.md` — when liminis-graph ships externally, users will edit ontology.yaml in their own workspaces. Drift detection is the difference between a usable feature and a foot-gun.
