# ADR-0428: Legacy-Migration Generation Stamp and a Content-Scoped Guard Exemption

**Status**: Accepted
**Date**: 2026-08-17
**Issue**: #428
**Relates to**: ADR-0378 (multi-stream WAL per group directory), ADR-0387 (WAL stream generation
identity), ADR-0414 (unknown-generation streams refuse to advance)

## Context

ADR-0378's `migrate_wal_root_if_needed` relocates a pre-378 flat `LCG_WAL_DIR` (loose `*.jsonl`,
`.checkpoints/`, `.wal-bounds.json` directly under the WAL root) into `<wal_root>/liminis/` on
first boot under an upgraded binary. ADR-0414 (shipped in 0.13.2) made
`knowledge_rebuild_from_wal` refuse outright, for any group, once a position has been recorded for
it (`applied_seq.is_some()`) if the group's current on-disk generation is unknown (missing or
corrupt `.wal-generation.json`).

These two decisions were each individually correct and are now in a genuine conflict. A pre-0.13.0
install never had a `.wal-generation.json` — the feature postdates it — so the migration relocates
real content into a per-group directory that has no generation to relocate. The very next
`knowledge_status` call (or the migration's own resulting first rebuild's completion write)
records a `WalPosition` for that group with `generation: None` — an ordinary, correct "adopt on
first encounter" per ADR-0387. From that moment on, every group this migration has ever touched
permanently satisfies ADR-0414's refusal condition: a recorded position, an unknown generation.
`knowledge_rebuild_from_wal` becomes unavailable, forever, on every workspace that has gone
through this migration — which is every workspace upgrading from ≤0.12.x, and every workspace
already migrated by 0.13.0 or 0.13.1 (neither of which stamped a generation either, since this gap
predates ADR-0414 by definition — the migration code and the guard were both correct in isolation
and only collide because neither anticipated the other).

Verified against the released `aarch64-apple-darwin` artifacts for 0.13.1 and 0.13.2 with an
identical legacy workspace seeded from this repo's `crates/core/tests/fixtures/real_corpus_wal/wal/`
fixture (16 files, `max_seq: 12481`, no `.lcg/db`): both versions migrate the flat layout
identically, but 0.13.1 rebuilds successfully (1506 entities, 2392 relationships, 228 episodes,
`applied_seq == max_seq == 12481`) while 0.13.2 refuses with `WalGenerationUnknown`. This is a
regression on the upgrade path, not a new-workspace defect — and it breaks two documented
recovery playbooks that assume `knowledge_rebuild_from_wal` works after `.lcg/db` is lost or
deliberately removed: the #398 lbug-upgrade rollback procedure, and any operator-invoked
ADR-0009 degraded-mode recovery that reaches for an explicit rebuild rather than relying solely on
the automatic startup sequence (which does not go through this guard at all — see Consequences).

**This is a direct, explicit reversal of two prior positions**, not an implementation detail:

- ADR-0387's own spec explicitly excluded, as Out of Scope, "a one-time migration pass that
  retroactively writes generation files for every pre-existing stream."
- ADR-0414's own spec explicitly excluded, as Out of Scope, "any lcg-side derivation or minting of
  a substitute generation for a stream lcg did not create from empty."

Both exclusions were reasoned, not incidental: ADR-0414's own Context section documents that the
two real production incidents motivating the guard were externally-published streams whose
publish step silently dropped `.wal-generation.json` via a `*.jsonl`-only glob — and **there is no
on-disk signal that distinguishes that case from a workspace this migration touched.** Both end up
as `<wal_root>/<group>/` holding `*.jsonl` content, a recorded `WalPosition`, and no
`.wal-generation.json`. Any fix that stamps or adopts a generation purely by inspecting directory
contents would silently re-open exactly the failure ADR-0414 exists to close, for the externally-
stripped case, defeating the guard's actual protection rather than fixing this issue's regression.

## Decision

Two independent, narrowly-scoped mechanisms — one per gap — rather than one general "backfill a
missing generation" rule. Each is scoped to a condition specific enough that it cannot also fire
for a genuinely external, unknown stream.

### Mechanism 1 — migration-time stamp, scoped by provenance

`migrate_wal_root_if_needed` now calls `wal_generation::ensure_generation(&default_dir)`
immediately after relocating a genuine flat legacy layout — but **only on a call whose own
top-level scan found loose legacy artifacts to relocate**. That "found something loose at the WAL
root's top level" signal is a reliable, unforgeable local-provenance marker: nothing else in this
codebase — no publish/subscribe path, no external tool, no other WAL-writing code path — ever
places WAL artifacts directly at the WAL root's top level. A call that finds nothing loose (an
already-fully-migrated root, a fresh install, or a native per-group install that never had a flat
layout) mints nothing. This closes FR-001/SC-001: a fresh ≤0.12.x-to-current upgrade ends migration
with a readable `.wal-generation.json`, before any rebuild is ever attempted.

This mechanism is defensible against ADR-0387's Out-of-Scope exclusion because it is not "a
migration pass over every pre-existing stream" — it is scoped to content this exact code path just
proved, in this exact call, that it owns (it watched the loose files before it moved them).

### Mechanism 2 — guard exemption, scoped by content, not provenance

Mechanism 1 alone does not help a workspace already migrated by 0.13.0/0.13.1: by the time this
fix runs, the loose-top-level-entries signal is gone (an earlier release already did the move), so
there is nothing left to distinguish that workspace from an externally-stripped stream by
inspecting the directory. This gap is closed by a **content-based safety condition** instead,
applied inside `handle_rebuild_from_wal`'s existing unknown-generation guard: the refusal is
exempted exactly when `recorded_position.applied_seq == Some(0)` **and** the group has zero rows
in the database (`recovery::group_has_no_content`).

This is the literal condition under which there is demonstrably nothing previously applied to
protect — regardless of *why* the generation is unknown. A full replay from empty can only ever
reproduce the WAL's own content; there is nothing a genuine reset could silently corrupt, because
nothing has been applied yet. The exemption does not attempt to identify *why* the generation is
missing (this migration's history vs. an external publish gone wrong) — it does not need to,
because the safety property it relies on (nothing at risk) holds either way. Any group with real
prior content at risk — `applied_seq > 0`, or a non-empty database regardless of `applied_seq` —
remains refused exactly as before, satisfying FR-003/SC-004: the guard's actual protection is
unchanged for every case it was built to catch.

When the exemption applies on a **non-dry-run** call, `handle_rebuild_from_wal` mints a generation
right then (`wal_generation::ensure_generation`) before running `position_reset_detected`, rather
than leaving the workspace unstamped and permanently re-hitting this same exemption on every later
call — that would be a real regression relative to 0.13.1, which has no guard to hit at all. The
newly-minted generation makes `position_reset_detected(Some(0), None, Some(new))` evaluate `true`
— exactly ADR-0387's Story 5 Scenario 3 ("a generationless baseline gains a generation"), whose
purge-and-replay machinery is unmodified and already tested — so the completion write persists the
new generation and every later call against this group behaves as an ordinary, generation-known
replay. `dry_run: true` never mutates under this branch: it falls through to the ordinary,
non-reset preview path (`current_generation` stays `None`), consistent with every other dry-run
path in this codebase never writing to disk.

### FR-004: the refusal message names both possible remedies

For a call that still refuses (the exemption does not apply), the error message now names two
remedies rather than one, since the code cannot tell which applies and does not need to: remove or
move aside `.lcg/db` and retry (the #398 rollback route, which lands a locally-owned workspace back
in Mechanism 2's exempted case above), or republish the stream's full directory, dot-namespace
included, if it was genuinely published externally. The prior text ("republish this stream's full
directory") assumed the ADR-0387 publish/subscribe path exclusively and gave an operator on a
locally migrated workspace — who has no publisher to re-copy from — no actionable route.

### `ensure_generation`'s documented precondition

`wal_generation::ensure_generation`'s doc comment previously stated callers "MUST only invoke this
when the stream has no prior content." Both mechanisms above deliberately violate that precondition
— they mint over a directory that already holds real WAL content. The function's own behavior
already supported this (it only checks whether a generation record exists, not whether content
exists), so no code change was needed there; the doc comment is updated to describe these two
call sites explicitly, replacing the blanket "MUST NOT" with the specific safety argument each one
relies on (provenance for Mechanism 1, content-emptiness for Mechanism 2).

## Rejected Alternatives

**A single, general "stamp any group missing a generation on next open" backfill.** Would satisfy
FR-001/FR-002 but violates FR-003/SC-004 directly: directory shape alone cannot distinguish this
migration's own history from an externally-stripped publish (see Context), so this would silently
re-admit the exact failure class ADR-0414 exists to catch, just relocated from "compares null
against null forever" to "compares a locally-fabricated value with no relationship to the true
publisher-side identity." Rejected as unsafe, not merely inelegant.

**A guard-narrowing rule alone, with no migration-time stamp.** Rejected because FR-001's own
acceptance scenario requires a readable `.wal-generation.json` immediately after migration, before
any rebuild is ever attempted — a workspace that boots, migrates, and is then inspected via
`knowledge_status` before any rebuild call must already show a known generation, which only
Mechanism 1 provides.

**An explicit repair tool/IPC method for FR-002.** Ruled out by FR-005 (no new IPC/MCP dispatch
method — this ships as a patch release) and by CLAUDE.md's `knowledge_*`/`ToolSpec` pairing rule,
which any such tool would require satisfying.

**Broadening the exemption to any `applied_seq.is_none()` or any `applied_seq` value.** Rejected:
`applied_seq > 0` means content has genuinely been applied and is exactly the case with something
at risk from a silently-substituted identity. The exemption is deliberately as narrow as the
"nothing at risk" property requires — `Some(0)` and an empty database — not broadened for
convenience.

## Consequences

- **ADR-0387's "no retroactive migration pass" and ADR-0414's "no lcg-side derivation of a
  substitute generation" no longer describe current behavior for these two specific, narrow
  cases.** Amendment notes were added to both documents pointing here. Every other case both ADRs
  describe is unchanged: a stream with real content and an unknown generation still refuses
  exactly as ADR-0414 designed, and lcg still never mints a generation for a directory it has no
  evidence of owning or emptiness proof for.
- **The narrow guard-bypass mint, in the rare case of a genuinely externally-stripped stream first
  touched at `applied_seq == Some(0)`/empty database, fabricates a locally-owned generation for
  it.** This is no worse than pre-ADR-0414 behavior for that one first-touch moment — nothing was
  at risk then either, since the database is empty by the exemption's own definition — and any
  subsequent real risk (the database becomes non-empty) is fully refused from then on, exactly as
  before this issue.
- **`knowledge_status`'s existing three-state `generation_status` classification
  (`not_applicable`/`unknown`/`known`) is unchanged in shape** — a stamped-by-migration or
  stamped-by-exemption group simply reports `"known"` instead of `"unknown"` going forward, a
  value change consistent with FR-005's "no schema change" constraint, not a new state or field.
- **No IPC/MCP schema, response shape, or dispatch method changes** (FR-005/SC-005) — both
  mechanisms operate entirely within existing response shapes; `handle_rebuild_from_wal`'s success
  path already reports `reset_detected`/`previous_generation`/`generation` for the Story-5-Scenario-3
  case Mechanism 2 reuses.
- **`docs/operations.md`'s "lcg never retroactively mints one into a directory it didn't create"
  claim is now qualified**, not simply corrected — it documents the two exceptions this issue adds
  and why each is narrow enough not to weaken the publish contract's guarantees for a genuinely
  external stream.
- **Cross-repo**: no change to the ADR-0387 publish/subscribe contract or to what an external,
  non-lcg publisher must write — both mechanisms apply only to a stream lcg has direct, first-hand
  evidence about (either that it just relocated the content itself, or that the database holds
  none of it yet).
