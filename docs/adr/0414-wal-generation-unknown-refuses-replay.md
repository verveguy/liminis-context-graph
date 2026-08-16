# ADR-0414: Unknown-Generation Streams Refuse to Advance, Not Warn

**Status**: Accepted
**Date**: 2026-08-16
**Issue**: #414
**Relates to**: ADR-0387 (WAL stream generation identity), ADR-0378 (multi-stream WAL per group
directory), ADR-0375 (`wal_max_seq` bounds manifest)

## Context

ADR-0387 gave each WAL stream a stable identity (`.wal-generation.json`) independent of its `seq`
numbering, specifically so `knowledge_rebuild_from_wal` could tell a genuine producer-side reset
apart from ordinary forward progress. Its `position_reset_detected` rule deliberately treats an
unknown current generation (missing or corrupt `.wal-generation.json`) as **never** a mismatch —
by design, a stream that has no generation was meant to keep booting and replaying normally
forever, tolerated indefinitely (Story 5, Scenarios 1-2 in ADR-0387).

In real-world hydrated channels this made detection **inert end-to-end**. Two real channels hydrated from published
WAL repos both reported `generation: null` for every group, and diagnosis traced this to a root
cause outside lcg's minting logic (`WalWriter::new`'s `global_seq == 0` guard mints correctly,
verified against the released binary): the publish step copied `*.jsonl` files only. A shell glob
does not match a leading dot, so `git add wal/*`/`cp wal/*.jsonl`/`rsync --include='*.jsonl'` all
silently drop `.wal-generation.json` (and the rest of the stream's dot-namespace) while appearing
to publish the complete stream.

The consequence was not merely "detection doesn't help yet" — it was actively dangerous. A
downstream consumer's own reset-detection logic (comparing generations to decide whether to force
a clean rehydrate) had been comparing `null` against `null` since it shipped, and had therefore
never once fired, through a full two-channel hydrate. Any producer-side rebuild or force-push that
had already occurred against a tracked channel would have been silently applied as an ordinary
forward advance, with no error surfaced at either end — precisely the corruption class ADR-0387
was written to close. Nothing about this was visible without someone happening to read a status
field.

## Decision

Once a `WalPosition` has already been recorded for a group (`applied_seq.is_some()`), a subsequent
`knowledge_rebuild_from_wal` call against that group **refuses outright** if the group's current
on-disk generation is unknown — missing or corrupt `.wal-generation.json`, collapsed
indistinguishably per ADR-0387's existing `read_generation` (a damaged sidecar must never
masquerade as a detected reset; this ADR does not change that).

- The call fails with a dedicated `Error::WalGenerationUnknown`, naming the group and stating that
  `.wal-generation.json` is absent or unreadable, and pointing at the stream-publish contract
  (`docs/operations.md`) as the fix.
- Replay does not proceed in any form: `from_seq`/`to_seq`/`force_clear` are not applied, and this
  applies uniformly to `dry_run: true` as well — there is nothing safe to preview when reset
  detection itself could not run.
- No configuration flag, environment variable, or request parameter bypasses the check. This was
  explicitly considered and rejected (see Rejected Alternatives).
- The refusal is scoped to the affected group only. A sibling group sharing the same WAL root
  whose own generation is known remains independently replayable, in the same or a later call.
- A group's **first** encounter (`applied_seq: None`, no row recorded yet) is unaffected: it
  performs ordinary first-time adoption, including adopting an unknown generation, exactly as
  ADR-0387 originally designed. This decision narrows ADR-0387's tolerance for exactly one case —
  a recorded position whose stream has since become (or always was, on a second look)
  generation-unknown — not the first-adoption case.

`knowledge_status` gains a companion, non-behavioral change: a `generation_status` field
(`"not_applicable"` / `"unknown"` / `"known"`) alongside the existing `generation` value, so an
operator can observe the condition before it produces a rebuild failure. This is pure
classification of data `knowledge_status` already computes (`max_seq`, `generation`) — no new
filesystem I/O, no change to what `generation` itself reports.

## Rejected Alternatives

**Warn-and-proceed, with a strict-mode opt-in flag for callers that want the hard failure.**
Rejected because the flag would outlive its justification by construction: the only reason to
tolerate a generation-less stream at all is the two currently-hydrated dev/test channels, and
those get republished — regaining a real generation — as soon as their publish step is fixed,
which they must do regardless of what lcg does here (a stream without a generation cannot
participate in reset detection no matter how tolerant the consumer is). A compatibility switch
added now would be permanent surface for a window that closes in days, and it would default to
the unsafe behavior for exactly as long as anyone forgot to flip it.

There is a sharper reason than surface-area hygiene, though: **a warning is the same category of
signal that already failed here.** The downstream consumer's null-vs-null generation compare was,
functionally, a silent warning — a condition nobody was watching for, that never fired despite
being wrong the entire time. Reproducing that pattern with a louder log line does not fix the
underlying failure mode; only a hard stop that a caller cannot fail to notice does.

**Continue tolerating an unknown generation indefinitely, rely on `knowledge_status`'s new
`generation_status` field alone to make the condition visible.** Rejected because visibility is
necessary but not sufficient — nothing compels an operator or an automated pipeline to check
`knowledge_status` before calling `knowledge_rebuild_from_wal`, and the two real production
channels this issue reproduces against were hydrated and re-hydrated repeatedly without anyone
reading that field. The refusal is what makes the condition impossible to silently drive past, not
merely possible to notice.

## Consequences

- **Every currently-hydrated stream lacking a generation stops replaying once a position has been
  recorded for it — intended, not collateral.** Those streams genuinely lack the identity that
  makes a subsequent replay's reset detection meaningful. There are no production consumers of
  this WAL stream format today (everything hydrating lcg streams is dev/test), so the cost is
  borne entirely by dev/test users who must fix their publish step regardless.
- **This can trip on what looks like "the first rebuild call ever made."** `knowledge_status`'s
  own one-time backfill (`recovery::backfill_applied_seq_if_absent`) can record a `WalPosition`
  row for a group before `knowledge_rebuild_from_wal` is ever explicitly called against it — an
  operator polling `knowledge_status` (a routine health-check pattern) satisfies this decision's
  "position already recorded" precondition on its own. This is the intended, desired behavior for
  the real production repros this issue reproduces, not an edge case to special-case around.
- **ADR-0387's Story 5 Scenarios 1-2 no longer describe current behavior** for a group with a
  recorded position — see the amendment note added to ADR-0387 itself. Story 5 Scenario 3 (a
  stream that gains a generation for the first time after a genuine reset) is unaffected: this
  decision only changes what happens while the current generation stays unknown, not the
  comparison once it becomes known.
- **No new minting or derivation path.** This decision changes only how an already-unknown
  generation is *handled* at replay time; it does not touch `WalWriter::new`'s minting guard or
  introduce any way for lcg to fabricate an identity for a stream it did not itself observe from
  empty (ADR-0387's Out of Scope, unchanged).
- **Cross-repo**: the actual fix for the two reproduction channels is a publish-step change (copy
  the whole stream directory, dot-namespace included) outside this repository — this repository's
  deliverable is the refusal itself plus the documented contract
  (`docs/operations.md`), not the cross-repo implementation.
