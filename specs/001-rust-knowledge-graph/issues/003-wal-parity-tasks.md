# Tasks: Issue #3 — WAL Parity for Git-Friendly Persistence

**Input**: `specs/001-rust-knowledge-graph/issues/003-wal-parity-plan.md`, issue #3 body  
**Prerequisites**: Issue #1 (Foundation) merged to `main` ✓

## Format: `[ID] [P?] [Story] [Constitution-tag?] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[WAL]**: Touches WAL serialization or replay logic; TDD MANDATORY (test written and failing before implementation commit)

---

## Phase 1: Dependencies & Error Infrastructure

**Purpose**: Add new Cargo dependencies and extend the error type. Blocks all WAL code.

- [ ] T001 Add `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `chrono = { version = "0.4", features = ["serde"] }`, `uuid = { version = "1", features = ["v4"] }` to `Cargo.toml` workspace `[workspace.dependencies]` and opt all four into `liminis-graph-core/Cargo.toml`
- [ ] T002 Extend `liminis-graph-core/src/error.rs` with two new variants: `WalIo(#[from] std::io::Error)` (wrapping `std::io::Error`) and `WalParse(String)` (for JSON parse errors during replay)

**Checkpoint**: `cargo build -p liminis-graph-core` succeeds with new deps; `cargo clippy -- -D warnings` clean

---

## Phase 2: Golden Fixture Files (TDD setup)

**Purpose**: Commit the small fixture JSONL files that the compat tests will validate against. Must exist before tests are written so the tests can reference real file paths.

- [ ] T003 [P] Create `liminis-graph-core/tests/fixtures/wal/python_produced.jsonl` — 5 JSONL lines hand-crafted to match Python WAL format exactly: 2 `MERGE (n:Entity …) SET …` lines and 2 `MERGE (n:Episodic …) SET …` lines, plus 1 `SET n.attributes = $attrs` line, all with `dim=4` embeddings, `seq` values 0–4, `ts` as ISO-8601 UTC strings, `db` as `""` (matching Python default). Document expected replay counts in a comment at the top of the file.
- [ ] T004 [P] Create `liminis-graph-core/tests/fixtures/wal/rust_produced_expected.jsonl` — 1 JSONL line showing the exact expected Rust serialization of a `WalLine { seq: 1, ts: "2026-05-19T00:00:00.000000+00:00", db: "graphiti", cypher: "MERGE (n:Entity {uuid: $uuid})", params: {"uuid": "test-uuid-1"} }` with fields in `seq, ts, db, cypher, params` order (no extra whitespace). Used as a pin test for key ordering.

**Checkpoint**: Both fixture files exist and are valid JSONL (each line parseable by `jq '.'`)

---

## Phase 3: WalLine Type — TDD First [WAL]

**Purpose**: TDD for `WalLine` serialization. Write failing tests first, then implement.

### Tests (write first, must fail before T007)

- [ ] T005 [P] [WAL] Write `liminis-graph-core/tests/wal_serialization.rs` with tests:
  - `test_walline_serializes_field_order`: assert that `serde_json::to_string(&WalLine {...})` produces JSON with keys in order `seq, ts, db, cypher, params`
  - `test_walline_roundtrip`: serialize then deserialize and assert all fields equal
  - `test_walline_deserializes_python_fixture`: parse every line of `tests/fixtures/wal/python_produced.jsonl` and assert `Ok(WalLine {..})` with the expected `seq` values
  - `test_walline_pins_rust_output`: serialize a known `WalLine` and assert byte-for-byte equality against `tests/fixtures/wal/rust_produced_expected.jsonl`

### Implementation

- [ ] T006 [WAL] Implement `liminis-graph-core/src/wal.rs` — `WalLine` struct with `#[derive(Debug, Clone, Serialize, Deserialize)]`, fields declared in order `seq: u64, ts: String, db: String, cypher: String, params: serde_json::Value` (satisfies R-01, AD-W1)

**Checkpoint**: `cargo test -p liminis-graph-core wal_serialization` passes (all 4 tests green)

---

## Phase 4: WalWriter — TDD First [WAL]

**Purpose**: TDD for `WalWriter` appender. Write failing tests first, then implement.

### Tests (write first, must fail before T009)

- [ ] T007 [P] [WAL] Write `liminis-graph-core/tests/wal_appender.rs` with tests:
  - `test_with_chunk_writes_file_on_success`: call `with_chunk`, log 3 mutations, return `Ok`; assert exactly one `.jsonl` file exists in `TempDir` with 3 lines
  - `test_with_chunk_discards_on_error`: call `with_chunk`, log 2 mutations, return `Err`; assert no `.jsonl` files exist in `TempDir` (R-02)
  - `test_seq_monotonic_across_chunks`: two successive `with_chunk` calls; assert the `seq` values in the second file are strictly greater than those in the first
  - `test_seq_resumes_from_existing_wal`: create a `TempDir` with a pre-written fixture line containing `"seq": 7`, construct a new `WalWriter` pointing at that dir, log one mutation; assert `seq == 8`
  - `test_file_rotation_on_max_events`: set `max_events_per_file = 2`; write a chunk with 2 lines, then a chunk with 2 lines; assert 2 `.jsonl` files exist
  - `test_mutation_filter_excludes_reads`: call `log_mutation` with `MATCH (n) RETURN n`; assert the pending buffer is empty (mutation filter excludes reads)
  - `test_mutation_filter_excludes_index_ddl`: call `log_mutation` with `CREATE_VECTOR_INDEX(…)`; assert the pending buffer is empty
  - `test_filename_format`: write one chunk; assert the created filename matches the regex `^\d{8}_\d{6}_[0-9a-f]{6}_0000\.jsonl$`

### Implementation

- [ ] T008 [WAL] Implement `liminis-graph-core/src/wal.rs` — `WalWriter` struct and methods: `new(wal_dir, max_events_per_file)` (scans existing files for max seq per AD-W4), `log_mutation(cypher, params, database)` (filters non-mutations per AD-W7, pushes `WalLine` to `pending_lines`), `with_chunk<F,T>(f)` (chunk-atomicity per AD-W2; rotates if needed per AD-W3; writes all pending lines atomically via `BufWriter` + `flush` + fsync)

**Checkpoint**: `cargo test -p liminis-graph-core wal_appender` passes (all 8 tests green)

---

## Phase 5: WalReplayer — TDD First [WAL]

**Purpose**: TDD for the cold-boot replayer. Write failing tests first, then implement.

### Tests (write first, must fail before T011)

- [ ] T009 [P] [WAL] Write `liminis-graph-core/tests/wal_replay.rs` with tests:
  - `test_replay_files_in_lexicographic_order`: create 3 JSONL files named with timestamps in non-creation order; write Entity MERGEs with `seq` values that only make sense if files are read in filename order; replay; assert final entity count equals the number of unique UUIDs (R-07)
  - `test_replay_tolerates_truncated_final_line`: write a JSONL file whose last line is `{"seq":5,"ts":"2026-05-19T00:` (truncated); assert `replay()` returns `Ok` and `stats.lines_skipped == 1` (R-05)
  - `test_replay_skips_unknown_op_without_abort`: write a JSONL line with `"cypher": "EXPLAIN MATCH (n) RETURN n"` (first token EXPLAIN, unknown); assert `replay()` returns `Ok` and the line is counted in `stats.lines_skipped` (R-08)
  - `test_replay_empty_dir_succeeds`: call `replay()` on an empty `TempDir`; assert `Ok(ReplayStats { lines_replayed: 0, lines_skipped: 0, files_read: 0 })`
  - `test_replay_golden_fixture_counts`: replay `tests/fixtures/wal/python_produced.jsonl` against a fresh LadybugDB (via `Db::open` + `Conn::init_schema(4)`); assert entity count = 2, episodic count = 2 (R-04, R-06 — counts match known Python-built baseline)

### Implementation

- [ ] T010 [WAL] Implement `liminis-graph-core/src/replay.rs` — `ReplayStats` struct and `WalReplayer` with `new(wal_dir)` and `replay(conn)`: collect `*.jsonl` files, sort lexicographically, iterate lines, parse, classify first token, skip unknown with `eprintln!` warning, execute known ops via `conn.raw_query()`, skip truncated final lines (R-05, R-07, R-08)

**Checkpoint**: `cargo test -p liminis-graph-core wal_replay` passes (all 5 tests green)

---

## Phase 6: Forward-Compat Structural Tests [WAL]

**Purpose**: Verify Rust-produced WAL lines are accepted by the Python reader format (R-03) using the committed golden fixtures — no Python runtime required.

- [ ] T011 [P] [WAL] Write `liminis-graph-core/tests/wal_compat.rs` with tests:
  - `test_forward_compat_five_fields_present`: serialize any `WalLine` to JSON and assert the parsed JSON object has exactly the keys `["seq","ts","db","cypher","params"]` — no extra fields
  - `test_forward_compat_ts_is_string`: assert the `ts` field is a JSON string (not a number or object) and parses as a valid ISO-8601 datetime with UTC timezone marker
  - `test_forward_compat_params_is_object`: assert the `params` field is a JSON object (not an array or null)
  - `test_backward_compat_python_fixture_parseable`: parse all lines of `tests/fixtures/wal/python_produced.jsonl` with `serde_json::from_str::<WalLine>` and assert all succeed (R-04 structural)
  - `test_backward_compat_seq_monotonic`: parse `python_produced.jsonl`; assert `seq` values are non-decreasing

**Checkpoint**: `cargo test -p liminis-graph-core wal_compat` passes (all 5 tests green)

---

## Phase 7: Db::open_or_rebuild Integration

**Purpose**: Wire cold-boot detection to the replayer, enabling R-06.

- [ ] T012 [WAL] Add `Db::open_or_rebuild(db_path: &str, wal_dir: &str, embedding_dim: usize) -> Result<Self, Error>` to `liminis-graph-core/src/db.rs`: if `db_path` does not exist AND `wal_dir` contains `*.jsonl` files, create fresh DB, `init_schema(embedding_dim)`, `WalReplayer::new(wal_dir).replay(&conn)?`, return `Db`. If `db_path` does not exist AND no JSONL files, call `Db::open(db_path)` (creates fresh DB). If `db_path` exists, call `Db::open(db_path)` (normal path).
- [ ] T013 [WAL] Write integration test `test_open_or_rebuild_from_wal` in `liminis-graph-core/tests/wal_replay.rs`: use `TempDir`, write the golden fixture into a `wal/` subdirectory, call `Db::open_or_rebuild(db_path, wal_dir, 4)`; assert entity count = 2, episodic count = 2 (R-06)

**Checkpoint**: `cargo test -p liminis-graph-core` — all existing + new tests pass

---

## Phase 8: Library Exports & Bench Stub

**Purpose**: Export the new public API from `lib.rs` and add a bench stub so CI compiles it.

- [ ] T014 [P] Update `liminis-graph-core/src/lib.rs` to add `pub mod wal; pub mod replay;` and re-export `pub use wal::{WalLine, WalWriter}; pub use replay::{WalReplayer, ReplayStats};`
- [ ] T015 [P] Add `liminis-graph-core/benches/wal_replay_bench.rs` — a `criterion` bench stub that imports `WalReplayer` and defines an empty `criterion_group!` / `criterion_main!` (no actual timing); add `[[bench]] name = "wal_replay_bench" harness = false` to `liminis-graph-core/Cargo.toml`

**Checkpoint**: `cargo build -p liminis-graph-core` + `cargo bench --no-run -p liminis-graph-core` both succeed; `cargo clippy -- -D warnings` clean

---

## Phase 9: CI Update

**Purpose**: Ensure CI passes with all new tests and the bench stub.

- [ ] T016 Verify `.github/workflows/ci.yml` `test` job runs `cargo test` (which now includes all `wal_*` test files). No structural CI changes needed unless the golden fixture files require special handling. If `cargo test` fails on CI due to lbug extension download (network-dependent), investigate and fix — do not skip.

**Checkpoint**: Push to `fabrik/issue-3` → CI `test` job passes on both `ubuntu-latest` and `macos-latest`

---

## Dependencies & Execution Order

| Phase | Depends On | Notes |
|-------|------------|-------|
| Phase 1 (T001, T002) | Issue #1 merged | Blocking — must complete first |
| Phase 2 (T003, T004) | T001 | Parallel; fixture files must exist before test files reference them |
| Phase 3 tests (T005) | T002, T003, T004 | Tests written first — must fail before T006 |
| Phase 3 impl (T006) | T005 | Implements `WalLine` to make T005 pass |
| Phase 4 tests (T007) | T006 | Tests written first — must fail before T008 |
| Phase 4 impl (T008) | T007 | Implements `WalWriter` to make T007 pass |
| Phase 5 tests (T009) | T008 | Tests written first — must fail before T010 |
| Phase 5 impl (T010) | T009 | Implements `WalReplayer` to make T009 pass |
| Phase 6 (T011) | T006, T010, T004 | Can start once WalLine + Replayer + fixtures exist |
| Phase 7 (T012, T013) | T010 | Db extension depends on WalReplayer existing |
| Phase 8 (T014, T015) | T006, T010, T012 | Wire-up and bench stub; parallel with each other |
| Phase 9 (T016) | All | Validate CI last |

### Parallel Opportunities

- T003 and T004 (fixture files) can be written in parallel.
- T005 (serialization tests) and T007 (appender tests) can be written in parallel once T006 is implemented.
- T014 and T015 can be done in parallel.

---

## Constitution Compliance

| Task | Tag | Gate |
|------|-----|------|
| T005, T006 | [WAL] | T005 must produce failing `cargo test` before T006 commits |
| T007, T008 | [WAL] | T007 must produce failing `cargo test` before T008 commits |
| T009, T010 | [WAL] | T009 must produce failing `cargo test` before T010 commits |
| T011 | [WAL] | Structural compat test; no Python runtime required |
| T013 | [WAL] | Integration test proving R-06 cold-boot rebuild |
| T015 | (bench stub) | `cargo bench --no-run` in CI satisfies bench-stub requirement |
| All | [Principle IV] | `with_chunk` API enforces WAL-before-DB; confirmed by T007 `test_with_chunk_discards_on_error` |
| All | [Principle V] | Run `cargo tree | grep -E "tch|candle|onnxruntime"` before final commit — must return empty |
