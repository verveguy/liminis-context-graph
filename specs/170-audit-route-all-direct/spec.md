# Feature Specification: Audit — Route All Direct-Write Paths Through Shared Type Coercion

**Feature Branch**: `fabrik/issue-170`
**Created**: 2026-06-22
**Status**: Draft
**Input**: Issue #170 — "AUDIT: route all direct-write paths through shared type coercion (timestamps/FLOAT[]) to match WAL replay"

## Background

The `knowledge_merge_entities` TIMESTAMP bug (fixed in Issue #169) was one symptom of a class problem: **direct-write code paths build Cypher literals and parameters ad-hoc**, without applying the type coercion that the WAL-replay path enforces. The WAL-replay path (`replay.rs` → `json_to_cypher_literal`) correctly handles:

- RFC-3339 `TIMESTAMP` literals (#130 fix — bare strings and millisecond integers produce `TYPE_MISMATCH` errors in lbug/Kuzu)
- Native `FLOAT[]` list literals for embeddings (#133 fix — FalkorDB's `vecf32(...)` syntax is rejected by lbug)
- Apostrophe/special-character escaping for string values (#128 fix)

Every direct-write feature that bypasses replay — node/edge insertions, correction application, canonicalization writes, WAL-dump serialization — must apply the same coercion or it silently writes the wrong types into the canonical schema. This is not a theoretical risk; `merge_entities` demonstrated exactly this failure mode in production.

This issue:
1. Generalizes `json_to_cypher_literal` into a **single shared coercion helper** accessible by every write path (direct and replay)
2. Audits each known direct-write path and routes it through that helper
3. Adds tests that explicitly verify correct type output for each path (TIMESTAMP fidelity, `FLOAT[]` format, string escaping)

The deliverable is a codebase where no write path formats timestamps or embeddings by hand, and a documented inventory confirming every write path's coercion status.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Developer adding a new write path cannot accidentally bypass coercion (Priority: P1)

A developer implementing a new IPC write method (e.g., a future entity-update path) reaches for the shared coercion helper rather than hand-formatting a Cypher SET clause. The helper is discoverable, documented via its signature and inline invariant comment, and returns the correct Cypher literal for every schema type in use (TIMESTAMP, FLOAT[], string, integer, boolean, null).

**Why this priority**: The root cause of #169 was that there was no obvious single point to reuse — each path invented its own formatting. Making the shared helper the obvious and easy path prevents recurrence.

**Independent Test**: Add a unit test directly on the shared coercion helper that verifies: (a) an RFC-3339 datetime string is emitted as a Kuzu `TIMESTAMP('...')` literal; (b) a `Vec<f32>` is emitted as `[0.1, 0.2, ...]` with no `vecf32(` prefix; (c) a string containing an apostrophe is escaped correctly; (d) `null` / `None` is emitted as `NULL`.

**Acceptance Scenarios**:

1. **Given** a datetime string `"2024-01-15T10:30:00Z"`, **When** passed through the shared coercion helper with type hint `TIMESTAMP`, **Then** the output is `TIMESTAMP('2024-01-15T10:30:00Z')` (or equivalent lbug-accepted form).
2. **Given** a `Vec<f32>` embedding of length 768, **When** coerced, **Then** the output begins with `[` and contains no `vecf32(` substring.
3. **Given** a string value containing `O'Brien`, **When** coerced as a string, **Then** the apostrophe is escaped and the output does not break a surrounding Cypher string literal.
4. **Given** a `null` / `serde_json::Value::Null`, **When** coerced, **Then** the output is the bare token `NULL`.

---

### User Story 2 — `knowledge_dump_wal` preserves TIMESTAMP fidelity through round-trip (Priority: P1)

After Issue #161 implemented `knowledge_dump_wal`, the WAL it produces must survive a dump→wipe→replay cycle without corrupting TIMESTAMP columns. A regression in coercion here would silently down-type timestamps to strings, causing lbug `TYPE_MISMATCH` errors on WAL replay.

**Why this priority**: `dump_wal` is the WAL-compaction and disaster-recovery path. A TIMESTAMP coercion bug in the dump would corrupt the canonical recovery WAL — invisible until a replay is attempted under pressure.

**Independent Test**: Seed a test graph with an entity whose `created_at` field is a known RFC-3339 timestamp. Call `knowledge_dump_wal`. Read the emitted WAL line and assert the timestamp appears as a `TIMESTAMP(...)` literal (not a bare string or integer). Then wipe the DB and replay the dump; query the entity and assert `created_at` matches the original value exactly (not truncated or type-changed).

**Acceptance Scenarios**:

1. **Given** an entity with `created_at = "2024-06-01T12:00:00.000000Z"`, **When** `knowledge_dump_wal` is called and the output WAL is inspected, **Then** the WAL line contains `TIMESTAMP('2024-06-01T12:00:00.000000Z')` (or equivalent) — not the bare string `"2024-06-01T12:00:00.000000Z"`.
2. **Given** an entity with a non-null `name_embedding` (FLOAT[]), **When** `knowledge_dump_wal` serializes it, **Then** the WAL line contains `[<float>, <float>, ...]` and no `vecf32(` substring.
3. **Given** the dump WAL is replayed via `knowledge_rebuild_from_wal`, **When** `knowledge_find_entities` is called post-replay for a known entity, **Then** the entity's `created_at` value is byte-identical to the pre-dump value and no `[WAL WARN]` or `[WAL SKIP]` lines were emitted.

---

### User Story 3 — Corrections paths (`corrections.rs`) write correct types (Priority: P1)

The `same_as`, `retract`, `apply_entity_type_labels`, and `reprocess_entity_types` code paths in `corrections.rs` build Cypher mutations directly against the DB. Any timestamp or embedding written by these paths must use the same coercion as replay.

**Why this priority**: Corrections are applied to production graphs and their mutations are WAL-durable. A type mismatch here corrupts production data and survives into dump WALs.

**Independent Test**: In an integration test, apply a `same_as` correction that triggers an edge SET mutation including a `valid_at` or `created_at` timestamp. After applying, query the edge's property directly and verify the value is a TIMESTAMP type (not a string or integer). Verify no `TYPE_MISMATCH` lbug error was logged.

**Acceptance Scenarios**:

1. **Given** a `same_as` correction that rewrites edges and updates the canonical entity's `created_at`, **When** applied, **Then** querying `created_at` on the canonical entity returns a value whose Kuzu type is `TIMESTAMP`, not `STRING`.
2. **Given** `apply_entity_type_labels` writes a property update, **When** the mutation is executed, **Then** no `TYPE_MISMATCH` error is produced and the property is stored with the correct type.
3. **Given** `retract` marks an edge as invalidated (setting `invalidated_at`), **When** the timestamp property is read back, **Then** its type is `TIMESTAMP` and its value matches the invalidation time to millisecond precision.

---

### User Story 4 — `insert_relates_to_edge` / `insert_mentions_edge` write correct types (Priority: P1)

These are the primary write paths for relationship and mention edges produced during `knowledge_process_chunk`. If their Cypher literals are hand-formatted, embedding or timestamp type bugs are invisible until a WAL replay (e.g., after disaster recovery) fails.

**Why this priority**: These are the highest-frequency write paths in the codebase — every ingestion call exercises them. A silent type error here affects every relationship the system creates.

**Independent Test**: Call `knowledge_process_chunk` (or directly exercise `insert_relates_to_edge` and `insert_mentions_edge` in a unit test) and verify: the `created_at` column on the resulting edges has Kuzu type `TIMESTAMP`; any edge with a `name_embedding` has Kuzu type `FLOAT[]` (not `STRING`).

**Acceptance Scenarios**:

1. **Given** a call to `insert_relates_to_edge` with a non-null `name_embedding`, **When** the edge is inserted and queried back, **Then** the embedding column has lbug type `FLOAT[]` and its value round-trips without precision loss.
2. **Given** a call to `insert_mentions_edge` with a `created_at` timestamp, **When** the edge is inserted and queried back, **Then** `created_at` has lbug type `TIMESTAMP` and equals the inserted value.

---

### User Story 5 — Relation canonicalization (#163) writes correct types (Priority: P2)

The relation canonicalization path (Issue #163) emits `relation_type` / edge writes that must coerce timestamps and embeddings exactly as replay does.

**Why this priority**: P2 because canonicalization is a batch post-processing step, not on the hot ingest path, but it still writes durable mutations. A type error would propagate silently until a WAL replay.

**Independent Test**: Trigger a canonicalization on a test relation and assert the resulting edge's timestamp columns are `TIMESTAMP` type (not `STRING`) and any embedding columns are `FLOAT[]`.

**Acceptance Scenarios**:

1. **Given** a canonicalization edge write for a relation type with a `created_at` field, **When** the write is executed, **Then** `created_at` has Kuzu type `TIMESTAMP`.
2. **Given** a canonicalization edge write for a relation type with an embedding field, **When** the write is executed, **Then** the embedding column has Kuzu type `FLOAT[]`.

---

### Edge Cases

- **Null embedding field**: A node or edge with a `null` embedding (not yet indexed) must serialize as `NULL` in all write paths, not as an empty list or empty string.
- **Timestamp precision**: lbug/Kuzu TIMESTAMP accepts microsecond precision. Coercion must preserve at least millisecond precision; truncation to seconds is a correctness defect.
- **Legacy TIMESTAMP format from DB**: When `dump_wal` reads a timestamp value back from lbug (which may return it as a chrono datetime, a string, or an integer epoch), the coercion helper must normalize it to RFC-3339 before emitting the WAL literal.
- **`knowledge_query_cypher` param interpolation**: If this method accepts user-supplied Cypher params that are interpolated into a query string, those params must also go through the shared coercion or the method must document that it passes params as typed query parameters (not string interpolation). If no param interpolation is done, this path is explicitly confirmed safe.
- **Repeated coercion**: Applying the shared helper to a value that is already a correctly-formatted Cypher literal (e.g., `TIMESTAMP('...')`) must not double-encode it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The function `json_to_cypher_literal` currently in `replay.rs` MUST be generalized and relocated to a shared module (e.g., `crates/core/src/coerce.rs` or an existing `util.rs`) so that all write paths can import it without depending on the replay module.
- **FR-002**: The shared coercion helper MUST handle, at minimum: RFC-3339 datetime strings → `TIMESTAMP('...')` literals; `Vec<f32>` / `serde_json::Value::Array` of floats → `[f32, f32, ...]` list literals; string values → `'...'` with apostrophe escaping; integers and booleans → bare literals; `null` / `serde_json::Value::Null` → `NULL`.
- **FR-003**: The existing call site in `replay.rs` (`json_to_cypher_literal`) MUST be updated to call the shared helper — do not leave a duplicate implementation in `replay.rs`.
- **FR-004**: The `knowledge_dump_wal` implementation (#161) MUST route all node and edge property serialization through the shared coercion helper. No property value in a dump WAL line may be formatted with a hand-rolled `format!` call or `.to_string()` on a datetime or float vector.
- **FR-005**: The `insert_relates_to_edge` and `insert_mentions_edge` functions in `db.rs` MUST route all Cypher parameter values through the shared coercion helper. If they currently use typed query parameters (not literal interpolation), this must be explicitly confirmed and documented with a comment.
- **FR-006**: The `corrections.rs` write paths — `apply_same_as_correction`, `apply_retract_correction`, `apply_entity_type_labels`, and any helper that builds a Cypher `SET` clause — MUST use the shared coercion helper for any property value being written. Ad-hoc timestamp formatting in `corrections.rs` (e.g., `Utc::now().to_rfc3339()` interpolated directly into a Cypher string) MUST be removed.
- **FR-007**: The relation canonicalization write path (#163) MUST use the shared coercion helper for any property values it sets on canonicalized edges.
- **FR-008**: `knowledge_query_cypher` MUST be audited: if it performs any Cypher string interpolation of caller-supplied param values, those values MUST go through the shared coercion helper. If it passes params as structured typed query parameters to lbug (not string interpolation), that approach is acceptable as-is and must be documented.
- **FR-009**: A unit test suite (`crates/core/src/coerce.rs` or `crates/core/tests/coerce_tests.rs`) MUST be added covering: TIMESTAMP coercion from RFC-3339 string; FLOAT[] coercion from a Vec<f32>; string escaping (apostrophe); null coercion; integer coercion; boolean coercion.
- **FR-010**: A round-trip integration test MUST be added for `knowledge_dump_wal` that: (a) inserts an entity with a known `created_at` and a non-null embedding; (b) calls `knowledge_dump_wal`; (c) reads the emitted WAL lines and asserts TIMESTAMP and FLOAT[] literal formats; (d) wipes the DB; (e) replays the dump; (f) queries the entity and asserts `created_at` value is byte-identical to the original.
- **FR-011**: An integration test (or extension to an existing test) MUST verify that `insert_relates_to_edge` / `insert_mentions_edge` write a `TIMESTAMP`-typed `created_at` and a `FLOAT[]`-typed embedding (not `STRING`). The test must query the lbug value type, not just parse the string.
- **FR-012**: An integration test MUST verify that `apply_same_as_correction` in `corrections.rs` does not produce `TYPE_MISMATCH` errors and writes `TIMESTAMP`-typed timestamps.
- **FR-013**: A **write-path inventory** MUST be maintained as a comment block in the shared coercion module listing every write path, whether it uses the shared helper or typed query params, and a one-line status. This serves as a checklist so future write paths know the required pattern.
- **FR-014**: The `vecf32(` legacy shim (`legacy_wal.rs` `strip_vecf32`) MUST remain in place for WAL replay of pre-existing WAL files — this issue does not remove backward compatibility. But NO NEW write path may emit `vecf32(...)` syntax.

### Key Entities

- **Shared coercion helper**: A Rust function (or small module) that accepts a value (as `serde_json::Value` or typed Rust variants) and a schema type hint, and returns a correctly-formatted Cypher literal string. Replaces all per-path hand-rolled formatting.
- **Write path**: Any code path that builds and executes a Cypher statement that writes to the lbug DB — distinguished from the WAL-replay path which routes through `json_to_cypher_literal` already.
- **Type coercion**: The transformation from Rust/JSON representation to lbug/Kuzu Cypher literal syntax: datetime → `TIMESTAMP('...')`, float vec → `[f32, ...]`, string → `'...'` (escaped), null → `NULL`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The function `json_to_cypher_literal` (or its equivalent) exists in exactly one place in the codebase; `grep -rn "json_to_cypher_literal\|to_cypher_literal" --include="*.rs" .` returns a definition site and N call sites, none of which are duplicated implementations.
- **SC-002**: `grep -rn "vecf32(" --include="*.rs" .` returns zero results in write paths (only in `legacy_wal.rs` for backward-compat reading, and possibly in comments/tests).
- **SC-003**: The `knowledge_dump_wal` round-trip integration test passes: `created_at` is byte-identical before and after dump→wipe→replay.
- **SC-004**: `cargo test` passes with zero `TYPE_MISMATCH` log lines in any test that exercises a direct-write path.
- **SC-005**: The write-path inventory comment in the shared coercion module lists every write path identified in this audit with a `[✓ shared helper]` or `[✓ typed params]` status, and no entry has an `[unchecked]` or `[TODO]` status.
- **SC-006**: `cargo clippy --release --all-targets -- -D warnings` passes with zero new warnings introduced by this change.
- **SC-007**: The `knowledge_dump_wal` WAL output contains no lines matching the pattern `vecf32\(` (verified by the round-trip test grepping the output directory).

## Assumptions

- The existing `json_to_cypher_literal` in `replay.rs` is the authoritative correct implementation for all coercion cases; this issue generalizes it rather than rewriting it.
- `insert_relates_to_edge` and `insert_mentions_edge` may already use lbug's typed query-parameter binding (not string interpolation) for some or all values — the audit must determine this. If they use typed params, the coercion is handled at the DB driver layer and no change is needed, but this must be confirmed and documented.
- `knowledge_query_cypher` is a pass-through for user-supplied Cypher; if users supply raw Cypher strings (not params), the method has no mechanism to coerce them and this is explicitly out of scope. Only user-supplied *params* that are interpolated by the service are in scope.
- The coercion helper does not need to handle lbug-specific types beyond TIMESTAMP, FLOAT[], strings, integers, booleans, and null — these are the only types used in the current schema (`schema.rs`).
- Issue #169 (the P0 TIMESTAMP fix in `merge_entities`) is a prerequisite that must be merged before this audit begins, so the audit starts from a baseline where `merge_entities` is already corrected.
- No behavior change is intended for end users — this is a correctness refactor. The observable effect is elimination of potential `TYPE_MISMATCH` errors and correct round-tripping of all values.

## Out of Scope

- Adding new write paths or IPC methods (the audit covers only existing paths).
- Removing the `strip_vecf32` legacy shim from `legacy_wal.rs` — backward compatibility with existing WAL files is preserved.
- Coercing user-supplied raw Cypher strings in `knowledge_query_cypher` — only param interpolation is in scope.
- Performance optimization of the coercion helper — correctness only.
- Changes to the WAL file format or WAL schema.
- Fuzzing the coercion helper against arbitrary inputs — unit tests cover the schema-relevant types only.

## Source References

- `crates/core/src/replay.rs` — current home of `json_to_cypher_literal` (generalization target)
- `crates/core/src/db.rs` — `insert_relates_to_edge`, `insert_mentions_edge` (audit targets)
- `crates/core/src/corrections.rs` — `apply_same_as_correction`, `apply_retract_correction`, `apply_entity_type_labels` (audit targets)
- `crates/core/src/legacy_wal.rs` — `strip_vecf32` shim (to be preserved, not removed)
- `crates/core/src/schema.rs` — canonical column list per node/edge type (ground truth for which properties need coercion)
- Issue #161 — `knowledge_dump_wal` (audit target; round-trip test required)
- Issue #162 — `knowledge_merge_entities` (the triggering incident; fix merged in #169)
- Issue #163 — Relation canonicalization (audit target)
- Issue #128 — apostrophe escaping fix (prior coercion fix; must be preserved in shared helper)
- Issue #130 — TIMESTAMP literal fix (prior coercion fix; must be preserved in shared helper)
- Issue #133 — FLOAT[] vs vecf32 fix (prior coercion fix; must be preserved in shared helper)
- `crates/core/tests/ipc_parity.rs` — integration test suite to extend
