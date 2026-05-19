# Tasks: Issue #1 — Foundation

**Input**: `specs/001-rust-knowledge-graph/issues/001-foundation-plan.md`, `specs/001-rust-knowledge-graph/issues/001-foundation-spec.md`

## Format: `[ID] [P?] [Story] [Constitution-tag?] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[LDB]**: Touches the LadybugDB driver layer; parity test REQUIRED

---

## Phase 1: Workspace Skeleton

**Purpose**: Create the Cargo workspace, crate stubs, and repo skeleton. All Phase 1 tasks can run in parallel once the workspace `Cargo.toml` exists.

- [ ] T001 Create workspace `Cargo.toml` with `[workspace]` members `["liminis-graph-core", "liminis-graph"]` and `[workspace.dependencies]` for `lbug = "=0.16.1"`, `tempfile`, `thiserror`
- [ ] T002 [P] Create `liminis-graph-core/Cargo.toml` (lib crate, workspace dep `lbug`) and `liminis-graph-core/src/lib.rs` (empty pub mod stubs)
- [ ] T003 [P] Create `liminis-graph/Cargo.toml` (bin crate, depends on `liminis-graph-core`) and `liminis-graph/src/main.rs` (prints `"liminis-graph starting"` stub)
- [ ] T004 [P] Create `liminis-graph-core/benches/placeholder.rs` with a bench stub and a `[[bench]]` entry in `liminis-graph-core/Cargo.toml`
- [ ] T005 [P] Create `docs/adr/0001-record-architecture-decisions.md` — meta-ADR explaining why ADRs are used in this project
- [ ] T006 [P] Create `.github/ISSUE_TEMPLATE/bug_report.md`, `.github/ISSUE_TEMPLATE/feature_request.md`, and `.github/PULL_REQUEST_TEMPLATE.md` referencing the constitution at `.specify/memory/constitution.md`

**Checkpoint**: `cargo build` at workspace root compiles both crates (no logic yet)

---

## Phase 2: Core Library Implementation [LDB]

**Purpose**: Implement the synchronous `liminis-graph-core` public API. These tasks must be done in dependency order within the phase.

- [ ] T007 Implement `liminis-graph-core/src/error.rs` — `Error` enum with variants for `lbug::Error`, `InvalidPath`, `QueryFailed(String)` using `thiserror`
- [ ] T008 [P] Implement `liminis-graph-core/src/types.rs` — `EntityRow` and `EpisodicRow` plain structs with all fields from the schema DDL (uuid, name, group_id, embedding as `Vec<f32>`, etc.)
- [ ] T009 Implement `liminis-graph-core/src/db.rs` — `Db::open(path: &str)`, `Db::connect()` (constructs `Conn` and loads vector + fts extensions per AD-2), `Conn::insert_entity`, `Conn::insert_episodic`, `Conn::create_vector_indexes` (documented: must be called after inserts per AD-4), `Conn::search_entities(name_prefix)` (depends on T007, T008)
- [ ] T010 Implement `liminis-graph-core/src/schema.rs` — `init(conn: &Conn, embedding_dim: usize)` that runs the Entity and Episodic `CREATE NODE TABLE IF NOT EXISTS` DDL with the dim parameter substituted (depends on T007)
- [ ] T011 Wire up `liminis-graph-core/src/lib.rs` — pub re-exports for `Db`, `Conn`, `Error`, `EntityRow`, `EpisodicRow`, `schema::init` (depends on T007–T010)

**Checkpoint**: `cargo build -p liminis-graph-core` succeeds; `cargo clippy -p liminis-graph-core -- -D warnings` clean

---

## Phase 3: Integration Test / LadybugDB Spike [LDB]

**Purpose**: Prove `lbug 0.16.1` can open a file DB, write Entity+Episodic nodes with 768-dim vectors, round-trip HNSW and FTS indexes, and read results back. This is the acceptance gate for the spike.

- [ ] T012 [LDB] Write `liminis-graph-core/tests/integration_spike.rs` — sync `#[test]` that: (1) opens a `tempfile::TempDir` DB, (2) connects (loads extensions), (3) `schema::init(conn, 768)`, (4) inserts 3 `EntityRow` with `name_embedding: vec![0.0f32; 768]`, (5) inserts 1 `EpisodicRow` with `content_embedding: vec![0.0f32; 768]`, (6) calls `create_vector_indexes()`, (7) calls `search_entities("")` and asserts 3 results (depends on T009–T011)

**Checkpoint**: `cargo test -p liminis-graph-core` passes on local Linux and macOS

---

## Phase 4: Example Consumer (Principle II Gate)

**Purpose**: Demonstrate the library API is a peer to the binary — an external consumer can ingest documents and search using only `liminis-graph-core`.

- [ ] T013 Write `examples/basic_ingest/main.rs` (≤ 50 lines, no Liminis-internal deps) — opens a temp DB, ingests 3 sample `EntityRow` docs with synthetic embeddings, creates indexes, searches by name prefix, prints results to stdout. Add `[[example]]` entry in workspace `Cargo.toml` (depends on T011)

**Checkpoint**: `cargo run --example basic_ingest` succeeds and prints entity names

---

## Phase 5: Binary Stub (Principle II Symmetry)

**Purpose**: Wire the binary crate to the library, demonstrating Principle II symmetry at the binary level.

- [ ] T014 Expand `liminis-graph/src/main.rs` to open a DB path from `std::env::args`, connect, print schema version or a status message using `liminis-graph-core`. Must compile and run without panicking (depends on T011)

**Checkpoint**: `cargo run -p liminis-graph -- /tmp/test.db` exits 0 and prints a status line

---

## Phase 6: GitHub Actions CI

**Purpose**: Green CI on every push, covering build/test/clippy/fmt on Linux+macOS and a bench compile stub.

- [ ] T015 Create `.github/workflows/ci.yml` with:
  - `test` job: matrix `[ubuntu-latest, macos-latest]`, steps: checkout → `dtolnay/rust-toolchain@stable` (with clippy + rustfmt components) → `cargo build --release` → `cargo test` → `cargo clippy -- -D warnings` → `cargo fmt --check`
  - `bench-stub` job: `ubuntu-latest`, steps: checkout → toolchain → `cargo bench --no-run` (compile only; no timing)

**Checkpoint**: Push to `fabrik/issue-1` → both CI jobs pass on GitHub Actions

---

## Dependencies & Execution Order

| Phase | Depends On | Notes |
|-------|-----------|-------|
| Phase 1 (T001) | nothing | Must complete first |
| Phase 1 (T002–T006) | T001 | All parallel after T001 |
| Phase 2 (T007, T008) | Phase 1 | Parallel with each other |
| Phase 2 (T009, T010) | T007, T008 | Parallel with each other |
| Phase 2 (T011) | T009, T010 | Wire-up step |
| Phase 3 (T012) | T011 | Integration spike — blocks Principle III gate |
| Phase 4 (T013) | T011 | Can start once T011 done |
| Phase 5 (T014) | T011 | Can start once T011 done; parallel with T013 |
| Phase 6 (T015) | All | Write CI last once local build is green |

### Parallel Opportunities

Once T001 is done: T002–T006 all in parallel.
Once T011 is done: T012, T013, T014 all in parallel.
T015 can be written any time but should not be pushed until T012 passes locally.

---

## Constitution Compliance

| Task | Tag | Gate |
|------|-----|------|
| T009, T010, T012 | [LDB] | Integration spike (T012) IS the parity test — must pass on both runners |
| T004 | (bench stub) | `cargo bench --no-run` in CI satisfies the bench-stub requirement |
| All | [Principle V] | Run `cargo tree \| grep -E "tch\|candle\|onnxruntime"` before final commit — must return empty |
