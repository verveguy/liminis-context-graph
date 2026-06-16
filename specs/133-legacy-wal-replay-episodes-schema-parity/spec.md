# Feature Specification: Legacy-WAL Replay Compatibility — `episodes` Schema Parity + FalkorDB-Dialect Translation (VECF32, bulk-SET)

**Feature Branch**: `fabrik/issue-133`
**Created**: 2026-06-15
**Status**: Draft
**Input**: Production WAL-recovery on another machine. With #128 (apostrophe escaping) and #130 (timestamp literals) confirmed fixed (0 occurrences in the latest replay), replay of the FalkorDB-era WAL still fails en masse. Investigation traced this to two distinct root causes, both already solved in graphiti's `kuzu_driver.py` and `wal_replay_helpers.py` — the fix is to mirror graphiti's Kuzu driver, not invent new translations.

## Background

`lbug` is Kuzu (the community fork formerly `kuzudb`). Our graphiti fork ships a Kuzu driver (`graphiti_core/driver/kuzu_driver.py`) plus replay helpers (`graphiti_core/driver/wal_replay_helpers.py`). Those are the canonical reference for the correct Kuzu-dialect schema and FalkorDB→Kuzu Cypher translations.

**Root cause A — `RelatesToNode_` schema is missing the `episodes` column (SILENT DATA LOSS)**

graphiti's `kuzu_driver.py` declares `episodes STRING[]` on the reified-edge node table; liminis-graph's `schema.rs` omits it. When a WAL mutation does `SET r.episodes = $episodes`, lbug raises `Binder exception: Cannot find property episodes for r`, failing the entire reified-edge MERGE — losing the relationship's `fact`, `name`, `fact_embedding`, and provenance `episodes`. These failures are currently silently bucketed as `legacy_skipped_lines` (via a pattern added with #128), so the rebuild reports "clean" while discarding the entire relationship layer. In the latest partial run: 48,518 mis-classified skips plus a downstream cascade of ~55,026 `Cannot find property uuid for r` errors.

**Root cause B — FalkorDB-dialect Cypher constructs lbug cannot execute**

The FalkorDB-era WAL (`.lcg/wal/*.jsonl` dated ~2026-03-25 → 2026-03-31, written before the FalkorDB→Kuzu migration per ADR-035) contains FalkorDB-dialect Cypher. Two constructs:

- **`vecf32(...)` wrapper** — FalkorDB's float32-vector constructor. lbug has no such function → `Catalog exception: function VECF32 does not exist`. 27,819 occurrences in the partial run. lbug's `FLOAT[N]` columns accept a bare list literal / list-typed param, so stripping the wrapper is lossless. The Rust port must be **case-insensitive** (observed: uppercase `VECF32`) and handle **both** `vecf32($ident)` / `vecf32($a.b)` (param-ref) and `vecf32([...])` (inline array literal) — graphiti's reference regex covers only lowercase param-ref forms.
- **`SET n = $props` bulk property set** (preemptive parity, no observed failure yet) — Neo4j/FalkorDB allow setting all properties from one map param; lbug requires individual `SET n.k = $k` assignments. graphiti already solves this in `expand_bulk_property_set`; porting now consolidates the translation layer and preempts the next whack-a-mole.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Relationship layer survives legacy-WAL recovery (Priority: P1)

An operator recovering a workspace from FalkorDB-era WAL gets the relationship layer (reified `RelatesToNode_` edges: `fact`, `name`, `episodes` provenance, `fact_embedding`) reconstructed, not silently dropped.

**Why this priority**: The relationship layer is currently discarded while the rebuild reports success — a silent-data-loss bug. A "successful" recovery yields a graph with entities but no relationships.

**Independent Test**: Replay a WAL containing `MERGE (r:RelatesToNode_ …) SET r.fact = …, r.episodes = $episodes, …`. Assert: the mutation counts as `mutations_replayed` (NOT `legacy_skipped_lines`), and post-rebuild the `RelatesToNode_` node exists with its `episodes` array populated.

**Acceptance Scenarios**:

1. **Given** `RelatesToNode_` has an `episodes STRING[]` column, **When** a `SET r.episodes = $episodes` mutation replays, **Then** it succeeds and `episodes` round-trips as a `STRING[]`.
2. **Given** the legacy-skip list no longer contains the `episodes` pattern, **When** an `episodes`-bearing mutation would have failed pre-fix, **Then** post-fix it is counted in `mutations_replayed`, and `legacy_skipped_lines` does NOT increment for it.
3. **Given** a full legacy-WAL replay, **When** it completes, **Then** the reconstructed graph has a non-zero relationship layer (`RelatesToNode_` count > 0) — not entities-only.

---

### User Story 2 — FalkorDB-era embeddings replay losslessly (Priority: P1)

Embeddings written by the FalkorDB-era WAL (`vecf32(...)` wrapper, any case, param-ref or inline-array) replay into lbug's native `FLOAT[N]` columns.

**Independent Test**: Replay WAL lines containing `VECF32([0.1, -0.2, …])`, `vecf32($embedding)`, and `VECF32($x.y)`. Assert `failed_lines == 0` for them and the `FLOAT[N]` column round-trips.

**Acceptance Scenarios**:

1. **Given** a mutation `SET n.name_embedding = VECF32([…])` (inline, uppercase), **When** replayed, **Then** the wrapper is stripped, the create succeeds, no `Catalog exception` is raised.
2. **Given** a mutation `SET n.content_embedding = vecf32($emb)` (param-ref, lowercase), **When** replayed, **Then** the wrapper is stripped to `$emb`, which interpolates to a list literal and binds to the `FLOAT[N]` column.
3. **Given** a mutation with a dotted param `vecf32($p.embedding)`, **When** replayed, **Then** it strips to `$p.embedding`.
4. **Given** a mutation containing no `vecf32`, **When** normalized, **Then** the Cypher is returned unchanged (idempotent / no-op safe).

---

### User Story 3 — Bulk property-set is expanded (Priority: P2, preemptive parity)

WAL lines using Neo4j/FalkorDB bulk `SET n = $props` are expanded to individual assignments lbug accepts.

**Why this priority**: Not yet observed in our WAL (only 5.7% scanned) but a known graphiti-Kuzu dialect gap already solved upstream; porting now consolidates the translation layer and preempts the next failure.

**Acceptance Scenarios**:

1. **Given** `SET n = $props` where `$props` is a map, **When** normalized, **Then** it becomes `SET n.k1 = $props_k1, n.k2 = $props_k2, …` with the nested dict flattened into top-level params.
2. **Given** `SET n.field = $value` (individual assignment), **When** normalized, **Then** it is left unchanged (must not false-match).
3. **Given** mixed bulk + individual SET clauses, and/or multiple bulk SETs in one query, **When** normalized, **Then** all bulk SETs expand correctly and offsets don't corrupt.

---

### User Story 4 — Recovery fidelity is observable, not silently masked (Priority: P1)

After this lands, an operator can trust that a "clean" rebuild (`failed_lines == 0`) actually means the data is present — the silent-skip masking is closed.

**Acceptance Scenarios**:

1. **Given** a rebuild over legacy WAL, **When** it completes, **Then** the `wal_replay_complete` summary's `legacy_skipped_lines` reflects only genuinely-unsupported constructs (e.g. Community/HAS tables), not `episodes` mutations.
2. **Given** any `episodes`-property error recurs in the future (e.g. another missing column), **When** it occurs, **Then** it surfaces as a loud `failed_lines` (with a `failed_samples` entry), not a silent skip.

### Edge Cases

- **`vecf32` case variants** — `VECF32(`, `vecf32(`, `VecF32(` must all strip.
- **`vecf32` inline array vs param ref** — both `vecf32([…])` and `vecf32($x)` must strip. Verify which actually occurs against a real WAL sample; implement both regardless.
- **Nested / multiple `vecf32` in one statement** (e.g. entity + edge embedding in one MERGE) — strip all occurrences.
- **`episodes` value shape** — the WAL supplies `$episodes` as a JSON array of strings; `json_to_cypher_literal` already renders arrays as `[ … ]`, which binds to `STRING[]`.
- **Idempotency** — all normalizations must be no-ops when their construct is absent (the vast majority of lines).
- **ALTER on a DB that already has the column** — the probe-first pattern (mirroring `relation_type`) avoids the lbug "ALTER ADD existing column corrupts hash index" bug noted in `schema.rs`.
- **Ordering** — `strip_vecf32` must precede `interpolate_params` so `vecf32($e)` → `$e` → interpolates; `expand_bulk_property_set` must precede interpolation because it mutates params.
- **bulk-SET param-name collisions** — graphiti prefixes flattened keys with the param name (`props_uuid`) to avoid clobbering an existing top-level `$uuid`; the port must preserve that.

## Requirements *(mandatory)*

- **FR-001.** Add `episodes STRING[]` to the `RelatesToNode_` `CREATE NODE TABLE` definition in `liminis-graph-core/src/schema.rs`, matching graphiti's `kuzu_driver.py`.
- **FR-002.** Add a non-destructive migration for existing databases, mirroring the existing `relation_type` pattern in `schema.rs::migrate` (probe via `MATCH (n:RelatesToNode_) … RETURN n.episodes LIMIT 0`; if absent, `ALTER TABLE RelatesToNode_ ADD episodes STRING[]`; tolerate the error non-fatally). Fresh rebuilds get the column from FR-001's CREATE; existing live DBs get it via this ALTER.
- **FR-003.** Remove `"cannot find property episodes for"` from `LEGACY_SCHEMA_ERROR_PATTERNS` in `liminis-graph-core/src/replay.rs`. With FR-001/FR-002 in place these mutations succeed; if any `episodes` error ever recurs it MUST surface as a real `failed_line`, not a silent skip. (Leave the `community` / `has` patterns — those are a separate graphiti feature not implemented here.)
- **FR-004.** Implement a `vecf32(...)` wrapper-stripping normalization, ported from graphiti's `strip_vecf32_wrappers` but **case-insensitive** and handling **both** `vecf32($ident)` / `vecf32($a.b)` and `vecf32([ …inline… ])`. Use a balanced-parenthesis strip (find `vecf32(` case-insensitively, scan to the matching `)`, replace `vecf32(<inner>)` with `<inner>`). MUST be idempotent / no-op when no `vecf32` is present.
- **FR-005.** Implement a `SET <var> = $<param>` bulk-property-set expansion, ported from graphiti's `expand_bulk_property_set`: when the named param is a JSON object, rewrite to individual `var.key = $param_key` assignments and flatten the nested object into top-level params; must NOT match individual `SET var.field = $x` assignments; must handle multiple/mixed SET clauses without offset corruption.
- **FR-006.** Wire both translations into the replay path in `replay.rs` in this order, **before** `interpolate_params`: raw `wal_line.cypher` → `strip_vecf32` (cypher only) → `expand_bulk_property_set` (cypher + params) → `interpolate_params(cypher, params)` → `raw_query`.
- **FR-007.** House the two text/param translations in one focused, dependency-light module (e.g. `liminis-graph-core/src/legacy_wal.rs`) with unit tests. (#128 apostrophe-escaping and #130 timestamp-literal handling stay in `json_to_cypher_literal` because they are param-value transforms; the new module is for Cypher-text / param-shape transforms. Document this split in the module doc comment.)
- **FR-008.** Add regression fixtures/tests covering: (a) an `episodes`-bearing reified-edge MERGE replaying as `mutations_replayed` with `episodes` round-tripping; (b) `vecf32` strip across uppercase-inline, lowercase-param-ref, and dotted-param forms; (c) bulk-`SET n = $props` expansion including the individual-assignment non-match and the multiple-clause cases; (d) a regression asserting the `episodes` error class is no longer counted as `legacy_skipped_lines`.
- **FR-009.** Pre-commit gates MUST pass: `cargo fmt --all --check && cargo test --release && cargo clippy --all-targets -- -D warnings`.

## Success Criteria *(mandatory)*

- **SC-001.** A legacy-WAL replay reconstructs a non-zero relationship layer (`RelatesToNode_` count > 0), with `episodes` provenance and `fact_embedding` populated — not entities-only.
- **SC-002.** `VECF32`/`vecf32` failures drop to 0 in a full replay (was 27,819 in the 5.7% partial), and the `Catalog exception: function VECF32 does not exist` class disappears.
- **SC-003.** `episodes` mutations are counted in `mutations_replayed`, and `legacy_skipped_lines` no longer includes them (was 48,518 mis-classified in the partial run).
- **SC-004.** The downstream `Cannot find property uuid for r/e` cascade collapses substantially (it was driven by the failed episodes + vecf32 creates).
- **SC-005.** A "clean" rebuild (`failed_lines == 0`) genuinely means data present — verified by SC-001, not by skip-masking.
- **SC-006.** New unit/regression tests (FR-008) pass; existing tests unaffected; all pre-commit gates green.
- **SC-007.** Re-running the production legacy-WAL recovery shows materially higher fidelity vs. the pre-fix partial run (which was ~95% failing/skipped at the 5.7% mark).

## Assumptions

- **A1.** `lbug` is Kuzu; graphiti's `kuzu_driver.py` + `wal_replay_helpers.py` are the authoritative reference for correct Kuzu-dialect schema and translations. (Verified: episodes column present upstream / absent locally; vecf32 + bulk-set helpers exist upstream.)
- **A2.** lbug `FLOAT[N]` columns accept a bare list literal / list-typed param, so stripping `vecf32(...)` is lossless (graphiti relies on exactly this).
- **A3.** Sole-user recovery context (production on another machine): additive schema change + WAL rebuild is sufficient; no multi-user on-disk migration needed.
- **A4.** Only ~5.7% of the 43,821 WAL files were scanned, so the error taxonomy is a lower bound; the bulk-SET construct (FR-005) is included preemptively for that reason.

## Out of Scope

- **Community / HAS_MEMBER tables.** graphiti has `Community` nodes and `HAS_MEMBER` rels (community-detection feature); liminis-graph does not implement them, so `"table community does not exist"` / `"table has does not exist"` remain legitimate legacy-skips.
- `expired_at TIMESTAMP` and any other column-parity gaps beyond `episodes`. A full `RelatesToNode_` column-parity audit against `kuzu_driver.py` is worth a follow-up; this issue fixes only the proven failing column (`episodes`).
- Changing the current write path (it already emits lbug-native Cypher; `vecf32` appears nowhere in `liminis-graph-core/src`).
- On-disk migration of an existing built DB beyond the additive ALTER (consistent with the #126 sole-user stance).

## Source References

- `graphiti_core/driver/kuzu_driver.py` (graphiti fork, branch `liminis`) — canonical Kuzu schema reference; `RelatesToNode_` definition with `episodes STRING[]`.
- `graphiti_core/driver/wal_replay_helpers.py` (graphiti fork, branch `liminis`) — reference implementations for `strip_vecf32_wrappers` and `expand_bulk_property_set`.
- #128 — apostrophe escaping (param-value transform; stays in `json_to_cypher_literal`).
- #130 / PR #131 — timestamp typed literals (param-value transform; stays in `json_to_cypher_literal`).
