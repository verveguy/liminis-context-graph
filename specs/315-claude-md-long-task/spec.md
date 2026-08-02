# Feature Specification: CLAUDE.md's long-task guidance names the wrong failure and misses the safe technique

**Feature Branch**: `fabrik/issue-315`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "CLAUDE.md's long-task guidance names the wrong failure and misses the safe technique — see issue #315 body for full background."

## Background

`CLAUDE.md`'s "Rust pre-commit checks" section carries the repo's warning about long-running commands:

> do not run `cargo test --release` locally, and never end a turn waiting on a backgrounded command

It names the right consequence and the right blast radius (`#208`, `#190`, `#219`, `#212`, `#236`, and since then `#283` and `#297`). But measured against what stage workers actually did, it is aimed imprecisely and omits the one technique that would have saved most of the failures.

Counting real `ScheduleWakeup` invocations in `.fabrik/logs/`: **75 across this repo's stage logs**, 65 with parseable reasons —

| waiting on | count |
|---|---:|
| local `cargo test --release` / clippy / test suites | ~33 |
| GitHub Actions runs | 17 |
| benchmark workflow | 15 |

Three gaps:

1. **~32 of the waits are on CI or bench runs, which this section never mentions.** A Review-stage worker waiting on a `real-corpus-e2e` run does not see itself in a paragraph about `cargo test --release`. The rule it needs — *never wait on a CI run, the Fabrik engine gates on CI via `fabrik:awaiting-ci`* — is absent.
2. **The safe technique is missing.** The fatal step is *ending the turn*, not backgrounding. A worker may background a long task and poll within the same turn using bounded foreground waits under the 10-minute cap. Most of those ~33 local waits could have survived that way. The current text implies backgrounding itself is the error.
3. **Guidance alone has not worked.** The section names `cargo test --release` explicitly and it was run-and-awaited 23+ times regardless. That is context for why the wording should be sharper and placed where a worker in a *Review* or *Validate* stage will read it, not only alongside pre-commit instructions.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A worker facing an 18-minute check knows what to do (Priority: P1)

A Fabrik stage worker (in any stage — Implement, Review, or Validate) hits a point where it needs a signal from a command that will exceed the ~10-minute foreground call cap: a release-mode test suite, a CI run, or a benchmark workflow. Today, CLAUDE.md's only guidance on this lives in the pre-commit section, is scoped to `cargo test --release` by name, and doesn't tell the worker what to do instead — only what not to do. The worker needs an explicit, ordered set of options that covers CI/bench waits as well as local long-running commands, and that makes clear backgrounding-and-polling-in-turn is safe while ending the turn to await a notification is not.

**Why this priority**: This is the entire content of the issue — one section of guidance, read by every stage worker, that has repeatedly failed to prevent the exact failure mode it names (23+ recurrences of the named command despite explicit guidance against it). Fixing it is the only unit of work in scope.

**Independent Test**: Read the revised `CLAUDE.md` cold, without prior conversation context, and confirm: (a) a worker waiting on a GitHub Actions or benchmark run finds an applicable rule without having to infer it from a `cargo`-specific paragraph, (b) the bounded in-turn polling technique is described with a concrete example, (c) no sentence in the file implies that backgrounding itself (as opposed to ending the turn while awaiting it) is the error.

**Acceptance Scenarios**:

1. **Given** `CLAUDE.md`, **When** an agent needs a signal that exceeds the foreground call cap, **Then** it finds an explicit ordered set of options: prefer not to run it (CI owns the release suite); run the in-budget subset; if the result is genuinely required, background and poll within the same turn; never end the turn awaiting a notification.
2. **Given** `CLAUDE.md`, **When** an agent is waiting on a CI or benchmark run, **Then** it finds an explicit rule that stages never wait on CI, and that the engine gates on CI via `fabrik:awaiting-ci`.

---

### Edge Cases

- A worker in the Implement stage (which has no CI-gating relationship, same as Review — only Validate is configured with `wait_for_ci: true`) still needs the local-command guidance (don't run `cargo test --release`, use the debug-mode gate instead) — the revised guidance must not read as exclusively a Review/Validate concern.
- A worker that has already started a long-running command in the background (correct) but is tempted to end its turn to "wait for the notification" (incorrect) needs the distinction between the two to be unambiguous, not just implied by adjacent wording.
- The casualty list is illustrative context, not a target for exhaustive maintenance — the requirement (FR-005) is that the list named at time of writing is accurate, not that every future recurrence gets appended.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The guidance MUST state that a stage never waits on a CI or benchmark run, and that the engine gates on CI via `fabrik:awaiting-ci`.
- **FR-002**: The guidance MUST distinguish *backgrounding* (permitted) from *ending a turn while awaiting a background result* (fatal), and describe bounded in-turn polling as the supported technique when a long result is genuinely required.
- **FR-003**: The guidance MUST present the options in preference order, leading with "don't run it — CI runs this exact command."
- **FR-004**: The guidance MUST be discoverable from a Review/Validate context, not only from the pre-commit section. Cross-reference or relocate as appropriate.
- **FR-005**: The claimed figures (suite duration, call cap, casualty list) MUST be accurate at time of writing, and the casualty list updated to include #283 and #297.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A reader waiting on a GitHub Actions run finds an applicable rule without inferring from a `cargo`-specific paragraph.
- **SC-002**: The bounded in-turn polling technique appears with a concrete example.
- **SC-003**: No guidance in the file implies that backgrounding is itself the error.

## Assumptions

- This is a documentation-only change to `CLAUDE.md` (and, if cross-referenced, other files it points to); no code, tests, or CI configuration change.
- The existing 10-minute foreground call cap, the 15–18 minute release-suite duration, and the `fabrik:awaiting-ci` engine-gating mechanism are accurate as currently documented elsewhere in this repo and in the Fabrik engine; this issue does not re-derive them, only reflects them accurately in the guidance text.
- Where exactly the revised guidance lives (relocated wholesale, split with a cross-reference, or left in place with additions) is left to the Research/Plan stage to decide, per FR-004's "as appropriate."

## Out of Scope

- Reducing the suite's 15–18 minute runtime — the underlying cause, filed separately.
- Fabrik-side tool exposure and stage-prompt changes — filed as handarbeit/fabrik#1345.

## Source References

- handarbeit/fabrik#1345 — the engine-side report, with the measurement this issue draws on
- `CLAUDE.md` — "Rust pre-commit checks" section, the current home of the guidance being revised
