# Feature Specification: knowledge_status cannot distinguish an empty group from an unhydrated one

**Feature Branch**: `fabrik/issue-456`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "`knowledge_status` reports `applied_seq` and `max_seq` for every group but never compares them, so 'this group is empty because it holds nothing' and 'this group is empty because its WAL has not been replayed' are indistinguishable to a caller."

## Background

On startup with an empty database beside populated per-group WAL directories, `lcg-service` comes up
`healthy` with an empty graph and treats the empty DB as authoritative. It does not replay on boot; it
only scans the WAL to report `max_seq`/`generation`.

The engine already holds both facts needed to detect the inconsistency, it just never compares them:

- `applied_seq` — from the DB (`backfill_applied_seq_if_absent`, `recovery.rs:170`), derived from
  **DB contents**: an empty group takes the `count_episodics_by_group_ids == 0` path and records
  `applied_seq = 0` without consulting the WAL at all.
- `max_seq` — from the WAL directory scan.

`applied_seq` of 0/null while `max_seq > 0` for a group with no content is precisely "the DB is behind
what the log says." Nothing compares them today, so a fresh or wiped database beside a full WAL is
reported as an authoritative empty corpus. A consumer that mirrors "what the engine currently holds"
can propagate that emptiness — silent data loss whose root cause is an empty DB being assumed correct
rather than merely unhydrated.

**This is the same collapse #414 fixed, one layer up.** #414 had the identical shape: `generation:
null` meant both "no stream exists yet" and "a stream exists but its generation is unrecoverable," so a
real problem was indistinguishable from a normal state. Its FR-001 changed no existing field — it added
a sibling `generation_status` with three explicit values (`known` / `unknown` / `not_applicable`) to
**both** the flat `wal` object (which reflects the default group) and every `wal_groups[*]` entry. This
issue is that same fix, applied to hydration state instead of generation state, and follows the same
placement and additive pattern.

**Why `healthy` must not move.** #455 (the report this issue is split from) offered, as one option, a
non-`healthy` health signal for this condition. That option is explicitly rejected here. `healthy:
false` is consumed by process supervisors — container orchestrators, systemd, load-balancer probes —
whose response to unhealthy is to restart or replace the process. A restart hydrates nothing: the
database is still empty, the WAL is still ahead, and the next boot reports unhealthy again. Flipping
this state to unhealthy converts a condition that needs an operator or consumer decision into a
crash-loop, and removes from rotation a service that is fully able to serve reads. `healthy` answers
"can this process serve requests?" — and it can. The hydration question is per-group data state, and
belongs where the two numbers being compared already live: `knowledge_status`'s per-group WAL
reporting, not `handle_health`.

**Downstream motivation.** This blocks the orac project: a downstream consumer needs to distinguish
"empty" from "not yet hydrated" to avoid propagating an empty mirror. It was reported while hardening
tarial's volume/fault-tolerance behaviour on a wiped data volume.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Distinguish "empty" from "not yet hydrated" per group (Priority: P1)

An operator or a downstream consumer (e.g. orac, which mirrors the graph and must not propagate an
empty result as if it were authoritative) queries `knowledge_status` against a workspace where the
database is empty or partially caught up but the WAL directory holds unreplayed content for one or
more groups. Today, a genuinely empty group and a group whose WAL is simply ahead of the DB both
report as an empty-looking group unless the caller manually fetches and compares `applied_seq` against
`max_seq` itself — and a caller that skips that comparison (as the reported downstream case did) treats
an unhydrated group as an authoritative empty corpus.

**Why this priority**: This is the entire reported defect and the entire deliverable of this issue —
without it, nothing changes for the blocked downstream consumer.

**Independent Test**: Seed a single workspace with three groups in three different states (no WAL
content at all; WAL content the DB has not applied; WAL content the DB has fully applied), call
`knowledge_status` once, and confirm each group's per-group signal reports its own state independently
of the other two.

**Acceptance Scenarios**:

1. **Given** a wiped database beside a populated WAL directory for group A, where A's `applied_seq` has
   never been backfilled, **When** `knowledge_status` is called, **Then** A's per-group hydration signal
   reports the "WAL holds unapplied content" state, distinct from the "not applicable" state.
2. **Given** a group B with no WAL content at all (no `*.jsonl` files, `max_seq` absent or zero),
   **When** `knowledge_status` is called, **Then** B's per-group hydration signal reports "not
   applicable."
3. **Given** a group C whose recorded `applied_seq` is greater than or equal to its `max_seq` (and
   `max_seq` is nonzero), **When** `knowledge_status` is called, **Then** C's per-group hydration signal
   reports "up to date."
4. **Given** a single workspace containing groups A, B, and C from Scenarios 1-3 all at once, **When**
   `knowledge_status` is called once, **Then** the response reports three different values, one per
   group, each correct independently of the others.
5. **Given** any of the workspace states in Scenarios 1-4, **When** `handle_health` is called, **Then**
   it returns exactly the same `healthy`/`degraded` value it would have returned before this change —
   this change does not alter that determination in any case.

---

### User Story 2 - The new field is a documented, reliable contract (Priority: P2)

A team building a downstream consumer against `knowledge_status` (e.g. orac, or any future consumer)
needs the new per-group signal documented well enough to treat it as an API contract, not something
inferred by reading `lcg` source or reverse-engineering behavior from observation — the same trap that
originally produced this issue (a consumer comparing `applied_seq`/`max_seq` itself instead of a
documented signal).

**Why this priority**: Necessary for the field to be usable at all outside this repository, but
strictly follows Story 1 — there is nothing to document until the field exists.

**Independent Test**: A reader unfamiliar with `lcg`'s internals can, from `docs/ipc-mcp-reference.md`
alone, correctly predict the field's value for a given `applied_seq`/`max_seq` pair without reading
source code.

**Acceptance Scenarios**:

1. **Given** the published `docs/ipc-mcp-reference.md`, **When** a reader looks up `knowledge_status`,
   **Then** the new field's name, its possible values, and the precise condition each value represents
   are documented, along with where it appears (the flat `wal` object and each `wal_groups[*]` entry).

---

### Edge Cases

- A group whose `applied_seq` has never been backfilled at all. Per-group backfill happens lazily
  inside `knowledge_status` itself (`handlers.rs:367`) rather than at startup (startup backfill,
  `main.rs:598`, is default-group-only) — the signal must be correct on a group's very first
  `knowledge_status` call, not only on a second call after backfill has already happened once.
- A group whose WAL directory exists but is itself empty (`max_seq` is zero or absent) is
  indistinguishable, for this signal's purposes, from a group with no WAL directory at all — both are
  "not applicable," since there is no WAL content to be behind on.
- `applied_seq` greater than `max_seq` (for example, following a generation reset elsewhere in the
  system) is treated as "up to date," not a new fourth state or an error — this issue does not add
  anomaly detection for that condition; see Assumptions.
- The field must be correct for every group listed in `wal_groups`, not only the default group — this
  is the same scope #414's `generation_status` already covers, and this signal must match that
  coverage exactly.
- Determining the signal must not itself trigger any replay, write, or other mutation beyond what
  `knowledge_status` already performs today (the existing lazy `applied_seq` backfill, which is
  unchanged, existing behavior — not something this issue adds).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_status` MUST report, per group, a signal with exactly three possible values
  that let a caller distinguish: (a) the group is up to date with its WAL (`applied_seq` is present and
  is greater than or equal to `max_seq`, and `max_seq` is nonzero); (b) the group's WAL holds content
  the database has not applied (`max_seq` is nonzero and exceeds `applied_seq`, treating an absent or
  never-backfilled `applied_seq` as zero for this comparison); (c) the distinction does not apply
  because the group has no WAL content at all (`max_seq` is zero or absent). Naming follows the
  `generation_status` precedent: field name `hydration_status`, values `hydrated` / `wal_ahead` /
  `not_applicable`, for states (a) / (b) / (c) respectively.
- **FR-002**: `hydration_status` MUST be a **new, additive sibling field**. `applied_seq`, `max_seq`,
  `generation`, and `generation_status` MUST keep their current values and meanings, so a caller reading
  only those fields is unaffected by this change.
- **FR-003**: `hydration_status` MUST be reported for every group present in `wal_groups`, not only the
  default group, and MUST be correct for a group whose `applied_seq` has never been backfilled — the
  lazy per-group backfill (`handlers.rs:367`) MUST be allowed to run (per its existing, unchanged
  behavior) before the comparison, so the signal is correct on a group's first `knowledge_status` call.
- **FR-004**: `hydration_status` MUST also appear as a sibling of the existing top-level `generation`/
  `generation_status` fields in the flat `wal` object (which reflects the default group), mirroring
  exactly where `generation_status` was added by #414 — both the flat `wal` object and every
  `wal_groups[*]` entry, not one or the other.
- **FR-005**: `handle_health`'s `healthy` / `degraded` determination MUST NOT change in any case as a
  result of this issue. See *Why `healthy` must not move* in Background — this is a hard constraint,
  not a preference.
- **FR-006**: Determining `hydration_status` MUST NOT perform a WAL replay, a database write, or any
  other mutation beyond what `knowledge_status` already performs today. It is a pure comparison of
  values (`applied_seq`, `max_seq`) the handler already reads.
- **FR-007**: Tests MUST cover, at minimum: a genuinely empty group with no WAL content
  (`not_applicable`); a group whose WAL holds content the DB has not applied, including the case where
  `applied_seq` was never backfilled before this call (`wal_ahead`); a fully hydrated group
  (`hydrated`); and a single workspace containing all three groups at once, asserting each reports its
  own value independently of the others (Story 1, Scenario 4).
- **FR-008**: `docs/ipc-mcp-reference.md` MUST document `hydration_status`: its name, its three possible
  values, the precise condition each value represents, and where it appears in the response shape (flat
  `wal` object and every `wal_groups[*]` entry) — so a consumer can rely on it as a documented contract
  rather than inferring it from source or from comparing `applied_seq`/`max_seq` itself.

### Key Entities *(include if feature involves data)*

- **Hydration status** *(new)*: A per-group classification — `hydrated` / `wal_ahead` / `not_applicable`
  — that `knowledge_status` exposes so "empty because there's nothing" and "empty because it hasn't been
  replayed" are no longer conflated. Derived entirely from values `knowledge_status` already reads
  (`applied_seq`, `max_seq`); introduces no new stored state.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a wiped database beside a populated WAL for group A, `knowledge_status` reports A's
  inconsistency explicitly (`hydration_status: "wal_ahead"`), and a caller can distinguish it from a
  genuinely empty group without independently comparing sequence numbers itself.
- **SC-002**: `handle_health` returns exactly what it does today, unchanged, in every workspace state
  exercised by this issue's tests.
- **SC-003**: A workspace with one hydrated group, one unhydrated (`wal_ahead`) group, and one
  genuinely empty group reports three different `hydration_status` values in a single `knowledge_status`
  call.
- **SC-004**: No IPC or MCP call performs additional filesystem I/O, database I/O, or mutation as a
  result of this change, beyond what `knowledge_status` already performs today.

## Assumptions

- The suggested field name (`hydration_status`) and its three values (`hydrated` / `wal_ahead` /
  `not_applicable`) are adopted directly, mirroring `generation_status`'s naming and shape exactly —
  the issue flagged naming as open but proposed this shape, and this project's precedent (#414) settled
  the analogous naming question the same way. Research/Plan may still revisit the literal strings if a
  concrete conflict turns up, but the three-state semantic split itself is fixed by this spec.
- `applied_seq` greater than `max_seq` is classified as `hydrated` ("up to date"), not a distinct
  anomaly state. Detecting that specific condition as its own signal is out of scope for this issue.
- The existing lazy per-group `applied_seq` backfill inside `knowledge_status` (`handlers.rs:367`) is
  unchanged, existing behavior; this issue does not introduce a new mutation, it only adds a computed
  field derived from values that backfill (and the WAL scan) already produce.
- `wal_groups` only ever lists groups that have a WAL directory on disk; a group with literally no WAL
  directory at all is not something `hydration_status` needs to represent, since it would not appear in
  `wal_groups` to begin with.

## Out of Scope

- **Auto-hydration on startup** (#455's option 1). It collides with consumer-driven rebuild and
  generation-based reset detection (#387), with #414's unknown-generation refusal — which would make a
  glob-published stream fail *at boot* rather than at an explicit rebuild — and with #432, where
  `force_clear`'s guard can clear the wrong group. Worth its own issue and its own argument.
- **Refusing or erroring reads while unhydrated** (#455's option 3). Since #413 an omitted `group_ids`
  means all groups, so refusing a read because one group is unhydrated would break queries over groups
  that are fine.
- **Changing `handle_health`'s `healthy`/`degraded` semantics** in any way — see FR-005 and Background.
- **A distinct anomaly state for `applied_seq > max_seq`** — classified as `hydrated` per Assumptions,
  not surfaced separately.
- **Any new stored state.** `hydration_status` is computed on read from values already read; nothing
  new is persisted.

## Source References

- ADR-0414 (`docs/adr/0414-wal-generation-unknown-refuses-replay.md`) — the precedent for the mechanism
  (additive sibling field), the placement (flat `wal` object + every `wal_groups[*]` entry), and the
  three-state shape this issue follows.
- `crates/core/src/recovery.rs:170` (`backfill_applied_seq_if_absent`) — where `applied_seq` is derived
  from DB contents, including the empty-group `count_episodics_by_group_ids == 0` path that records
  `applied_seq = 0` without consulting the WAL.
- `crates/core/src/handlers.rs:367` — where per-group `applied_seq` backfill happens lazily from
  `knowledge_status` (as opposed to `main.rs:598`'s default-group-only startup backfill).
- `crates/core/src/handlers.rs` — `wal_generation_status` and `wal_group_positions_json`, the existing
  `generation_status` implementation this issue's `hydration_status` mirrors.
- ADR-0027 — autonomous WAL recovery; `is_recoverable` matches only corrupt-WAL and
  permission/missing-file cases, never a merely-empty database, so it does not fire for the condition
  this issue addresses.
- #353 — `applied_seq` backfill, the mechanism this issue's comparison depends on.
- #455 — the report this issue is split from; source of the *Why `healthy` must not move* constraint
  and the two explicitly out-of-scope options.
