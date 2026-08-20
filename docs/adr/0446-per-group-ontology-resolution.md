# ADR-0446: Per-Group Ontology Resolution

**Status**: Accepted
**Date**: 2026-08-20
**Issue**: #446
**Relates to**: ADR-0014 (ontology extractor trait parameter), ADR-0018 (ontology hash sidecar),
ADR-0049 (bare-path ontology loader), ADR-0378 (multi-stream WAL per group directory), ADR-0385
(per-group mutation attribution)

## Context

Workspace-scoped ontology (#83, ADR-0014) gave `liminis-context-graph` a single
`Option<Arc<Ontology>>` loaded once from `{workspace}/.lcg/ontology.yaml` at startup and applied
uniformly to every group. But one lcg instance routinely holds many co-resident `group_id`s (#378
made per-group WAL streams a first-class concept), and those groups often have unrelated subject
domains: a content channel's `Person`/`Organization` vocabulary and a catalog group's
`KnowledgeChannel`/`Topic`/`Team` vocabulary cannot both be served by one workspace-wide ontology
without one group's `mode: strict` (or type vocabulary) wrongly constraining the other.

Four design questions had to be settled before implementation, each with a real cost either way.

## Decision 1: Additive `AppState` field, lazy-load-and-cache, not eager scan

**Chosen**: a new `AppState.group_ontologies: Arc<Mutex<HashMap<String, Option<Arc<Ontology>>>>>`
field, populated lazily by `AppState::resolve_ontology(group_id)` on that group's first touch,
caching the *already-resolved* result (per-group file if valid, else the workspace-wide
`ontology` field, else `None`). The existing `ontology` field is untouched in type and meaning.

**Rejected**: eagerly scanning `{workspace}/.lcg/ontology/*.yaml` at startup, and/or replacing
`AppState.ontology`'s type outright to fold per-group resolution into a single new structure.

**Rationale**: `AppState { ... }` is constructed literally at 53 sites across the test suite, one
bench, and `wal_exec.rs`'s own test helper — this repo's CLAUDE.md flags exactly this class of
change (struct field shape changes) as historically costly (#46, #58). An additive field needs one
new line per site, not a signature rethink. Lazy population mirrors the pattern
`AppState.wal_writers` already established for per-group state (`with_wal_writer`): a group this
process never touches never pays a file read, and the existing "restart required to pick up
ontology changes" contract extends unchanged to per-group files — there was never a need for a
startup-time scan for a feature that doesn't hot-reload.

## Decision 2: A malformed per-group file falls back to the workspace ontology, logged

**Chosen**: `load_group_ontology` mirrors `load_ontology`'s existing silent-fallback shape — a
missing, empty, or malformed per-group file returns `None` from that function (with a logged
`eprintln!` for the malformed case), and `resolve_ontology` then falls through to
`self.ontology.clone()`, exactly as if no per-group file existed.

**Rejected**:
- Silently resolving to `None` (no ontology at all) for a group with a broken per-group file —
  more surprising than falling back to an ontology already known to be valid, and strictly worse
  for the group's extraction than doing nothing.
- Hard-failing (mirroring `load_ontology_from_path`'s `Result`-based, loud-error shape from
  ADR-0049) — would let one typo'd per-group YAML file take down extraction for that group
  entirely, a blast radius disproportionate to a config-file mistake in a system designed to
  degrade gracefully around ontology (an ontology-less workspace has always been a supported,
  first-class state).

**Rationale**: falling back to the one ontology this process already validated at startup is the
least-surprising failure mode, and the `eprintln!` keeps the mistake observable rather than
silently masked — the same tradeoff `load_ontology` itself already makes for the workspace file.

## Decision 3: Drift detection stays workspace-scoped — not extended per-group

**Chosen**: `ontology_sidecar.rs` (`.lcg/ontology-hash.json`, ADR-0018) and
`AppState.ontology_drift` are untouched by this issue. `handle_knowledge_status` and both
WAL-replay drift-sidecar refresh sites (`handlers.rs`) continue reading `state.ontology`
unconditionally, never `resolve_ontology` — they record "the graph is now consistent with the
*workspace* ontology," a concept this issue does not extend to a per-group dimension.

**Rejected**: a `.lcg/ontology/<group_id>.hash.json` sidecar per group, with a corresponding
per-group `OntologyDriftState`.

**Rationale**: the spec's Functional Requirements never mention drift for per-group ontologies,
and extending it would mean a new sidecar-per-group plus a new per-group state model — real,
non-trivial scope nothing in the spec asked for. Building it unasked risks scope creep and an
untested surface; leaving it silently ambiguous risks a future reader assuming per-group drift
exists when it doesn't. This ADR states the gap explicitly instead: **a group governed only by a
per-group ontology file gets no drift warning today if that file changes across a restart.** This
is a deliberate v1 limitation, not an oversight, and a natural (but unscoped) follow-up.

## Decision 4: The published ontology sidecar is write-only — never read back by lcg

**Chosen**: `add_episode` writes `.wal-ontology.json` into the extracting group's own WAL
directory (`ontology_sidecar::write_wal_ontology_sidecar`, reusing the existing `OntologySidecar`
struct/serialization from ADR-0018 rather than inventing a second schema) immediately after that
group's extraction commits, using the exact ontology that guided it. Nothing in this codebase ever
reads this file back, on either the producer or consumer side.

**Rationale**: `docs/operations.md`'s publish contract already establishes that "publishing" is an
external, manual whole-directory copy (`cp -R`/`rsync -a`/`git add -A`) — there is no lcg
"publish" or "hydrate" command. Writing one new dot-namespace file that travels under the existing
contract satisfies FR-007 (the published ontology travels with the stream) with zero new
publish/hydrate code. Because there is no hydrate/consume code path in lcg today, FR-008 ("a
received ontology MUST NOT govern the consumer's own behavior") is satisfied **structurally**
rather than by a runtime guard: there is nothing to gate, because nothing reads the file. This
file is a new, fourth bucket in the publish contract — distinct from the existing load-bearing
(`.wal-generation.json`) and cache (`.wal-bounds.json`) buckets — because unlike a cache, its
absence never triggers a performance cost or a rescan; it only means a consumer inspecting the
stream loses a piece of documentation. See the updated publish-contract table in
`docs/operations.md`.

**A future consequence, not this issue's to solve**: if a later issue adds an actual hydrate/ingest
command to lcg, that issue must audit against `.wal-ontology.json` the same way it must audit
against every other dot-namespace file — specifically, it must not accidentally start applying a
received ontology operatively. This ADR's Decision 4 is the reason that audit item exists.

## Consequences

- `AppState` gains one field (`group_ontologies`) and one method (`resolve_ontology`); every
  existing consumer of `state.ontology` that needs group-scoped behavior (`add_episode`,
  `handle_reprocess_entity_types`, `handle_canonicalize_relations`,
  `handle_reprocess_relation_types`) switches to `state.resolve_ontology(&group_id)`.
  `handle_knowledge_status` and both WAL-replay drift-sidecar sites deliberately do not.
- `handle_backfill_relation_types`/`backfill.rs` needed no code change: it derives pseudo relation
  types from edge fact text, never consults an `Ontology`'s declared vocabulary, so FR-006's
  concern for it is already fully addressed by #447's `group_id` requirement.
- A new on-disk surface: `{workspace}/.lcg/ontology/<group_id>.yaml` (percent-encoded via
  `wal_group::encode_group_dir_name`, the same bijective scheme WAL directories already use) and
  `<wal_root>/<group_id>/.wal-ontology.json`. Neither requires a schema migration; both are plain
  files a human or external tool can inspect directly.
- No IPC/MCP protocol surface change — `knowledge_status`'s shape is unchanged, avoiding
  coordination with the separate `service_protocol.py` consumer (the liminis app).
- Per-group ontology resolution requires no additional operator action for a single-group
  deployment: with no `.lcg/ontology/` directory present, every group falls through to the
  existing workspace-wide `ontology` field exactly as before this issue (SC-002).
- **A known v1 gap, deliberately not closed by this issue**: `group_ontology_path`/
  `load_group_ontology` do not apply `wal_group::check_no_case_insensitive_collision`, unlike
  `AppState::with_wal_writer`'s WAL directory creation. Two already-safe `group_id`s differing
  only by ASCII case (e.g. `Catalog`/`catalog`) resolve to the same filename on a
  case-insensitive filesystem (macOS APFS, Windows NTFS), so one group's ontology could silently
  load for the other on those platforms — the one cross-group-leakage vector this issue does not
  structurally close. Closing it requires either reusing the WAL guard against a per-group
  ontology directory listing of files rather than subdirectories, or a dedicated variant; left as
  a follow-up rather than folded into this issue's diff. See `docs/ontology.md`'s "Known v1
  limitation" note.
