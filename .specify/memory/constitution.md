<!--
Sync Impact Report
Version change: 1.0.0 → 2.0.0
Modified principles:
  - I. "IPC Parity During Migration" → "IPC Surface Is a Stable Contract" (MAJOR: redefines a
    NON-NEGOTIABLE principle). The migration the original principle governed is complete; the
    engine now ships standalone as an open-source, local-first product. What survives is the
    obligation the migration created: the wire protocol is a public contract, and the recorded
    request/response corpus remains its executable regression gate.
Added sections: none
Removed sections: none
Templates requiring updates:
  - .specify/templates/plan-template.md ✓ updated 2026-07-13 — Constitution Check gate I reworded to contract-stability framing
  - .github/PULL_REQUEST_TEMPLATE.md ✓ updated 2026-07-13 — principle row I relabeled
  - .specify/templates/tasks-template.md ✓ no change required ([IPC] tag semantics unchanged)
Follow-up TODOs: none
Source: specs/001-rust-knowledge-graph/spec.md (initial); amended 2026-07-13
-->
# Liminis Context Graph Constitution

## Core Principles

### I. IPC Surface Is a Stable Contract (NON-NEGOTIABLE)

The newline-delimited JSON-RPC 2.0 surface over the Unix socket is the engine's public API, consumed by downstream applications in any language. Changes MUST be backward compatible: adding methods and adding optional fields is fine; renaming a method, removing a field, or changing the shape of an existing request/response pair requires a MAJOR version bump and a documented migration. The recorded request/response corpus (`crates/core/tests/ipc_parity.rs` + `crates/core/tests/fixtures/ipc_corpus/`) is the executable definition of the wire contract and gates merges for any change touching the IPC surface.

**Rationale**: Clients drive the engine over the socket from arbitrary languages and upgrade on their own cadence. Silent shape drift breaks all of them at once, and unlike a library API there is no compiler to catch it — only the corpus tests make the contract enforceable rather than aspirational.

### II. Library and Binary Are Peers

The crate's library API is the source of truth; the binary is a thin wrapper around it. Every IPC-exposed capability MUST also be reachable via the library API. No feature lives only in the binary path. External consumers can embed the library without touching the IPC layer.

**Rationale**: The "reusable by other projects" decision is load-bearing — if the binary accretes private behavior, the library degrades into a Liminis-only artifact.

### III. LadybugDB Only

The graph store is a load-bearing dependency, not an interchangeable backend. There is no driver abstraction. Code MAY use LadybugDB-specific Cypher dialect, schema features, and index APIs directly. If a different store is wanted, fork.

**Rationale**: The upstream graphiti-core library's driver abstraction across FalkorDB/Neo4j/Kuzu/Ladybug is the single largest source of complexity in the codebase we are replacing, and we have never shipped a non-Ladybug deployment.

### IV. WAL Is Authoritative

Every mutation MUST append to the WAL before the DB write commits. The DB file MUST be rebuildable from WAL. WAL format stays JSONL and remains forward/backward compatible across patch versions; format breaks require a MAJOR version bump and a documented migration.

**Rationale**: WAL files are checked into user workspaces. Breaking the format breaks every shipped workspace. Treating the DB as a cache and WAL as truth is what makes that survivable.

### V. LLM and Embedding Adapters Stay Out-of-Process

The Rust binary MUST contain no ML runtime. Extraction, dedup, and embedding reach their models via HTTP or subprocess adapters. Adding an in-process model to the core crate requires a constitution amendment.

**Rationale**: Pulling MLX or sentence-transformers into the Rust build chain destroys portability, increases binary size by orders of magnitude, and couples the release cadence to Python ML wheels.

## Performance & Resource Budgets

The following are constitution-level invariants, not aspirational targets. Regressions against the Python baseline on the same workspace are merge-blockers:

- p95 search latency ≤ 500 ms while ≥ 100 episodes are extracting concurrently
- Dedup wall time on a 50k-entity workspace ≤ 30% of Python brute-force baseline, with ≥ 95% decision overlap
- Steady-state memory on a 100k-node workspace ≤ 60% of the Python service's footprint
- Cold-boot WAL replay throughput ≥ 3× the Python baseline

Benchmarks for these budgets MUST live in `crates/core/benches/` and run in CI.

## Development Workflow

- **Spec-kit drives all features.** No code lands without a corresponding `specs/<NNN>-<slug>/spec.md`. The Plan stage owns implementation decisions; the Spec stage stays HOW-free.
- **Parity tests required** for any change touching the IPC surface or the LadybugDB driver layer.
- **Benchmarks required** for any change touching search, dedup, or replay hot paths.
- **TDD is encouraged, not mandatory** for non-hot-path code; mandatory for WAL replay logic and IPC serialization.
- **ADRs required** for any deviation from the principles above, in `docs/adr/`.

## Governance

This constitution supersedes other conventions. Amendments require a documented rationale in the PR description, a version bump per the rules below, and propagation to dependent templates (plan, tasks, checklist) when the change affects them.

- **MAJOR**: Removing or redefining a NON-NEGOTIABLE principle; WAL format break.
- **MINOR**: New principle, materially expanded guidance, or new budget.
- **PATCH**: Wording, typos, non-semantic clarifications.

PRs touching IPC, WAL format, the LadybugDB driver layer, or any performance budget MUST link to the relevant constitution section and confirm compliance.

**Version**: 2.0.0 | **Ratified**: 2026-05-18 | **Last Amended**: 2026-07-13
