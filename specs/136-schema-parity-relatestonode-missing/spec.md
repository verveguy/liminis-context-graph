# Feature Specification: Schema Parity — `RelatesToNode_` Missing `expired_at` Column → WAL Replay Drops Expired/Invalidated Relationships

**Feature Branch**: `fabrik/issue-136`
**Created**: 2026-06-16
**Status**: Draft
**Input**: Schema-parity regression (sibling of #133): `RelatesToNode_` is missing the `expired_at TIMESTAMP` column. #133 added `episodes STRING[]` but did not add `expired_at`, which graphiti's canonical Kuzu schema declares. FalkorDB-era WAL does `SET r.expired_at = $expired_at`; lbug has no such column → `Binder exception: Cannot find property expired_at for r`, which fails the whole reified-edge mutation and cascades into 74,544 `Cannot find property uuid` errors.

## Background

`lbug` is Kuzu (the community fork formerly `kuzudb`). graphiti's `kuzu_driver.py` (branch `liminis`) is the canonical schema reference. That file declares `RelatesToNode_` with the bitemporal triple `expired_at TIMESTAMP, valid_at TIMESTAMP, invalid_at TIMESTAMP`; liminis-graph's `schema.rs` has `valid_at` and `invalid_at` but **no `expired_at`**.

graphiti's edge model uses all three temporal columns for time-windowed relationship invalidation. The FalkorDB-era WAL (written before the FalkorDB→Kuzu migration) carries `SET r.expired_at = $expired_at` mutations that are still present at replay time. With the column absent, lbug raises a binder exception for every such line, failing the entire reified-edge MERGE — losing the relationship's `fact`, `name`, `uuid`, and `fact_embedding`. The binder exception then cascades: downstream mutations on the same variable (`r`) can't bind either, producing 74,544 `Cannot find property uuid` cascade failures.

Latest full-replay error taxonomy (all prior fixes — #128 apostrophe, #130 timestamp, #133 vecf32/episodes/bulk-SET — already in):

| Count | Error | Class |
|------:|-------|-------|
| 74,544 | `Binder exception: Cannot find property uuid` | cascade |
| 30,496 | `Binder exception: Cannot find property expired_at for r` | **root cause** |
| 8 | `Table HAS_MEMBER …` | legacy (out of scope) |
| 0 | apostrophe / timestamp / VECF32 / episodes | ✅ fixed |

The DB reached 553 MB (vs. original 713 MB); `expired_at` is the sole remaining root cause. Fixing it eliminates the 30,496 root failures and collapses the ~74k cascade.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Expired/invalidated relationships survive legacy-WAL recovery (Priority: P1)

An operator recovering a workspace from FalkorDB-era WAL gets relationships that carry an `expired_at` value (graphiti's invalidated/historical edges) reconstructed rather than silently dropped.

**Why this priority**: Every reified edge whose mutation includes `SET r.expired_at = ...` fails entirely today, losing its `fact`, `name`, `uuid`, and embedding. A replay that claims success is actually dropping the historical relationship layer wholesale.

**Independent Test**: Replay a WAL fixture line containing `SET r.expired_at = $expired_at` on a `RelatesToNode_` MERGE. Assert: `failed_lines == 0`, and the `RelatesToNode_` node exists post-replay with `expired_at` round-tripping correctly.

**Acceptance Scenarios**:

1. **Given** `RelatesToNode_` has an `expired_at TIMESTAMP` column, **When** a `SET r.expired_at = $expired_at` mutation replays, **Then** it succeeds and `expired_at` round-trips as a `TIMESTAMP`.
2. **Given** an existing DB (created before this fix) that lacks the `expired_at` column, **When** `create_edge_tables` (migration path) runs, **Then** `ALTER TABLE RelatesToNode_ ADD expired_at TIMESTAMP` is applied non-fatally, and subsequent `expired_at` mutations succeed.
3. **Given** a DB that already has the `expired_at` column (e.g., post-migration or fresh), **When** `create_edge_tables` runs again, **Then** the ALTER probe detects the column exists and skips the ALTER without error (idempotent).
4. **Given** a full legacy-WAL replay after this fix, **When** it completes, **Then** the `Binder exception: Cannot find property expired_at for r` error class is absent.

---

### User Story 2 — Cascade uuid failures collapse (Priority: P1)

The ~74k `Cannot find property uuid` cascade errors, which were driven by the failed `expired_at` MERGEs, disappear.

**Why this priority**: These cascade errors are not independent bugs — they are a symptom of the root `expired_at` binder failure. Fixing the root cause should eliminate them without any additional code change.

**Acceptance Scenarios**:

1. **Given** a legacy-WAL replay after this fix, **When** `expired_at` mutations succeed, **Then** the downstream `Cannot find property uuid` cascade drops to near-zero (or zero) compared to the 74,544 pre-fix baseline.

---

### Edge Cases

- **ALTER on a DB that already has the column** — the probe-first pattern (mirroring `relation_type` and `episodes` in `schema.rs`) avoids the lbug "ALTER ADD existing column corrupts hash index" issue.
- **NULL / absent `expired_at` in WAL params** — graphiti may supply a null `expired_at` for non-expired edges; the column must allow NULL (Kuzu TIMESTAMP columns are nullable by default — verify this assumption).
- **Column ordering** — `expired_at` belongs alongside `valid_at` and `invalid_at` (graphiti's canonical ordering: `expired_at, valid_at, invalid_at`).
- **Fresh DB (no migration needed)** — `CREATE NODE TABLE IF NOT EXISTS` in `schema.rs` gets the column from the start; the ALTER path is only for pre-existing DBs.
- **Interaction with existing bitemporal columns** — `valid_at` and `invalid_at` are already present and tested; `expired_at` follows the same type and nullability contract.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001.** Add `expired_at TIMESTAMP` to the `RelatesToNode_` `CREATE NODE TABLE` definition in `liminis-graph-core/src/schema.rs`, matching graphiti's `kuzu_driver.py`. Place it alongside `valid_at` and `invalid_at` (graphiti ordering: `expired_at, valid_at, invalid_at`).
- **FR-002.** Add a non-destructive migration for existing databases, mirroring the existing `relation_type` and `episodes` patterns in `schema.rs::create_edge_tables`: probe via a `MATCH (n:RelatesToNode_) RETURN n.expired_at LIMIT 0`-style query; if the column is absent, issue `ALTER TABLE RelatesToNode_ ADD expired_at TIMESTAMP`; tolerate the ALTER error non-fatally (the column may already exist on a partially-migrated DB). Fresh rebuilds get the column from FR-001's CREATE; existing live DBs get it via this ALTER.
- **FR-003.** Add a WAL fixture line setting `r.expired_at` to the regression test suite. Assert: `failed_lines == 0` for that fixture and the `expired_at` value round-trips correctly post-replay.
- **FR-004.** Pre-commit gates MUST pass: `cargo fmt --all && cargo test && cargo clippy --release --all-targets -- -D warnings`.

### Key Entities

- **`RelatesToNode_`**: Reified-edge node table in lbug (Kuzu). Represents a graphiti `RelatesToNode_` edge node with bitemporal validity columns (`expired_at`, `valid_at`, `invalid_at`). Lives in `liminis-graph-core/src/schema.rs`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001.** `Binder exception: Cannot find property expired_at for r` errors drop to **0** in a full legacy-WAL replay (was 30,496 pre-fix).
- **SC-002.** `Cannot find property uuid` cascade errors drop substantially vs. the 74,544 pre-fix baseline (ideally to 0, or to only those not caused by `expired_at` failures).
- **SC-003.** A freshly-created DB has `expired_at TIMESTAMP` in `RelatesToNode_` from the `CREATE NODE TABLE` statement — no migration required.
- **SC-004.** An existing DB (pre-fix) gains `expired_at TIMESTAMP` via the idempotent ALTER migration without data loss or error.
- **SC-005.** New regression test (FR-003) passes; existing tests unaffected; all pre-commit gates green.

## Assumptions

- **A1.** `lbug` is Kuzu; graphiti's `kuzu_driver.py` (branch `liminis`) is the authoritative schema reference. Kuzu TIMESTAMP columns are nullable by default (no explicit `DEFAULT NULL` needed).
- **A2.** The production recovery is a sole-user, additive-migration context (consistent with #126 and the `relation_type` / `episodes` precedents): no multi-user on-disk migration strategy is needed beyond the probe-and-ALTER pattern.
- **A3.** The `expired_at` column is set on relationship invalidation; it is `NULL` for currently-valid edges. The WAL may supply either a timestamp string or `null` for this field; `json_to_cypher_literal` already handles timestamp literals (#130) and null values.
- **A4.** Fixing `expired_at` eliminates both the 30,496 root failures and the ~74k uuid cascade — both are driven by the same binder exception on the same MERGE statement.

## Out of Scope

- **Community / HAS_MEMBER tables.** The 8 `Table HAS_MEMBER` legacy errors are a separate graphiti feature not implemented in liminis-graph; they remain a legitimate skip.
- **Full `RelatesToNode_` column-parity audit.** `expired_at` is the only column whose absence is proven by the error taxonomy. A broader audit (any other columns in graphiti's schema not in lbug's) is a follow-up.
- **Changes to the write path.** The current write path already emits lbug-native Cypher; this fix is read-path (replay) + schema only.
- **On-disk migration beyond additive ALTER.** Consistent with the sole-user recovery stance in prior issues.

## Source References

- `graphiti_core/driver/kuzu_driver.py` (graphiti fork, branch `liminis`) — canonical Kuzu schema; `RelatesToNode_` definition includes `expired_at TIMESTAMP`.
- `liminis-graph-core/src/schema.rs` — `RelatesToNode_` `CREATE NODE TABLE` and migration logic (`create_edge_tables`).
- `liminis-graph-core/src/replay.rs` — WAL replay path; `LEGACY_SCHEMA_ERROR_PATTERNS`.
- #133 — sibling fix that added `episodes STRING[]`; same pattern for schema + migration + regression test.
- #128 / #130 — prior replay-fidelity fixes (apostrophe escaping, timestamp literals).
