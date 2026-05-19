# Implementation Plan: Issue #1 — Foundation

**Branch**: `fabrik/issue-1` | **Date**: 2026-05-19 | **Spec**: `specs/001-rust-knowledge-graph/issues/001-foundation-spec.md`

## Summary

Stand up the Cargo workspace (`liminis-graph-core` lib + `liminis-graph` bin), prove the `lbug 0.16.1` Rust bindings can open a DB, write Entity/Episodic nodes with 768-dim vector properties, and round-trip HNSW + full-text indexes. Lay the repo skeleton (benches/, docs/adr/, .github/ templates) and green-field CI so all subsequent issues have somewhere to land.

## Technical Context

**Language/Version**: Rust (stable, 2021 edition)
**Primary Dependencies**: `lbug = "0.16.1"` (LadybugDB Rust bindings, MIT), `cxx = "=1.0.138"` (pinned by lbug), `tempfile` (integration tests only)
**Storage**: LadybugDB (community Kuzu fork) — local file, fresh DB for spike
**Testing**: `cargo test` — one integration test in `liminis-graph-core/tests/`
**Target Platform**: Linux (ubuntu-latest) + macOS (macos-latest) via GitHub Actions
**Project Type**: Cargo workspace — library crate + binary crate
**Performance Goals**: No performance hot-paths in scope for this issue; bench stub only
**Constraints**: Zero `tch`/`candle`/`onnxruntime` in `cargo tree`; `lbug` sync API only
**Scale/Scope**: Day-0 scaffold; the two crates, one integration test, one example, CI

## Architecture Decisions

### AD-1: Two-crate workspace, no driver abstraction

`liminis-graph-core` (lib) holds all DB interaction. `liminis-graph` (bin) depends on it. No trait for the DB driver — Principle III prohibits abstractions over LadybugDB. The library is the source of truth for all capabilities (Principle II).

### AD-2: Extension loading enforced at connection construction

`lbug` requires `INSTALL vector; LOAD EXTENSION vector;` and the FTS equivalent on every new connection. The `Conn::new()` constructor runs both unconditionally, so callers cannot forget. This mirrors the Python driver's `_load_extensions()` call in `__init__`.

### AD-3: Sync API, async boundary via spawn_blocking

`lbug` has no async bindings. The core library exposes a **synchronous** public API. Callers that need async (future IPC layer) wrap calls in `tokio::task::spawn_blocking`. The spike integration test and example are fully sync; no tokio dependency is introduced here.

### AD-4: HNSW index creation enforced after insertion

`CREATE_VECTOR_INDEX` must run after data is written (indexes block in-place vector writes). The library exposes `Conn::create_vector_indexes()` as a separate step with a doc comment explaining the ordering constraint. It is not called implicitly by insert operations.

### AD-5: Embedding dimension parameterized at schema creation

`FLOAT[768]` is hard-coded in the DDL for this issue (matching bge-base-en-v1.5). The `schema::init()` function takes a `dim: usize` parameter so later issues can override without changing the signature.

### AD-6: Extension network dependency on CI

`INSTALL vector` / `INSTALL fts` download extensions on first run. GitHub Actions runners have outbound internet access, so this works. The CI job runs with network enabled (default). If the precompiled `lbug` archive bundles the extensions, the download is a no-op — the spike will determine this at implementation time. No special CI step needed until the spike proves otherwise.

## Constitution Check

- **Principle I (IPC Parity)** — N/A. This issue does not touch the IPC surface.
- **Principle II (Library and Binary Are Peers)** — PASS. The example in `examples/basic_ingest/` calls the library API directly, proving every DB operation is reachable without the binary. The binary `src/main.rs` also exercises the library.
- **Principle III (LadybugDB Only)** — PASS. `lbug = "0.16.1"` pinned. No driver trait, no abstraction layer.
- **Principle IV (WAL Is Authoritative)** — N/A. No WAL operations in this issue.
- **Principle V (No ML Runtimes)** — PASS. Only `lbug`, `tempfile`, and std. `cargo tree` must show no `tch`, `candle`, or `onnxruntime`.

### Performance budget gates

Not applicable — no hot-path code in this issue. Bench stub job exists but contains no real benches.

### Workflow gates

- Spec exists at `specs/001-rust-knowledge-graph/issues/001-foundation-spec.md` ✓
- No IPC-touching changes in scope ✓
- Bench stub exists; no real hot-path benches needed yet ✓
- No WAL code in scope ✓
- No constitution deviations; no ADR needed beyond the meta-ADR ✓

## Project Structure

```text
Cargo.toml                              # workspace root
liminis-graph-core/
├── Cargo.toml
└── src/
    ├── lib.rs                          # public re-exports
    ├── error.rs                        # Error enum (thiserror)
    ├── db.rs                           # Db, Conn types + extension loading
    ├── schema.rs                       # DDL init, CREATE_VECTOR_INDEX
    └── types.rs                        # EntityRow, EpisodicRow (plain structs)
liminis-graph-core/tests/
└── integration_spike.rs                # round-trip test [LDB]
liminis-graph/
├── Cargo.toml
└── src/
    └── main.rs                         # thin CLI stub using core library
examples/
└── basic_ingest/
    └── main.rs                         # < 50 lines, no Liminis-internal deps
benches/
└── placeholder.rs                      # stub; no real benches
docs/
└── adr/
    └── 0001-record-architecture-decisions.md
.github/
├── ISSUE_TEMPLATE/
│   ├── bug_report.md
│   └── feature_request.md
└── PULL_REQUEST_TEMPLATE.md
.github/workflows/
└── ci.yml                              # build / test / clippy / fmt / bench-stub
```

## Public Library API

```rust
// liminis-graph-core/src/lib.rs
pub mod db;
pub mod error;
pub mod schema;
pub mod types;

pub use db::{Conn, Db};
pub use error::Error;
pub use types::{EntityRow, EpisodicRow};
```

```rust
// liminis-graph-core/src/db.rs
pub struct Db { inner: lbug::Database }
pub struct Conn { inner: lbug::Connection }

impl Db {
    pub fn open(path: &str) -> Result<Self, Error>;
    pub fn connect(&self) -> Result<Conn, Error>;  // loads extensions automatically
}

impl Conn {
    pub fn init_schema(&self, embedding_dim: usize) -> Result<(), Error>;
    pub fn insert_entity(&self, row: &EntityRow) -> Result<(), Error>;
    pub fn insert_episodic(&self, row: &EpisodicRow) -> Result<(), Error>;
    /// Must be called AFTER all insert_entity / insert_episodic calls.
    pub fn create_vector_indexes(&self) -> Result<(), Error>;
    pub fn search_entities(&self, name_prefix: &str) -> Result<Vec<EntityRow>, Error>;
}
```

## Schema DDL (verbatim from Python driver)

```sql
-- Entity table
CREATE NODE TABLE IF NOT EXISTS Entity (
    uuid STRING PRIMARY KEY,
    name STRING,
    group_id STRING,
    labels STRING[],
    created_at TIMESTAMP,
    name_embedding FLOAT[{dim}],
    summary STRING,
    attributes STRING
);

-- Episodic table
CREATE NODE TABLE IF NOT EXISTS Episodic (
    uuid STRING PRIMARY KEY,
    name STRING,
    group_id STRING,
    created_at TIMESTAMP,
    source STRING,
    source_description STRING,
    content STRING,
    content_embedding FLOAT[{dim}],
    valid_at TIMESTAMP,
    entity_edges STRING[]
);
```

Extension loading (per-connection):
```sql
INSTALL vector; LOAD EXTENSION vector;
INSTALL fts; LOAD EXTENSION fts;
```

HNSW index creation (after data insertion):
```sql
CALL CREATE_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', 'name_embedding', metric := 'cosine')
CALL CREATE_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx', 'content_embedding', metric := 'cosine')
```

## Integration Test (Spike)

File: `liminis-graph-core/tests/integration_spike.rs`

Steps:
1. Create `tempfile::TempDir` for the DB path.
2. `Db::open(&path)` → `Db::connect()` (loads extensions automatically).
3. `conn.init_schema(768)` — creates Entity + Episodic tables.
4. Insert 3 `EntityRow` values with synthetic `name_embedding: vec![0.0f32; 768]`.
5. Insert 1 `EpisodicRow` with synthetic `content_embedding: vec![0.0f32; 768]`.
6. `conn.create_vector_indexes()`.
7. `conn.search_entities("")` — assert results contain the 3 entities.
8. Assert no panic, no error.

The test uses `#[test]` (not `#[tokio::test]`) since the API is sync.

## CI Workflow

File: `.github/workflows/ci.yml`

```yaml
on: [push, pull_request]
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo build --release
      - run: cargo test
      - run: cargo clippy -- -D warnings
      - run: cargo fmt --check

  bench-stub:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo bench --no-run   # compile only; no real benches yet
```

## Example Consumer

File: `examples/basic_ingest/main.rs` (≤ 50 lines)

Uses `liminis_graph_core::{Db, EntityRow, EpisodicRow}`. Opens a temp DB, inserts 3 entities with synthetic embeddings, creates indexes, searches, prints results. Demonstrates Principle II — no `liminis-graph` binary code involved.

## Complexity Tracking

No constitution violations. No complexity entries needed.
