# Feature Specification: WAL Stream Generation Non-Null Guarantee & Reset-Detection Guard

**Feature Branch**: `fabrik/issue-414`
**Created**: 2026-08-15
**Status**: Draft
**Input**: User description: "WAL stream `generation` is null end-to-end — ADR-0387 reset detection is inert. After a clean per-group hydrate, `knowledge_status.wal_groups[*].generation` is `null` for every group. ADR-0387 makes generation identity the sole signal distinguishing a forward WAL advance from a reset/rebuild in both directions. With it null everywhere, that detection is inert."

## Background

ADR-0387 introduced `.wal-generation.json`, a per-group sidecar that gives a WAL stream a
stable identity independent of its `seq` numbering. Its purpose is to let
`knowledge_rebuild_from_wal` tell a genuine producer-side reset (re-extracted corpus,
republished from `seq: 0` with entirely new content and entity identities) apart from
ordinary forward progress — a distinction `applied_seq`/`max_seq` alone cannot make, because a
reset that produces a *longer* stream than before looks identical to normal growth.

In production, this detection is currently **inert**. Two channels hydrated from published WAL
repos (`GES/orac-psetadrs`, 3 files; `adamb1/a2h`, 1 file; lcg `0.13.0`) both report
`generation: null` for every group, and top-level `wal.generation` is also `null`. Both source
repos contain only `*.jsonl` under `wal/` — no `.wal-bounds.json` and no `.wal-generation.json`.

ADR-0387 already decided that `.wal-generation.json` is **producer-authored**: lcg mints one
only inside `WalWriter::new`'s first-content branch (i.e., only for streams *lcg itself
creates*), and explicitly never retroactively mints one into a directory it did not create
(`docs/operations.md`, `wal_generation.rs`). A directory with no `.wal-generation.json` is
treated as "unknown," and unknown-vs-unknown or unknown-vs-known comparisons are defined to
never register as a mismatch (`wal_generation::position_reset_detected`) — a deliberate choice
so that a missing or corrupted sidecar can never *masquerade* as a false-positive reset.

That same "never a mismatch" rule is also why the detection is silently powerless against a
**real** reset when the producer never writes the file at all: orac/tarial (the observed
real-world publisher in this issue's reproduction) does not currently write
`.wal-generation.json` into the WAL repos it publishes, so every group hydrated from those repos
stays permanently in the "unknown" state, and `knowledge_rebuild_from_wal`'s reset check
degrades to always-false, exactly the silent-corruption class ADR-0387 was designed to close.
This issue exists to decide how much of the fix belongs in lcg (a louder signal, and/or lcg
assigning its own identity to streams it did not create) versus in the producer contract
(orac/tarial actually writing the file), and to make `knowledge_status` and
`knowledge_rebuild_from_wal` behave in a well-defined, non-silent way in the meantime.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator can tell "generation known" from "generation unknown" (Priority: P1)

An operator or downstream consumer (e.g. orac #42, which drives its `force_clear` reset off a
generation compare) calls `knowledge_status` against a group that has been fully hydrated from a
published WAL stream. Today, both "this group has never been created" and "this group has
real content but no recorded generation" report identically as `generation: null` — there is no
way to tell a healthy-but-uncompliant stream apart from one that was simply never written to.

**Why this priority**: This is the reproduction's core symptom and the reason ADR-0387's
detection is inert end-to-end — without this distinction, nothing downstream of `knowledge_status`
can reason about generation at all.

**Independent Test**: Hydrate a group from a WAL directory containing `*.jsonl` content but no
`.wal-generation.json`, then call `knowledge_status`. The response must let a caller distinguish
this case from a group with no WAL directory at all.

**Acceptance Scenarios**:

1. **Given** a group WAL directory with no `*.jsonl` content and no `.wal-generation.json`
   (never hydrated), **When** `knowledge_status` is called, **Then** the group's generation is
   reported in a way that is recognizable as "not applicable / no stream yet," not conflated with
   a stream that exists but lacks a generation record.
2. **Given** a group WAL directory with `*.jsonl` content but no `.wal-generation.json` (the
   reproduction's case), **When** `knowledge_status` is called, **Then** the response surfaces
   this as a distinct, identifiable condition rather than indistinguishable `null`.

---

### User Story 2 - Reset detection has defined, non-silent behavior when generation is unknown (Priority: P1)

An operator runs `knowledge_rebuild_from_wal` against a group that already has a recorded
`WalPosition` and whose current on-disk generation is unknown. Today this silently proceeds as
an ordinary incremental replay with no trace that generation-based reset detection did not run —
the operator has no way to know they are exposed to the exact corruption scenario ADR-0387 exists
to prevent.

**Why this priority**: This is the other half of "inert" — even once `knowledge_status` reports
the unknown state (Story 1), nothing today tells an operator that a *specific replay* skipped the
safety check.

**Independent Test**: Run `knowledge_rebuild_from_wal` against a group with a recorded position
and an unknown on-disk generation, and confirm the response and/or logs make the "reset detection
did not run" condition observable.

**Acceptance Scenarios**:

1. **Given** a group with a previously recorded `WalPosition` and an on-disk generation that is
   currently unknown (missing or corrupt `.wal-generation.json`), **When**
   `knowledge_rebuild_from_wal` is called, **Then** the behavior matches whichever option is
   selected in Open Question 2 below (see `## Open Questions`), and is observable in the response
   and/or logs rather than being silent.

---

### User Story 3 - Producer contract for `.wal-generation.json` is unambiguous (Priority: P2)

A team implementing or maintaining a non-lcg WAL publisher (orac/tarial, or any future
distributed-WAL producer) needs a single, canonical, complete description of what to write, in
what format, and when, for `.wal-generation.json` — so that a newly published stream is
generation-compliant from its first write rather than requiring a later lcg-side workaround.

**Why this priority**: Addresses R4 and the issue's own framing that this "may be primarily a
producer fix." Lower priority than Stories 1–2 because it is a documentation deliverable, not new
runtime behavior, and `docs/operations.md` already contains a first pass at this contract.

**Independent Test**: A reader unfamiliar with lcg's internals can, from the documented contract
alone, correctly implement a `.wal-generation.json` writer for a new stream without consulting
lcg source code.

**Acceptance Scenarios**:

1. **Given** the published producer-contract documentation, **When** a new non-lcg publisher
   creates a group's WAL stream directory for the first time, **Then** the documentation states
   unambiguously that it MUST write `.wal-generation.json` at that time, the exact file name,
   location, and JSON shape, and the consequence of omitting it (reset detection stays inert for
   that stream).

---

### Edge Cases

- A group's WAL directory has `*.jsonl` content but no `.wal-generation.json` (this issue's
  reproduction: a non-compliant producer). Must be distinguishable from "never hydrated" (Story
  1) and must not crash or silently pass reset detection with no trace (Story 2).
- A group's WAL directory has never been written to at all — no subdirectory exists yet. This is
  the ordinary "not yet hydrated" state and MUST NOT be reported as a warning condition.
- `.wal-generation.json` exists but is corrupt or unparseable — per ADR-0387, this MUST continue
  to collapse to "unknown," not to an error, and MUST NOT be distinguished from "producer never
  wrote it" by any change in this issue (Assumptions).
- A producer writes `.wal-generation.json` for the first time on a stream that was previously
  populated with no generation record. Per ADR-0387's existing `position_reset_detected` rule, a
  recorded `WalPosition` with `generation: None` compared against a newly-appeared `Some` value on
  disk is *already* treated as a detected reset by design (Story 5, Scenario 3 in ADR-0387). This
  issue MUST NOT change that existing behavior — confirmed explicitly so a resolution of Open
  Question 1 does not accidentally regress it.
- `.wal-bounds.json` (ADR-0375) is a distinct sidecar from `.wal-generation.json` and does not
  itself carry a generation field today. Any change to what `.wal-bounds.json` parsing recognizes
  is in scope only insofar as it's needed to answer Open Question 1.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_status` MUST report, per group, a generation signal that lets a caller
  distinguish three states: (a) no WAL stream exists yet for this group, (b) a WAL stream exists
  with a known, stable generation, and (c) a WAL stream exists but its generation is currently
  unknown (missing or corrupt `.wal-generation.json`). States (a) and (c) currently both collapse
  to `generation: null` and MUST become distinguishable.
- **FR-002**: The generation source of truth for a WAL stream is [NEEDS CLARIFICATION: Open
  Question 1 — is it producer-authored only (ADR-0387's current decision, unchanged), or does lcg
  additionally derive and persist its own generation identity for a populated stream it did not
  create], as resolved by Open Question 1.
- **FR-003**: For a normally produced and fully hydrated WAL stream (i.e., once Open Question 1 is
  resolved and any resulting producer-contract and/or lcg-side change is in place),
  `knowledge_status.wal_groups[g].generation` (and the top-level `wal.generation` for the default
  group) MUST report a non-null, stable value — not the permanently-null state observed in this
  issue's reproduction.
- **FR-004**: When `knowledge_rebuild_from_wal` evaluates reset detection against a group whose
  current on-disk generation is unknown, the resulting behavior MUST be [NEEDS CLARIFICATION: Open
  Question 2 — a documented loud warning (response field and/or log line) with replay still
  proceeding, or a hard refusal to auto-advance without explicit operator opt-in], not the current
  behavior of silently treating "unknown" as "never a mismatch" with no observable trace.
- **FR-005**: This issue MUST NOT change ADR-0387's existing comparison semantics for a *known*
  generation mismatch (`wal_generation::position_reset_detected`'s existing `Some != Some` and
  `None`-recorded-vs-`Some`-current cases) — scope is limited to the unknown/null case and its
  reporting.
- **FR-006**: The `.wal-generation.json` producer contract (file name, location, JSON shape, and
  when a publisher MUST write it) MUST be documented in one canonical, unambiguous location,
  resolving Open Question 3 (confirm/strengthen the existing `docs/operations.md` section, and/or
  add a tracked cross-repo dependency).
- **FR-007**: A corrupt or unparseable `.wal-generation.json` MUST continue to be indistinguishable
  from "no file written," per ADR-0387's existing design (a damaged sidecar must never masquerade
  as a detected reset) — this issue MUST NOT introduce a way to tell those two cases apart.

### Key Entities *(include if feature involves data)*

- **WAL stream generation**: A per-group-directory identity token (`.wal-generation.json`,
  `{"generation": "<string>"}`), stable for the life of a stream, opaque (compared for equality
  only). Existing entity from ADR-0387; this issue does not redefine its shape, only where it
  comes from and how its absence is reported/handled.
- **Generation status** *(new, pending Open Question 1)*: A per-group classification —
  not-yet-hydrated / known / unknown — that `knowledge_status` must expose so state (a) and (c)
  above are no longer conflated.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a group hydrated from a WAL directory containing content, `knowledge_status`
  never reports a value indistinguishable between "never hydrated" and "hydrated but generation
  unknown."
- **SC-002**: For the specific reproduction scenario in this issue (a group hydrated from a
  published WAL repo containing only `*.jsonl` files), `knowledge_status` reports a generation
  state consistent with whatever Open Question 1 resolves to — either a real non-null generation
  value, or an explicit, documented "unknown" status distinct from `null`-as-absence.
- **SC-003**: `knowledge_rebuild_from_wal` run against a group with a recorded position and an
  unknown current generation produces an observable signal (response field and/or log) in 100% of
  such runs — zero silent skips.
- **SC-004**: A reader with no prior lcg source-code knowledge can correctly describe, from
  documentation alone, what a WAL publisher must do to make a newly created stream
  generation-compliant.

## Assumptions

- ADR-0387's on-disk shape (`.wal-generation.json`, `{"generation": "<string>"}`) and its
  comparison semantics for *known* generations are correct and unchanged by this issue — only the
  unknown/null case's reporting and handling are in scope (per the issue's own Scope section).
- The consumer-side reset mechanics in orac #42 (using a generation compare to drive
  `force_clear`) are out of scope and assumed correct given a non-null generation, per the
  issue's explicit Scope statement.
- A corrupted `.wal-generation.json` must remain indistinguishable from a missing one; this issue
  does not add file-integrity detection for the sidecar itself.
- The two real-world reproduction repos (`GES/orac-psetadrs`, `adamb1/a2h`) are representative of
  the current orac/tarial publisher behavior (no `.wal-bounds.json`, no `.wal-generation.json`)
  and are treated as the baseline "normally produced" stream this issue must make non-null (or
  explicitly, loudly unknown) against.

## Out of Scope

- Consumer-side reset mechanics in orac #42 (already correct given a non-null generation).
- Changing ADR-0387's comparison semantics for two *known*, differing generations.
- Adding integrity/corruption detection to `.wal-generation.json` itself (a corrupt file stays
  indistinguishable from a missing one).
- Any change to `.wal-bounds.json`'s (ADR-0375) existing bounds-caching purpose beyond what's
  needed to answer Open Question 1.

## Source References

- ADR-0387 (`docs/adr/0387-wal-stream-generation-identity.md`) — WAL stream generation identity,
  the design this issue's inertness report is against.
- `docs/operations.md` (`.wal-generation.json` section) — existing producer-contract
  documentation, first pass.
- `crates/core/src/wal_generation.rs` — `read_generation`, `ensure_generation`,
  `position_reset_detected`, `generation_mismatch`.
- `crates/core/src/handlers.rs` — `knowledge_status`'s `wal`/`wal_groups` generation reporting;
  `handle_rebuild_from_wal`'s detection insertion point.
- #362 (`to_seq` rebuild bound), #365 (WAL checkpoints) — prior art referenced by ADR-0387.
- orac #42 — consumer-side reset mechanics (out of scope, cross-repo).
