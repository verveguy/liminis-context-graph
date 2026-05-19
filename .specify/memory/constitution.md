<!--
Sync Impact Report
Version change: (initial) → 1.0.0
Modified principles: N/A (initial ratification)
Added sections:
  - Core Principles (I–V)
  - Performance & Resource Budgets
  - Development Workflow
  - Governance
Removed sections: N/A
Templates requiring updates:
  - .specify/templates/plan-template.md ✓ updated 2026-05-18 — Constitution Check section now enumerates principle, performance, and workflow gates
  - .specify/templates/spec-template.md ✓ no change required (spec stays HOW-free per Principle in Development Workflow)
  - .specify/templates/tasks-template.md ✓ updated 2026-05-18 — task format now supports [IPC], [WAL], [HOT], [LDB], [ADAPTER] constitution tags with per-tag gates
Follow-up TODOs: none
Source: specs/001-rust-knowledge-graph/spec.md
-->
# Liminis Graph Constitution

## Core Principles

### I. IPC Parity During Migration (NON-NEGOTIABLE)

While the Python `graphiti_service.py` is still in production in any liminis-framework release, every change to the Unix-socket IPC surface MUST preserve byte-compatibility with the Python service's request/response shapes. Parity tests against a recorded request/response corpus gate merges. The "Python is the oracle" period ends only by explicit constitution amendment.

**Rationale**: The repo's reason to exist is a staged, reversible migration. Drifting the IPC surface mid-migration removes the ability to A/B against a live oracle and turns rollback into a data-migration problem.

### II. Library and Binary Are Peers

The crate's library API is the source of truth; the binary is a thin wrapper around it. Every IPC-exposed capability MUST also be reachable via the library API. No feature lives only in the binary path. External consumers can embed the library without touching the IPC layer.

**Rationale**: The "reusable by other projects" decision is load-bearing — if the binary accretes private behavior, the library degrades into a Liminis-only artifact.

### III. LadybugDB Only

The graph store is a load-bearing dependency, not an interchangeable backend. There is no driver abstraction. Code MAY use LadybugDB-specific Cypher dialect, schema features, and index APIs directly. If a different store is wanted, fork.

**Rationale**: graphiti's driver abstraction across FalkorDB/Neo4j/Kuzu/Ladybug is the single largest source of complexity in the codebase we are replacing, and we have never shipped a non-Ladybug deployment.

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

Benchmarks for these budgets MUST live in `benches/` and run in CI.

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

**Version**: 1.0.0 | **Ratified**: 2026-05-18 | **Last Amended**: 2026-05-18
