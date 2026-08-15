# Feature Specification: WAL Stream Generation — Producer Contract & Unknown-State Guard

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
`generation: null` for every group, and top-level `wal.generation` is also `null`.

Generation identity is entirely separate from `.wal-bounds.json` — the two source repos shipping
without a `.wal-bounds.json` is a red herring for this issue. `.wal-bounds.json` (ADR-0375) is a
cached min/max-`seq` manifest for bounds lookups; it carries no generation information and never
has. Generation lives exclusively in its own sidecar, `.wal-generation.json`
(`crates/core/src/wal_generation.rs`), and both reproduction repos are missing that file too — that
absence, not the missing bounds manifest, is the actual condition this issue is about.

**Root cause**, confirmed against `WalWriter::new` (`crates/core/src/wal.rs:84-88`): lcg mints a
generation only when a stream's directory is opened with zero prior `.jsonl` content
(`global_seq == 0`). A hydrated stream lands its `*.jsonl` files on disk (via `git clone`/pull of
the published repo) before any lcg writer ever opens that directory, so by the time it's opened,
`global_seq` is already non-zero (189, 63 in the reproduction) — the minting branch is never
taken, and generation stays `None` permanently; nothing on any other code path mints one later.
This guard is deliberate and correct as designed: minting a generation for a stream that already
has content would fabricate an identity for a stream lcg never actually observed from creation,
which is worse than reporting none. Its consequence, though, is that the hydrate path — the
primary real-world use case ADR-0387 exists to serve — never acquires a generation under the
current design, and (per the resolution below) it must not be changed to do so.

**Resolution: generation identity must be producer-authored, not lcg-derived.** A locally
lcg-minted substitute cannot detect a producer-side reset: the reset check always compares the
generation lcg recorded last time against what's currently on disk, and if lcg minted that
recorded value itself, a producer force-push/rebuild changes the published `*.jsonl` files but not
lcg's own sidecar — so the comparison still reads "same stream" and the reset goes undetected.
Deriving a value locally would make `knowledge_status` report a non-null generation that satisfies
the letter of "non-null," while leaving reset detection exactly as inert as it is today — a worse
outcome than an honest `null`, because it looks fixed without being fixed. Generation identity has
to travel with the stream to mean anything, which makes it producer-authored by construction: this
issue does not add any lcg-side minting/derivation on the hydrate or any other consume-only path.

This issue's scope is therefore narrower than "make generation non-null everywhere": (1) make the
"generation unknown" state loud and distinguishable, both in `knowledge_status` and in
`knowledge_rebuild_from_wal`'s reset-detection path, rather than the current silent collapse into
`null`/"never a mismatch"; and (2) confirm and strengthen the producer-contract documentation for
`.wal-generation.json`. Making the two reproduction repos themselves report a real, non-null
generation requires their publisher (orac/tarial) to start writing `.wal-generation.json`, which is
a change outside this repository.

**Retrospective exposure, raised during clarification.** orac's consumer-side reset detection
(orac #42) decides `force_clear` by comparing generations, and every currently hydrated stream
reports `generation: null` — meaning that comparison has been null-vs-null since it shipped, for
every stream hydrated from published `*.jsonl` files, not just the two in this issue's
reproduction. Its reset detection has therefore never once fired, in either direction. The
consequence is retrospective as well as prospective: any producer-side rebuild or force-push that
has already happened against a currently-tracked channel would have been silently applied as a
forward advance, with no error surfaced at either end — precisely the corruption class ADR-0387
was written to close. Whether any such rebuild has actually occurred is an operational question
for channel operators to investigate independently (this issue has no way to detect, after the
fact, a reset that happened before the fix ships — that is the same undetectable case FR-002/FR-008
describe). Whether this exposure changes FR-004's warn-vs-refuse resolution is Open Question 4
below.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator can tell "generation known" from "generation unknown" (Priority: P1)

An operator or downstream consumer (e.g. orac #42, which drives its `force_clear` reset off a
generation compare) calls `knowledge_status` against a group that has been fully hydrated from a
published WAL stream. Today, both "this group has never been created" and "this group has real
content but no recorded generation" report identically as `generation: null` — there is no way to
tell a healthy-but-non-compliant stream apart from one that was simply never written to. This
condition is not hypothetical: it is the state of every stream produced by the current real-world
publisher, since it does not yet write `.wal-generation.json` at all.

**Why this priority**: This is the reproduction's core symptom and the reason ADR-0387's detection
is inert end-to-end — without this distinction, nothing downstream of `knowledge_status` can reason
about generation at all, and it is the only piece of this issue that changes what a caller observes
for the two specific groups in the reproduction (their generation stays unrecoverable, but "unknown"
becomes an honest, explicit signal instead of an indistinguishable `null`).

**Independent Test**: Hydrate a group from a WAL directory containing `*.jsonl` content but no
`.wal-generation.json`, then call `knowledge_status`. The response must let a caller distinguish
this case from a group with no WAL directory at all.

**Acceptance Scenarios**:

1. **Given** a group WAL directory with no `*.jsonl` content and no `.wal-generation.json` (never
   hydrated), **When** `knowledge_status` is called, **Then** the group's generation is reported in
   a way that is recognizable as "not applicable / no stream yet," not conflated with a stream that
   exists but lacks a generation record.
2. **Given** a group WAL directory with `*.jsonl` content but no `.wal-generation.json` (the
   reproduction's case, and the current state of every group produced by a non-compliant
   publisher), **When** `knowledge_status` is called, **Then** the response surfaces this as a
   distinct, identifiable "unknown" condition rather than indistinguishable `null`.

---

### User Story 2 - Reset detection warns loudly instead of silently skipping when generation is unknown (Priority: P1)

An operator runs `knowledge_rebuild_from_wal` against a group that already has a recorded
`WalPosition` and whose current on-disk generation is unknown. Today this silently proceeds as an
ordinary incremental replay with no trace that generation-based reset detection did not run — the
operator has no way to know they are exposed to the exact corruption scenario ADR-0387 exists to
prevent.

**Why this priority**: This is the other half of "inert" — even once `knowledge_status` reports the
unknown state (Story 1), nothing today tells an operator that a *specific replay* skipped the
safety check.

**Independent Test**: Run `knowledge_rebuild_from_wal` against a group with a recorded position and
an unknown on-disk generation, and confirm the response and/or logs make the "reset detection could
not run" condition observable.

**Acceptance Scenarios**:

1. **Given** a group with a previously recorded `WalPosition` and an on-disk generation that is
   currently unknown (missing or corrupt `.wal-generation.json`), **When**
   `knowledge_rebuild_from_wal` is called, **Then** the response and/or logs surface an explicit
   warning that generation-based reset detection could not run for this call. Whether replay then
   proceeds using the caller's requested `from_seq`/`to_seq`/`force_clear` (warn-and-proceed,
   matching today's non-blocking behavior) or is refused pending explicit operator opt-in is
   reopened by Open Question 4 below — a warning that goes unread reproduces the current
   silent-corruption exposure with extra steps, per the retrospective-exposure note in Background.

---

### User Story 3 - Producer contract for `.wal-generation.json` is unambiguous (Priority: P2)

A team implementing or maintaining a non-lcg WAL publisher (orac/tarial, or any future
distributed-WAL producer) needs a single, canonical, complete description of what to write, in
what format, and when, for `.wal-generation.json` — so that a newly published stream is
generation-compliant from its first write rather than requiring a later lcg-side workaround. Both
producers in this issue's reproduction (`GES/orac-psetadrs`, `adamb1/a2h`) currently ship without
this file and need their publish step changed to become compliant, independent of anything this
issue changes in lcg.

**Why this priority**: Addresses the producer-contract requirement and the issue's own framing that
this is primarily a producer-side fix. Lower priority than Stories 1–2 because it is a
documentation deliverable, not new runtime behavior, and `docs/operations.md` already contains a
first pass at this contract.

**Independent Test**: A reader unfamiliar with lcg's internals can, from the documented contract
alone, correctly implement a `.wal-generation.json` writer for a new stream without consulting lcg
source code.

**Acceptance Scenarios**:

1. **Given** the published producer-contract documentation, **When** a new non-lcg publisher
   creates a group's WAL stream directory for the first time, **Then** the documentation states
   unambiguously that it MUST write `.wal-generation.json` at that time, the exact file name,
   location, and JSON shape, and the consequence of omitting it (reset detection stays inert for
   that stream, surfaced only as the "unknown" state from Story 1).

---

### Edge Cases

- A group's WAL directory has `*.jsonl` content but no `.wal-generation.json` (this issue's
  reproduction: a non-compliant producer). Must be distinguishable from "never hydrated" (Story 1)
  and must not crash or silently pass reset detection with no trace (Story 2).
- A group's WAL directory has never been written to at all — no subdirectory exists yet. This is
  the ordinary "not yet hydrated" state and MUST NOT be reported as a warning condition.
- `.wal-generation.json` exists but is corrupt or unparseable — per ADR-0387, this MUST continue to
  collapse to "unknown," not to an error, and MUST NOT be distinguished from "producer never wrote
  it" by any change in this issue (Assumptions).
- A producer writes `.wal-generation.json` for the first time on a stream that was previously
  populated with no generation record. Per ADR-0387's existing `position_reset_detected` rule, a
  recorded `WalPosition` with `generation: None` compared against a newly-appeared `Some` value on
  disk is *already* treated as a detected reset by design (Story 5, Scenario 3 in ADR-0387). This
  issue MUST NOT change that existing behavior.
- `.wal-bounds.json` (ADR-0375) is a distinct sidecar from `.wal-generation.json` and does not
  itself carry a generation field, and never has — confirmed during clarification. This issue makes
  no change to `.wal-bounds.json` parsing.
- A stream hydrated via `git clone`/pull of a published repo is opened by lcg's `WalWriter` only
  after its `*.jsonl` files already exist on disk — `global_seq` is non-zero at open time, so the
  existing minting guard in `WalWriter::new` (`crates/core/src/wal.rs:84-88`) never fires for it.
  This issue does not change that guard.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_status` MUST report, per group, a generation signal that lets a caller
  distinguish three states: (a) no WAL stream exists yet for this group, (b) a WAL stream exists
  with a known, stable generation, and (c) a WAL stream exists but its generation is currently
  unknown (missing or corrupt `.wal-generation.json`). States (a) and (c) currently both collapse to
  `generation: null` and MUST become distinguishable.
- **FR-002**: The generation source of truth for a WAL stream is **producer-authored only**. lcg
  MUST NOT derive, mint, or substitute a generation for a stream it did not create, on the hydrate
  path or any other consume-only path — a locally-minted value cannot detect a producer-side reset
  (the comparison is always against lcg's own previously recorded value, which a local mint would
  make self-referential) and would misleadingly present as "fixed" while leaving reset detection as
  inert as it is today.
- **FR-003**: For a stream produced by a generation-compliant publisher (one that writes
  `.wal-generation.json` per FR-006's contract) and fully hydrated, `knowledge_status.wal_groups[g].generation`
  (and the top-level `wal.generation` for the default group) MUST report a non-null, stable value.
  For a stream produced by a non-compliant publisher — including both groups in this issue's
  reproduction, as their producers currently exist — the deliverable is FR-001's explicit "unknown"
  signal, not a fabricated non-null value; this issue does not, by itself, make the reproduction's
  two groups report a non-null generation, since that requires their producer to be fixed
  (out of this repo's scope).
- **FR-004**: When `knowledge_rebuild_from_wal` evaluates reset detection against a group whose
  current on-disk generation is unknown, it MUST surface an explicit, observable "unknown
  provenance" warning (response field and/or log line) rather than silently treating it as "never a
  mismatch" with no trace. Whether replay then proceeds using the caller's requested
  `from_seq`/`to_seq`/`force_clear` (warn-and-proceed, this issue's prior default) or is refused
  pending explicit operator opt-in is [NEEDS CLARIFICATION: Open Question 4 — reopened during
  clarification by the retrospective-exposure note in Background: since orac #42's generation
  compare has been null-vs-null for every hydrated stream, its reset detection has never fired, so
  any already-occurred producer reset was already silently misapplied as a forward advance; does
  that argue for refuse-to-advance now rather than warn-and-proceed?].
- **FR-005**: This issue MUST NOT change ADR-0387's existing comparison semantics for a *known*
  generation mismatch (`wal_generation::position_reset_detected`'s existing `Some != Some` and
  `None`-recorded-vs-`Some`-current cases) — scope is limited to the unknown/null case and its
  reporting.
- **FR-006**: The `.wal-generation.json` producer contract (file name, location, JSON shape, and
  when a publisher MUST write it) MUST be documented in one canonical, unambiguous location —
  confirming and strengthening the existing `docs/operations.md` section — and MUST explicitly note
  that a publisher creating a group's stream directory for the first time is required to write this
  file at that time for reset detection to ever apply to that stream.
- **FR-007**: A corrupt or unparseable `.wal-generation.json` MUST continue to be indistinguishable
  from "no file written," per ADR-0387's existing design (a damaged sidecar must never masquerade as
  a detected reset) — this issue MUST NOT introduce a way to tell those two cases apart.
- **FR-008**: lcg MUST NOT mint or derive a generation on any hydrate/consume-only path. The
  existing guard in `WalWriter::new` (`crates/core/src/wal.rs:84-88`, minting only when
  `global_seq == 0`) MUST remain the sole minting condition — no new code path may mint a generation
  against a directory with pre-existing content that lcg did not create.

### Key Entities *(include if feature involves data)*

- **WAL stream generation**: A per-group-directory identity token (`.wal-generation.json`,
  `{"generation": "<string>"}`), stable for the life of a stream, opaque (compared for equality
  only), producer-authored. Existing entity from ADR-0387; this issue does not redefine its shape or
  its authorship model, only how its absence is reported and handled.
- **Generation status** *(new)*: A per-group classification — not-yet-hydrated / known / unknown —
  that `knowledge_status` must expose so states (a) and (c) above are no longer conflated.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a group hydrated from a WAL directory containing content, `knowledge_status`
  never reports a value indistinguishable between "never hydrated" and "hydrated but generation
  unknown."
- **SC-002**: For the specific reproduction scenario in this issue (a group hydrated from a
  published WAL repo containing only `*.jsonl` files, no `.wal-generation.json`),
  `knowledge_status` reports the group's generation as explicitly "unknown" (FR-001) rather than an
  indistinguishable `null` — becoming a real non-null value requires the producer to be fixed, which
  is outside this issue's success measure.
- **SC-003**: `knowledge_rebuild_from_wal` run against a group with a recorded position and an
  unknown current generation produces an observable signal (response field and/or log) in 100% of
  such runs — zero silent skips.
- **SC-004**: A reader with no prior lcg source-code knowledge can correctly describe, from
  documentation alone, what a WAL publisher must do to make a newly created stream
  generation-compliant.

## Assumptions

- ADR-0387's on-disk shape (`.wal-generation.json`, `{"generation": "<string>"}`) and its
  comparison semantics for *known* generations are correct and unchanged by this issue — only the
  unknown/null case's reporting and handling are in scope.
- Generation identity must be producer-authored to be meaningful: a locally lcg-minted value cannot
  detect a producer-side reset, because the comparison is always against lcg's own previously
  recorded value. Confirmed during clarification; this rules out any lcg-side derivation/minting on
  a consume-only (hydrate) path.
- The consumer-side reset mechanics in orac #42 (using a generation compare to drive `force_clear`)
  are out of scope and assumed correct given a non-null generation, per the issue's explicit Scope
  statement.
- A corrupted `.wal-generation.json` must remain indistinguishable from a missing one; this issue
  does not add file-integrity detection for the sidecar itself.
- The two real-world reproduction repos (`GES/orac-psetadrs`, `adamb1/a2h`) are representative of
  the current orac/tarial publisher behavior (no `.wal-generation.json`) and will remain in the
  "unknown" generation state after this issue ships, until their publisher is changed independently.
- `.wal-bounds.json` (ADR-0375) is unrelated to generation identity and requires no change for this
  issue.

## Out of Scope

- Consumer-side reset mechanics in orac #42 (already correct given a non-null generation).
- Changing ADR-0387's comparison semantics for two *known*, differing generations.
- Adding integrity/corruption detection to `.wal-generation.json` itself (a corrupt file stays
  indistinguishable from a missing one).
- Any change to `.wal-bounds.json` (ADR-0375) parsing or its bounds-caching purpose.
- Any lcg-side derivation or minting of a substitute generation for a stream lcg did not create,
  on the hydrate path or otherwise (see Resolution in Background and FR-002/FR-008).
- Detecting, after the fact, whether a producer reset already occurred before this fix ships —
  that is the same undetectable case FR-002/FR-008 describe; any such audit is an operational task
  for channel operators, not a code deliverable of this issue.
- Implementing the actual producer-side fix in orac/tarial (making `GES/orac-psetadrs`,
  `adamb1/a2h`, or the publisher generally write `.wal-generation.json`) — this repo's deliverable
  is the documented contract (FR-006), not the cross-repo implementation.

## Source References

- ADR-0387 (`docs/adr/0387-wal-stream-generation-identity.md`) — WAL stream generation identity,
  the design this issue's inertness report is against.
- `docs/operations.md` (`.wal-generation.json` section) — existing producer-contract documentation,
  first pass.
- `crates/core/src/wal_generation.rs` — `read_generation`, `ensure_generation`,
  `position_reset_detected`, `generation_mismatch`.
- `crates/core/src/wal.rs:84-88` (`WalWriter::new`) — the `global_seq == 0` minting guard that is
  this issue's confirmed root cause, and which FR-008 requires stay unchanged.
- `crates/core/src/handlers.rs` — `knowledge_status`'s `wal`/`wal_groups` generation reporting;
  `handle_rebuild_from_wal`'s detection insertion point.
- #362 (`to_seq` rebuild bound), #365 (WAL checkpoints) — prior art referenced by ADR-0387.
- orac #42 — consumer-side reset mechanics (out of scope, cross-repo).
