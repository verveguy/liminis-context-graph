# Implementation Plan: Issue #3 — WAL Parity for Git-Friendly Persistence

**Branch**: `fabrik/issue-3` | **Date**: 2026-05-19 | **Spec**: `specs/001-rust-knowledge-graph/issues/` (see issue body)

## Summary

Add two new modules to `liminis-graph-core` — `wal.rs` (appender) and `replay.rs` (cold-boot replayer) — that produce and consume JSONL WAL files format-compatible with the Python `graphiti_core/driver/wal.py` implementation. Every mutation appends to `.graphiti/wal/` before the DB write commits (Principle IV). On cold boot with the DB absent, the replayer reads all JSONL files in lexicographic order and re-executes mutations against a fresh LadybugDB connection, tolerating truncated final lines. TDD is mandatory for all WAL code.

## Technical Context

**Language/Version**: Rust (stable, 2021 edition)  
**Primary Dependencies**: `lbug = "=0.16.1"` (existing), `serde` + `serde_json` (JSON), `chrono` (ISO-8601 timestamps), `uuid` (session IDs)  
**Storage**: LadybugDB — local file, `.graphiti/wal/` JSONL files alongside the DB  
**Testing**: `cargo test` — integration tests in `liminis-graph-core/tests/`; TDD mandatory per R-09  
**Target Platform**: Linux (ubuntu-latest) + macOS (macos-latest) via GitHub Actions  
**Project Type**: Library extension — two new modules in `liminis-graph-core`, no binary changes  
**Performance Goals**: Cold-boot WAL replay ≥ 3× Python baseline (constitution budget; not the focus here — structural correctness is)  
**Constraints**: No ML runtimes; no driver abstraction; WAL format MUST remain JSONL with the five-field schema  
**Scale/Scope**: Single workspace, single session; no multi-workspace or cross-group isolation

## Architecture Decisions

### AD-W1: Named `WalLine` struct, field-declaration-order JSON serialization

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct WalLine {
    pub seq:    u64,
    pub ts:     String,
    pub db:     String,
    pub cypher: String,
    pub params: serde_json::Value,
}
```

`serde_json` serializes named struct fields in declaration order. Fields are declared in `seq, ts, db, cypher, params` order — matching Python's `json.dumps()` dict insertion order. This is not a byte-identical guarantee but is sufficient for diff-readability and downstream tooling.

### AD-W2: Chunk-atomicity via closure API

```rust
impl WalWriter {
    pub fn with_chunk<F, T>(&mut self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut WalWriter) -> Result<T, Error>;
}
```

`with_chunk` invokes the closure (which calls `log_mutation()` one or more times — each call pushes to `pending_lines`), then on `Ok` writes all `pending_lines` atomically to a JSONL file. On `Err`, `pending_lines` is discarded. This satisfies R-02.

> **Note on spec vs. Python reality**: The issue body references a `chunk()` context manager in `wal.py`. Research found no such function in the actual Python codebase. Chunk-atomicity (R-02) is therefore a **new invariant introduced by the Rust service**, not a port of existing Python behavior. The spec reference was aspirational, not descriptive.

### AD-W3: Soft `max_events_per_file` — chunk wins, never splits

If committing a chunk would push `events_in_current_file + chunk_lines > max_events_per_file`, the writer rotates to a new file before writing the chunk. A chunk is never split across two files. A chunk larger than `max_events_per_file` goes to a single file that exceeds the limit — correctness over byte-matching.

### AD-W4: Backward scan for global sequence initialization

On `WalWriter::new()`, the writer scans all existing `.jsonl` files in reverse lexicographic order, reads the last non-empty line of each file (handling truncated bytes by searching backwards), parses `seq` from the JSON, and starts at `max_seq + 1`. This mirrors Python's `_scan_existing_files()` logic.

### AD-W5: Structural forward-compat test — no Python runtime required

R-03 forward-compat is validated purely in Rust:
1. A unit test asserts that `WalLine { seq: 1, ts: ..., db: "graphiti", cypher: "MERGE ...", params: {...} }` serializes to JSON with exactly the five fields in the correct order and with string-typed `ts`.
2. A golden fixture test parses a small committed Python-produced JSONL file and asserts each line deserializes to `WalLine` without error. This proves the schema is parseable in both directions without invoking Python.

### AD-W6: Small committed golden fixtures for backward/forward-compat tests

Two synthetic golden fixture files are committed to `liminis-graph-core/tests/fixtures/wal/`:
- `python_produced.jsonl` — 5 WAL lines hand-crafted to match Python output format exactly (Entity MERGE + Episodic MERGE), with short embeddings (dim=4) to keep the file small. The expected replay counts (nodes, edges) are hardcoded in the test.
- `rust_produced_expected.jsonl` — the expected Rust serialization output for a specific `WalLine` struct instance, used to pin the output format.

Full-size demo WAL files (600KB each, 768-dim embeddings) are too large to commit; the fixture approach covers format correctness without requiring the Python service in CI.

### AD-W7: Simple Cypher first-token mutation classification

Mutation detection uses `cypher.trim().split_whitespace().next().unwrap_or("").to_uppercase()` checked against a static `HashSet` of mutation keywords: `{CREATE, MERGE, SET, DELETE, DETACH, DROP, REMOVE}`. Index DDL is detected by checking if the Cypher contains `CREATE_VECTOR_INDEX` or `CREATE INDEX` or `DROP INDEX`. No `regex` dependency — the actual Cypher patterns the Rust service produces are well-controlled and don't have embedded string literals that contain mutation keywords.

For replay (R-08), the replayer classifies each line's Cypher by the same first-token method. If the first token is not in the known set, it logs a structured warning (`tracing::warn!`) and skips the line — replay does not abort.

### AD-W8: New workspace dependencies

```toml
# Cargo.toml (workspace dependencies)
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
chrono     = { version = "0.4", features = ["serde"] }
uuid       = { version = "1", features = ["v4"] }
```

`liminis-graph-core/Cargo.toml` opts in to all four. `liminis-graph/Cargo.toml` is unchanged (WAL is a library concern). No `tracing` dependency is added in this issue — warnings use `eprintln!` with structured text for now; a future issue can add `tracing`.

### AD-W9: Cold-boot detection in `Db::open`

`Db::open(path)` is extended with a companion method:

```rust
impl Db {
    pub fn open_or_rebuild(db_path: &str, wal_dir: &str, embedding_dim: usize) -> Result<Self, Error>;
}
```

If `db_path` does not exist but `wal_dir` exists and contains `.jsonl` files, this method creates a fresh DB, initializes the schema, runs `WalReplayer::replay()`, and returns the rebuilt `Db`. If neither exists, it behaves like `Db::open()` (creates a fresh empty DB). This satisfies R-06.

## Constitution Check

- **Principle I (IPC Parity)** — N/A. This issue does not touch the Unix-socket IPC surface.
- **Principle II (Library and Binary Are Peers)** — PASS. All WAL code lives in `liminis-graph-core` (library). `WalWriter`, `WalReplayer`, and `Db::open_or_rebuild` are reachable without the binary. No binary-only behavior introduced.
- **Principle III (LadybugDB Only)** — PASS. No driver abstraction introduced. The replayer executes mutations via `Conn::raw_query()` (existing LadybugDB binding). No new DB backend.
- **Principle IV (WAL Is Authoritative)** — PASS. The `with_chunk` API enforces WAL-before-DB: callers write WAL inside the closure; the DB mutation follows the closure's success return. The appender commits WAL atomically before the caller proceeds to the DB write. Format stays JSONL with the five-field schema. No MAJOR bump needed (format unchanged from Python).
- **Principle V (No ML Runtimes)** — PASS. Only `serde`, `serde_json`, `chrono`, `uuid` added. `cargo tree | grep -E "tch|candle|onnxruntime"` must remain empty.

### Performance budget gates

The cold-boot WAL replay budget (≥ 3× Python baseline) is a constitution invariant. This issue targets structural correctness, not throughput. A benchmark stub (`benches/wal_replay_bench.rs`) is included so the CI compiles it, but no timing assertion is added. The `[HOT]` tag is not applied to any task in this issue; the bench stub exists to enable future `[HOT]` work.

### Workflow gates

- Spec exists at issue body (no standalone spec.md for this issue — referenced from `specs/001-rust-knowledge-graph/spec.md` User Story 2) ✓
- No IPC-touching changes ✓
- `[WAL]`-tagged tasks have TDD requirement: test written and failing before implementation commit ✓
- No constitution deviations; no new ADR needed ✓

## Project Structure

```text
Cargo.toml                                   # workspace: add serde, serde_json, chrono, uuid
liminis-graph-core/
├── Cargo.toml                               # opt in to serde, serde_json, chrono, uuid
└── src/
    ├── lib.rs                               # add pub mod wal; pub mod replay; re-exports
    ├── error.rs                             # add WalIo(std::io::Error), WalJson(String) variants
    ├── db.rs                                # add Db::open_or_rebuild()
    ├── wal.rs                               # WalLine, WalWriter (NEW)
    └── replay.rs                            # WalReplayer, ReplayStats (NEW)
liminis-graph-core/tests/
├── wal_serialization.rs                     # [WAL] unit: WalLine serde round-trip, field order
├── wal_appender.rs                          # [WAL] unit: WalWriter chunk-atomicity, rotation, seq
├── wal_replay.rs                            # [WAL] unit: replayer ordering, skip truncated, skip unknown
└── wal_compat.rs                            # [WAL] golden fixture: python_produced.jsonl round-trip
liminis-graph-core/tests/fixtures/wal/
├── python_produced.jsonl                    # 5-line synthetic Python-format fixture (dim=4)
└── rust_produced_expected.jsonl             # expected Rust serialization output (1 line)
liminis-graph-core/benches/
└── wal_replay_bench.rs                      # stub bench (compile-only; no timing assertion)
```

## Public Library API

```rust
// wal.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalLine {
    pub seq:    u64,
    pub ts:     String,        // ISO-8601 UTC, e.g. "2026-05-18T12:00:00.123456+00:00"
    pub db:     String,
    pub cypher: String,
    pub params: serde_json::Value,
}

pub struct WalWriter {
    // private fields: wal_dir, global_seq, file_seq, events_in_file,
    //                 max_events_per_file, session_id, pending_lines, in_chunk
}

impl WalWriter {
    /// Opens (or creates) the WAL directory and scans existing files for max seq.
    pub fn new(wal_dir: impl Into<PathBuf>, max_events_per_file: usize) -> Result<Self, Error>;

    /// Buffers a mutation; must be called inside with_chunk.
    pub fn log_mutation(
        &mut self,
        cypher: &str,
        params: serde_json::Value,
        database: &str,
    ) -> Result<(), Error>;

    /// Chunk-atomic write: runs f; on Ok flushes buffer to one file; on Err discards.
    pub fn with_chunk<F, T>(&mut self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut WalWriter) -> Result<T, Error>;
}

// replay.rs
pub struct ReplayStats {
    pub lines_replayed: u64,
    pub lines_skipped:  u64,
    pub files_read:     u64,
}

pub struct WalReplayer {
    wal_dir: PathBuf,
}

impl WalReplayer {
    pub fn new(wal_dir: impl Into<PathBuf>) -> Self;
    pub fn replay(&self, conn: &Conn) -> Result<ReplayStats, Error>;
}

// db.rs (addition)
impl Db {
    /// If db_path absent but wal_dir has .jsonl files, rebuilds from WAL.
    pub fn open_or_rebuild(
        db_path: &str,
        wal_dir: &str,
        embedding_dim: usize,
    ) -> Result<Self, Error>;
}
```

## WalLine Serialization Contract

```json
{"seq":1,"ts":"2026-05-18T12:00:00.123456+00:00","db":"graphiti","cypher":"MERGE (n:Entity {uuid: $uuid}) SET n += $props","params":{"uuid":"..."}}
```

Field order: `seq` → `ts` → `db` → `cypher` → `params`. Values: `seq` integer, `ts` string (ISO-8601 UTC), `db` string, `cypher` string, `params` arbitrary JSON object (datetime values as ISO-8601 strings). Embeddings are JSON arrays of floats.

## File Rotation & Naming

Filename format: `{YYYYMMDD}_{HHMMSS}_{session_id_6}_{file_seq:04d}.jsonl`

Example: `20260519_120000_a1b2c3_0000.jsonl`

- `session_id_6`: first 6 hex chars of a `uuid::Uuid::new_v4()` generated at `WalWriter::new()`.
- `file_seq`: starts at the highest existing file_seq + 1 across all session files (or 0 if none).
- Rotation: when `events_in_current_file + chunk_lines > max_events_per_file`, rotate before writing.

## Replay Algorithm

1. Collect all `*.jsonl` files in `wal_dir` (non-recursive).
2. Sort lexicographically by filename (ISO-8601 timestamp prefix ensures chronological order per R-07).
3. For each file, read lines one by one:
   a. If the line is empty (blank): skip.
   b. Try `serde_json::from_str::<WalLine>(&line)`.
   c. On parse error: if it is the last line of the file, emit a structured warning and skip (R-05); otherwise emit warning and skip.
   d. On success: classify the Cypher first token.
   e. If classification is unknown: `eprintln!("[WAL WARN] skipping unknown op: {first_token}")`, skip (R-08).
   f. Execute via `conn.raw_query(cypher, params)`.
4. Return `ReplayStats`.

## Complexity Tracking

No constitution violations. No complexity entries needed.
