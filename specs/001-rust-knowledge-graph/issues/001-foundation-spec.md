# Issue #1 Spec: Foundation — Cargo Scaffold, CI, LadybugDB Spike, Library+Binary Symmetry

**Issue**: #1  
**Created**: 2026-05-19  
**Status**: Specified  
**Maps to**: User Story 5 (Reusable as a library) — Day-0 structural prerequisite for all other user stories.

---

## Goal

Stand up the repository skeleton so every subsequent issue has somewhere to land. Prove that LadybugDB's Rust bindings are load-bearing before committing the entire project to them, and establish the library+binary workspace layout that the constitution requires.

---

## User Stories

### Story 1 — Developer can clone, build, and verify the repo structure (P1)

A contributor clones the repo for the first time, runs `cargo build --release`, and gets two artefacts: a library crate (`liminis-graph-core`) and a binary (`liminis-graph`). Neither depends on any Liminis-internal code.

**Why P1**: Nothing else can be built without this. All subsequent issues land on top of this skeleton.

**Independent test**: `cargo build --release && cargo test && cargo clippy -- -D warnings && cargo fmt --check` all pass on Linux and macOS.

**Acceptance scenarios**:

1. **Given** a fresh checkout, **When** `cargo build --release` runs, **Then** both crates compile without errors and produce artefacts, with zero Liminis-internal dependencies in the dependency tree.
2. **Given** the workspace, **When** `cargo clippy -- -D warnings` runs, **Then** zero warnings are emitted.
3. **Given** the workspace, **When** `cargo fmt --check` runs, **Then** all files are formatted.

---

### Story 2 — External consumer can ingest and search via the library API (P1)

A developer who knows nothing about Liminis reads the quickstart in `examples/` and runs the example consumer. It ingests three sample documents into a local LadybugDB file and returns search results without any external service running.

**Why P1**: Validates Principle II (Library and Binary Are Peers) and Principle V (no ML-runtime deps). If the example fails at Day 0, the "reusable library" premise is already broken.

**Independent test**: `cargo run --example basic_ingest` succeeds end-to-end on a CI runner with only the LadybugDB native file as state.

**Acceptance scenarios**:

1. **Given** the example consumer, **When** it ingests three sample documents, **Then** the LadybugDB file contains the corresponding nodes and edges.
2. **Given** the ingested data, **When** a search query runs, **Then** results are returned and the example exits with code 0.
3. **Given** the `Cargo.toml`, **When** its dependency list is inspected, **Then** it contains zero ML-runtime dependencies (`tch`, `candle`, `onnxruntime`, etc.).

---

### Story 3 — CI validates the repo on every push (P1)

GitHub Actions runs build, test, clippy, fmt, and a bench-job stub on Linux and macOS for every push and pull request.

**Why P1**: Establishes the green baseline that all future PRs must maintain. The bench-job stub exists so `[HOT]` tasks have a job to extend, not create.

**Independent test**: Push a trivial change; all CI jobs pass in the Actions dashboard.

**Acceptance scenarios**:

1. **Given** a push to any branch, **When** CI runs, **Then** `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` all pass on both Linux and macOS runners.
2. **Given** the CI config, **When** it is inspected, **Then** a bench job exists (may be a stub with no actual benchmarks yet).
3. **Given** the LadybugDB round-trip test in `liminis-graph-core`, **When** CI runs, **Then** the test passes on both Linux and macOS.

---

### Story 4 — LadybugDB binding spike is documented and pinned (P2)

The LadybugDB Rust crate version is pinned in `Cargo.toml` and a passing integration test proves the binding can: open a file, create Entity/Episodic nodes with a vector property, run a Cypher query to read them back, and round-trip a HNSW + full-text index.

**Why P2**: The binding quality decision must be made before any real feature work; Research stage owns finding the right crate and version.

**Independent test**: `cargo test --test ladybug_spike` passes on Linux and macOS in CI.

**Acceptance scenarios**:

1. **Given** the pinned LadybugDB crate version in `Cargo.toml`, **When** the spike test runs, **Then** a file-backed graph opens, Entity and Episodic nodes with vector properties are written and read back, and a HNSW + full-text index round-trips successfully.
2. **Given** the spike test, **When** it is inspected, **Then** no test secret or environment variable beyond a temp directory path is required.

---

## Scope

### In scope

- `Cargo.toml` workspace root declaring two members: `crates/liminis-graph-core` (lib) and `crates/liminis-graph` (bin).
- `crates/liminis-graph` depends on `crates/liminis-graph-core`; `crates/liminis-graph-core` depends on no Liminis-internal code.
- LadybugDB Rust crate pinned to a specific version (Research stage determines which crate and version).
- `examples/basic_ingest.rs` — fewer than 50 lines, no external services required, ingests three hardcoded sample documents and runs a search.
- GitHub Actions workflow: `build-test` job (cargo build + test + clippy + fmt, Linux + macOS matrix) and `bench` job (stub, runs `cargo bench --no-run` so it can be extended later).
- `benches/` directory with a placeholder bench file (e.g., `benches/placeholder.rs`) that compiles but contains no real benches.
- `docs/adr/0001-record-architecture-decisions.md` — the meta-ADR explaining the ADR practice.
- `.github/` — issue templates (bug, feature) and a pull request template that references the constitution and asks for principle compliance.
- Constitution gates validated: Principle II (library API is the source of truth), Principle III (LadybugDB pinned, no abstraction), Principle V (zero ML-runtime deps).

### Out of scope

- IPC layer / Unix-socket server (US1).
- WAL serialization (US2).
- LLM or embedding adapters (US3+).
- Any performance-critical hot paths — `[HOT]` bench tasks come in subsequent issues.

---

## Requirements

### Functional Requirements

- **FR-001**: Workspace MUST contain at least a `liminis-graph-core` library crate and a `liminis-graph` binary crate; binary depends on library; library has no Liminis-internal dependencies.
- **FR-002**: LadybugDB Rust crate version MUST be pinned (exact version) in the workspace `Cargo.toml`.
- **FR-003**: A passing integration test MUST demonstrate: open a LadybugDB file, create Entity and Episodic nodes each with a vector property, run a Cypher read-back query, and verify HNSW + full-text index creation and query round-trip.
- **FR-004**: `examples/basic_ingest.rs` MUST compile and run successfully, ingesting three documents and returning search results, in under 50 lines, requiring no setup beyond the documented quickstart.
- **FR-005**: GitHub Actions CI MUST run `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` on both Linux and macOS runners.
- **FR-006**: A bench CI job MUST exist and pass (stub acceptable; must compile `benches/`).
- **FR-007**: `benches/` directory MUST exist with at least one placeholder bench file that compiles.
- **FR-008**: `docs/adr/0001-record-architecture-decisions.md` MUST exist following the MADR or similar format.
- **FR-009**: `.github/` MUST contain issue templates (bug report, feature request) and a pull request template referencing the constitution.
- **FR-010**: `Cargo.toml` MUST declare zero ML-runtime dependencies at any level of the workspace dependency tree.

### Key Entities

- **`liminis-graph-core`** (library crate): Exposes the public Rust API; all IPC-visible capabilities must be reachable here.
- **`liminis-graph`** (binary crate): Thin wrapper; depends on `liminis-graph-core` for all behaviour.
- **LadybugDB file**: A file-backed graph database; opened and written by the integration test and the example consumer.
- **ADR**: A structured decision record in `docs/adr/`; the meta-ADR (0001) describes the practice itself.

---

## Success Criteria

- **SC-001**: `cargo build --release`, `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` all pass locally and in CI on Linux and macOS with zero errors.
- **SC-002**: The LadybugDB round-trip integration test passes on both CI platforms with no manual setup.
- **SC-003**: The `examples/basic_ingest` example runs end-to-end in CI (no manual steps beyond checkout and `cargo run`).
- **SC-004**: Zero ML-runtime crates (`tch`, `candle`, `onnxruntime`, etc.) appear anywhere in `cargo tree` output for the workspace.
- **SC-005**: All three constitution gates (Principles II, III, V) are demonstrably satisfied by the artefacts committed.

---

## Assumptions

- The LadybugDB Rust crate name and exact version are determined by the Research stage; the spike test and Cargo.toml version are filled in during planning/implementation once Research confirms the binding quality.
- "LadybugDB" refers to the graph engine currently accessed via the Python `ladybugdriver.py`; the Research stage will confirm the exact Rust crate that exposes compatible APIs (including HNSW and full-text index support).
- The example consumer creates a temporary LadybugDB file (in a `tempfile` dir) and cleans up after itself; no persistent local state is required.
- Both Linux and macOS CI runners have the OS-level dependencies LadybugDB's Rust bindings need (Research stage identifies these and documents them in the quickstart).
- The bench job stub uses `cargo bench --no-run` or an equivalent that compiles without requiring hardware-specific tuning.
- GitHub Actions free tier (ubuntu-latest, macos-latest) is the target runner environment.

---

## Out of Scope

- IPC layer (US1), WAL serialization (US2), LLM/embedding adapters (US3+).
- Any ADR beyond the meta-ADR (0001); subsequent ADRs are written as their decisions arise.
- Hosted CI (beyond GitHub Actions), code-coverage gates, or release automation.
