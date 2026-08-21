# Feature Specification: Should the engine auto-hydrate a group whose WAL is ahead of its database?

**Feature Branch**: `fabrik/issue-463`
**Created**: 2026-08-21
**Status**: Draft
**Input**: User description: "When a group's database content is empty but its WAL directory holds unapplied entries, lcg does not replay it — the graph is populated only by an explicit `knowledge_rebuild_from_wal`. This issue asks whether, and under what policy, the engine should hydrate itself."

## Background

On startup with an empty database beside populated per-group WAL directories, `lcg-service` comes up
`healthy` with an empty graph and treats the empty DB as authoritative. `applied_seq` is derived from
DB contents (`backfill_applied_seq_if_absent`, `recovery.rs:170`), not from the WAL, so an empty
group records `applied_seq = 0` without the WAL contradicting it.

This is the policy half of #455, split from the observability half (#456, shipped in 0.13.3). #456
made the condition **visible** via `knowledge_status`'s `hydration_status` field (`wal_ahead`); it
deliberately did not decide what the engine should *do* about it. This issue is that decision.

ADR-0027's autonomous recovery does not apply: `is_recoverable` matches only corrupt-WAL and
permission/missing-file conditions, never a merely-empty database.

## Why This Is a Decision, Not an Implementation Task

This issue's first requirement is a decision, not an implementation: **should the engine auto-hydrate
at all**, or is `hydration_status` (#456) sufficient — leaving the decision with the operator or
consumer, which is what orac does today, by design?

Both answers are legitimate and lead to very different amounts of work:

- **"No, `hydration_status` is sufficient"** closes this issue with a documented rationale and ships
  immediately.
- **"Yes, auto-hydrate"** requires settling opt-in vs. default, the trigger condition (including the
  0-indexed `max_seq` limitation), boot-time failure handling with no operator present, and a
  migration path for consumers already driving rebuild explicitly — all against constraints C-001
  through C-004 below.

Specify's job is to reach this fork explicitly — with analysis and a recommendation — and then stop,
not to resolve it by picking whichever direction is easier to build. A human reads the fork, decides,
and only then applies the label for the next stage. `fabrik:cruise` was deliberately removed from
this issue for exactly this reason: under cruise, Specify would pick an answer and Implement would
build it, and the first human to look at the result would be reviewing an implementation of a
decision nobody made. That happened on #428, where a full Implement cycle was discarded because the
spec had resolved an open question in a direction that turned out to be wrong.

Why auto-replay is not obviously correct — three existing behaviours constrain it:

1. **Consumer-driven rebuild and reset detection (#387).** Downstream consumers (orac) drive
   `knowledge_rebuild_from_wal` explicitly and layer generation-based reset detection on top of it.
   An engine that replays on its own can bypass that logic, applying a stream the consumer had not
   decided to apply.
2. **#414's unknown-generation refusal.** A group whose generation cannot be verified refuses to
   replay. An auto-hydrate would route through that guard, so a workspace whose stream was published
   with a `*.jsonl` glob — dropping `.wal-generation.json` — would fail **at boot** rather than at an
   explicit rebuild the operator chose to run. Startup is the worst place to discover that.
3. **#462 (open).** `force_clear` currently clears groups the replay cannot restore. Any auto-replay
   path must not reach that behaviour unattended on every boot; this issue must not land before #462
   is fixed.

The read-only consumer model matters too: a consumer treats hydrated group graphs as read-only, and
only a stream's owner replays its own WAL. Whether "hydrate my own empty DB from my own WAL" is an
owner action or a consumer action needs stating, because it determines whether auto-hydration is even
coherent for a consumer.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Maintainer decides whether to add auto-hydration (Priority: P1)

As the maintainer, I need the auto-hydrate fork's tradeoffs and constraints laid out clearly, with a
recommendation, so I can make (and record) a go/no-go decision before any implementation work starts.

**Why this priority**: Blocks everything downstream — no Research or Plan stage can proceed
correctly until this decision is made, since the two branches (no-op close vs. multi-part feature)
have essentially no shared implementation work.

**Independent Test**: A maintainer can read this spec end-to-end and state "yes" or "no" and why,
without needing to read the underlying Rust source.

**Acceptance Scenarios**:

1. **Given** the fork analysis in this spec, **When** a maintainer reads it, **Then** they can
   identify both resolution paths, their scope, and the constraints each must satisfy.
2. **Given** a "no" decision, **When** it is recorded, **Then** the issue closes referencing
   `hydration_status` (#456) as sufficient, with no code changes required.
3. **Given** a "yes" decision, **When** it is recorded, **Then** this issue's milestone is
   re-evaluated (a policy change with four constraints is a different kind of dependency than the
   single remaining bug fix, #462, that 0.14.0 otherwise needs), and the Research stage proceeds
   against the conditional requirements below.

### User Story 2 - Opt-in lazy auto-hydration for standalone deployments (Priority: P2, contingent on a "yes" decision)

As an operator of a standalone `lcg-service` deployment with no orchestrating consumer, I want to
opt into auto-hydration so that a group whose WAL is ahead of its (empty) database gets populated
without me having to call `knowledge_rebuild_from_wal` by hand.

**Why this priority**: This story only exists if User Story 1 resolves "yes." It is not authorized
work until that decision is made.

**Independent Test**: With auto-hydration opted in, accessing or ingesting into a `wal_ahead`, empty
group triggers a successful replay without an explicit `knowledge_rebuild_from_wal` call; with it not
opted in, behavior is unchanged from today.

**Acceptance Scenarios**:

1. **Given** auto-hydration is not opted in, **When** the engine starts or a group is accessed,
   **Then** behavior is byte-for-byte identical to today (C-004) — no consumer that drives rebuild
   explicitly observes any change.
2. **Given** auto-hydration is opted in and a group is empty with `hydration_status: wal_ahead`,
   **When** that group is first accessed or ingested into (not at boot), **Then** the engine replays
   the WAL into the empty database without needing `force_clear`.
3. **Given** auto-hydration is opted in and the group's generation cannot be verified, **When** the
   lazy trigger fires, **Then** the engine refuses per #414's existing guard and surfaces an
   actionable error, rather than silently skipping or crash-looping.
4. **Given** auto-hydration is opted in and a `wal_ahead` group is only partially replayed (not
   empty), **When** the group is accessed, **Then** auto-hydration does not trigger for it (empty-DB
   only).

### Edge Cases

- Single-entry WAL: `max_seq` is 0-indexed, so a WAL with exactly one entry reports `Some(0)`, which a
  naive `max_seq > 0` check treats as falsy — the trigger condition must account for this if "yes" is
  chosen (documented limitation of `hydration_status`).
- A workspace whose stream was published via a `*.jsonl` glob (no `.wal-generation.json`) has unknown
  generation; under "yes," this must fail the same way #414 already requires, not bypass it.
- A `wal_ahead` group that is only partially replayed (not empty) is out of scope for the recommended
  "yes" design (empty-DB-only, see Conditional Requirements).
- A supervisor restarts the service in response to an `unhealthy` status; this must not be treated as
  a hydration opportunity, since restarting hydrates nothing and would only produce a crash-loop
  (C-001).

## Requirements *(mandatory)*

### Functional Requirements

These apply regardless of which way the central decision resolves:

- **FR-001**: The engine's `healthy` status computation MUST NOT change as a result of this issue
  (C-001) — a supervisor's response to `unhealthy` is to restart, which hydrates nothing and would
  produce a crash-loop.
- **FR-002**: No auto-hydrate path introduced by this issue may bypass #414's unknown-generation
  refusal (C-002).
- **FR-003**: No auto-replay path introduced by this issue may ship before #462 lands, so that no
  such path can reach the un-restorable `force_clear` behaviour unattended (C-003); enforced via a
  `blockedBy #462` relationship on the project board.
- **FR-004**: If auto-hydration is adopted, every consumer that drives `knowledge_rebuild_from_wal`
  explicitly today (e.g., orac/tarial) MUST retain a way to keep that exact behavior unchanged
  (C-004).

The following apply only if the central decision (see Open Questions) resolves "yes":

- **FR-005**: Auto-hydration MUST be opt-in, default OFF. A default-on behavior would race and
  bypass the consumer-driven rebuild and generation-based reset detection in #387.
- **FR-006**: The trigger MUST be lazy — firing on first access or ingest of a `wal_ahead` group —
  and MUST NOT run at boot. Boot-time auto-hydrate would hit #414's unknown-generation refusal with
  no operator present, and for lazy-seeded consumers the WAL may not even be on disk yet at boot.
- **FR-007**: Auto-hydration MUST be restricted to a genuinely empty group (not a partially-replayed
  `wal_ahead` group), so the replay never needs `force_clear` — this decouples the auto-hydrate path
  from #462's `force_clear` behaviour rather than depending on its resolution beyond C-003.
- **FR-008**: The trigger condition MUST correctly account for `max_seq`'s 0-indexing (a single-entry
  WAL reports `Some(0)`, not falsy) rather than a naive `applied_seq == 0 && max_seq > 0` check.
- **FR-009**: An auto-hydration failure (generation refusal, corrupt line, partial replay) MUST
  surface as an actionable error at the point it is triggered, since no operator is present to
  interpret a boot-time failure.
- **FR-010**: The consumer continues to own the git-seed step (WAL ← stream); auto-hydrate only ever
  covers DB ← WAL, never stream ingestion.

### Key Entities

- **Group**: a WAL-scoped unit of graph content with its own `applied_seq`, generation marker, and
  hydration status.
- **hydration_status**: the `knowledge_status` field (from #456) reporting `wal_ahead` when
  `applied_seq` trails `max_seq`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A maintainer can read this spec and record a "yes" or "no" decision on auto-hydration
  without needing to read the underlying Rust source.
- **SC-002**: If "no": the issue closes with the rationale documented, and no code changes ship.
- **SC-003**: If "yes": the eventual implementation satisfies every requirement in the conditional
  requirements section (FR-005 through FR-010), and none of C-001 through C-004 is violated.

## Assumptions

- `hydration_status` (#456, shipped 0.13.3) is the detection mechanism this issue builds on; it is
  not being changed here.
- tarial/orac's wiped-volume recovery does not require this issue — it is fully served by #456 +
  #462 today (`hydration_status` to detect, explicit per-channel `knowledge_rebuild_from_wal` to
  act). This issue would be a convenience for that consumer (skipping the explicit per-channel
  rebuild call), not a correctness requirement.
- No milestone deadline currently forces this decision; nothing is broken while it waits — #456
  already removed the silent-data-loss footgun that originally motivated #455.

## Out of Scope

- Any change to `force_clear` behavior itself (tracked in #462).
- Any change to #414's unknown-generation refusal logic.
- Auto-hydration for partially-replayed (non-empty) `wal_ahead` groups.

## Open Questions

- [ ] **Should the engine auto-hydrate at all**, or is `hydration_status` (#456) sufficient, leaving
      the decision with the operator/consumer (today's orac/tarial behavior)? This is the central
      fork; both "yes" and "no" are legitimate resolutions per the analysis above. This issue does
      not proceed past Specify — no `fabrik:cruise` or next-stage label should be applied — until a
      maintainer decides.

## Source References

- #455 — the original report, with the three options as first framed.
- #456 — the observability half, shipped in 0.13.3.
- #387, #414, #462, ADR-0027, ADR-0009.
