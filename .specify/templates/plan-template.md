# Implementation Plan: [FEATURE]

**Branch**: `[###-feature-name]` | **Date**: [DATE] | **Spec**: [link]
**Input**: Feature specification from `/specs/[###-feature-name]/spec.md`

**Note**: This template is filled in by the `/speckit-plan` command. See `.specify/templates/plan-template.md` for the execution workflow.

## Summary

[Extract from feature spec: primary requirement + technical approach from research]

## Technical Context

<!--
  ACTION REQUIRED: Replace the content in this section with the technical details
  for the project. The structure here is presented in advisory capacity to guide
  the iteration process.
-->

**Language/Version**: [e.g., Python 3.11, Swift 5.9, Rust 1.75 or NEEDS CLARIFICATION]  
**Primary Dependencies**: [e.g., FastAPI, UIKit, LLVM or NEEDS CLARIFICATION]  
**Storage**: [if applicable, e.g., PostgreSQL, CoreData, files or N/A]  
**Testing**: [e.g., pytest, XCTest, cargo test or NEEDS CLARIFICATION]  
**Target Platform**: [e.g., Linux server, iOS 15+, WASM or NEEDS CLARIFICATION]
**Project Type**: [e.g., library/cli/web-service/mobile-app/compiler/desktop-app or NEEDS CLARIFICATION]  
**Performance Goals**: [domain-specific, e.g., 1000 req/s, 10k lines/sec, 60 fps or NEEDS CLARIFICATION]  
**Constraints**: [domain-specific, e.g., <200ms p95, <100MB memory, offline-capable or NEEDS CLARIFICATION]  
**Scale/Scope**: [domain-specific, e.g., 10k users, 1M LOC, 50 screens or NEEDS CLARIFICATION]

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

Reference: `.specify/memory/constitution.md` (v1.0.0). For each gate, mark PASS / FAIL / N/A and justify any FAIL in the Complexity Tracking section below.

### Principle gates

- **I. IPC Parity During Migration** — Does this plan change the Unix-socket IPC surface? If yes, is byte-compatibility with the current Python service preserved, and are parity tests against the recorded request/response corpus included?
- **II. Library and Binary Are Peers** — Is every IPC-exposed capability also reachable via the library API? Any binary-only behavior introduced?
- **III. LadybugDB Only** — Does any artifact reintroduce a driver abstraction or non-LadybugDB store? (If yes, this requires a constitution amendment, not a plan.)
- **IV. WAL Is Authoritative** — Does every mutation append to WAL before DB commit? Is the WAL format change (if any) backward/forward compatible at the patch level? Format breaks require MAJOR bump.
- **V. LLM and Embedding Adapters Stay Out-of-Process** — Does this plan pull any ML runtime into the Rust binary? (If yes, this requires a constitution amendment.)

### Performance budget gates

For any plan touching the search, dedup, or replay hot paths, declare which budgets apply and where the benches live:

- p95 search latency ≤ 500 ms under concurrent extract load
- Dedup wall time ≤ 30% of Python brute-force baseline at 50k entities (≥ 95% decision overlap)
- Steady-state memory ≤ 60% of Python service at 100k nodes
- Cold-boot WAL replay ≥ 3× Python baseline

If a budget cannot be met, document the regression and the rollback plan in Complexity Tracking before proceeding.

### Workflow gates

- Spec exists at `specs/<NNN>-<slug>/spec.md`? (If no, stop and run `/speckit-specify` first.)
- For IPC-touching changes: parity tests planned?
- For hot-path changes: benchmarks planned in `benches/`?
- For WAL-replay or IPC-serialization code: tests written before implementation (TDD mandatory per Workflow section)?
- For any deviation from constitution: ADR planned in `docs/adr/`?

## Project Structure

### Documentation (this feature)

```text
specs/[###-feature]/
├── plan.md              # This file (/speckit-plan command output)
├── research.md          # Phase 0 output (/speckit-plan command)
├── data-model.md        # Phase 1 output (/speckit-plan command)
├── quickstart.md        # Phase 1 output (/speckit-plan command)
├── contracts/           # Phase 1 output (/speckit-plan command)
└── tasks.md             # Phase 2 output (/speckit-tasks command - NOT created by /speckit-plan)
```

### Source Code (repository root)
<!--
  ACTION REQUIRED: Replace the placeholder tree below with the concrete layout
  for this feature. Delete unused options and expand the chosen structure with
  real paths (e.g., apps/admin, packages/something). The delivered plan must
  not include Option labels.
-->

```text
# [REMOVE IF UNUSED] Option 1: Single project (DEFAULT)
src/
├── models/
├── services/
├── cli/
└── lib/

tests/
├── contract/
├── integration/
└── unit/

# [REMOVE IF UNUSED] Option 2: Web application (when "frontend" + "backend" detected)
backend/
├── src/
│   ├── models/
│   ├── services/
│   └── api/
└── tests/

frontend/
├── src/
│   ├── components/
│   ├── pages/
│   └── services/
└── tests/

# [REMOVE IF UNUSED] Option 3: Mobile + API (when "iOS/Android" detected)
api/
└── [same as backend above]

ios/ or android/
└── [platform-specific structure: feature modules, UI flows, platform tests]
```

**Structure Decision**: [Document the selected structure and reference the real
directories captured above]

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| [e.g., 4th project] | [current need] | [why 3 projects insufficient] |
| [e.g., Repository pattern] | [specific problem] | [why direct DB access insufficient] |
