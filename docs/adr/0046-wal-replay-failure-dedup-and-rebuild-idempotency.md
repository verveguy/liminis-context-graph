# ADR-0046: WAL Replay — Deduplicated Failure Samples and Fail-Fast Rebuild Idempotency

**Status**: Accepted
**Date**: 2026-07-26
**Issue**: #239

## Context

Two further defects in the WAL replay pathway (`crates/core/src/replay.rs`,
`crates/core/src/handlers.rs`), immediately following ADR-0043's fix to the same file, made
failures hard to diagnose and made a routine rebuild look catastrophic:

1. **One bad template blinds the whole failure report.** `classify_replay_failure` pushed one
   `FailureSample` per failing row, capped by `sample_cap` (default 10), with no dedup key. When
   a batch's `prepare()` failed, every row in that batch was classified individually — arithmetically
   correct, since `prepare()` is a pure function of the template — but the sample cap was consumed
   per *row*, not per *template*. With a default batch size of 64, the first template that failed
   to prepare filled `failed_samples` with ten byte-identical entries, and every other distinct
   failure category became invisible in the result payload for the rest of the run. A schema gap
   on `Entity` MERGE plus an unrelated genuine bug on `MENTIONS` in the same WAL meant an operator
   saw ten copies of the `Entity` error and never learned the `MENTIONS` failures existed.

2. **`knowledge_rebuild_from_wal` is non-idempotent against a non-empty database.** The native
   write path emits `CREATE`, not `MERGE`, for `Entity`/`Episodic`/`RelatesToNode_` (`db.rs`).
   `handle_rebuild_from_wal` dropped indexes before replay but never cleared the database — unlike
   `recover_rebuild_from_workspace_wal`, which already deletes the database first. Invoking
   `knowledge_rebuild_from_wal` with the default `from_seq: 0` (a full rebuild) against a populated
   database therefore produced a duplicate-primary-key failure for every node `CREATE`: a large,
   benign-looking `failed_lines` count that tripped `fidelity_warning` and, via defect 1, flooded
   the sample buffer with duplicate-key noise, burying any real problem underneath it. This is the
   single most likely scenario for an operator to hit first, since it happens on any rebuild run
   without first clearing the database by hand — which the README already told users to do, but
   nothing enforced.

3. **`fidelity_warning`'s denominator silently zeroes out on an all-unrecognised WAL.** The ratio
   (as extended by ADR-0043) excluded `unrecognised_lines` and `unparseable_lines` from both sides
   entirely. A WAL that is 100% unrecognised (wrong directory, incompatible format) produced
   `lines_replayed: 0` and `failed_lines: 0`, so the denominator was 0, the `total > 0` guard
   short-circuited, and `mutations_replayed: 0` was reported with no warning at all —
   indistinguishable from "nothing to do."

## Decision

### Failure-sample deduplication: key on `(template, error)`, cap bounds categories

`FailureSample` gains a `count: u64` field (serialized) and a `#[serde(skip)] template: String`
field used only as the dedup key — never exposed in the JSON response, so the shape change is
additive. `classify_replay_failure` now scans the existing `failed_samples` (bounded by
`sample_cap`, small — default 10 — so a linear scan is cheap and consistent with the function's
existing non-allocation-heavy style) for a sample whose `(template, error)` matches the current
failure; on a match it increments `count` instead of pushing a duplicate, and only pushes a new
entry — subject to the cap — when no match is found. `failed_lines` keeps incrementing
unconditionally outside this logic, so the total failure count is untouched by dedup.

The dedup key is the **full, untruncated template string**, not the already-truncated 200-char
preview stored in `FailureSample.cypher` for display — the preview risked a false merge between
two distinct long templates sharing the same first 200 characters, and the full template was
already an owned `String` in scope in both `flush_batch` call sites at negligible extra cost.

Both call sites of `classify_replay_failure` — `flush_batch`'s batch-level `prepare()`-failure
loop and its per-row `execute()`-failure branches — share the same `stats.failed_samples` state,
so a template that fails to prepare (classified once per row in a tight loop) dedups exactly the
same way as a duplicate-key error recurring across many rows at execute time. The existing
`sample_cap_respected` test asserted the *opposite* of the new behavior (5 identical failures
capped at 3 stored samples, i.e. the cap capped rows) — that assertion was the exact defect this
fix corrects, so the test was rewritten, not preserved, to assert the cap bounds distinct
categories instead (5 distinct duplicate-key uuids, cap 3 → 3 stored samples each with an accurate
`count`).

As a byproduct, the existing minor inconsistency where `classify_replay_failure` computed a
whitespace-collapsed preview for its `eprintln!` but stored a *different*, non-collapsed
`template.chars().take(200)` in `FailureSample.cypher` was fixed by reusing the same
collapsed-preview computation for both.

### Fidelity ratio: `unrecognised_lines` and `unparseable_lines` join both sides

Following the exact symmetric-join pattern ADR-0043 established for `match_prefixed_no_op`,
`unrecognised_lines` and `unparseable_lines` are added to **both** `total` (the denominator) and
`ineffective` (the numerator). Adding them only to the denominator — a literal reading of "toward
the total considered" — would leave the ratio at a constant 0 and never trigger a warning,
defeating the requirement's own goal. `legacy_skipped_lines` stays excluded from both sides,
exactly as before — a benign, expected outcome that must not by itself push a healthy replay over
the warning threshold.

The ratio computation was extracted from inline logic in `replay_opts` into a standalone
`compute_fidelity_warning(stats: &ReplayStats, threshold: f64) -> Option<String>` function. This
makes the numerator/denominator composition directly unit-testable against hand-constructed
`ReplayStats` values (all fields are `pub`) without needing a real lbug database or WAL files —
in particular, `legacy_skipped_lines`'s exclusion from the ratio has been essentially untestable
via integration tests, since `LEGACY_SCHEMA_ERROR_PATTERNS` (the mechanism that would set it) is
currently an empty array (its former entries became dead once #144 added stub tables for the
constructs they matched) — the unit-level extraction closes that gap.

### Non-empty-database guard: fail-fast by default, opt-in `force_clear`

Two designs satisfy the spec's FR-005 (either auto-clear or fail-fast); this ADR picks **fail-fast
by default, with an opt-in `force_clear: true` parameter** that performs the clear. This is the
more conservative of the two: it never deletes graph data without an explicit request, it turns
the README's pre-existing "delete it and rebuild" guidance from unenforced advice into an actual
enforced precondition, and it keeps the blast radius smaller for `liminis-app` (a closed-source
Electron consumer of this exact IPC surface that could not be audited for this issue — see Risks
in the issue's Research stage) — a new, clearly-named error type is a safer thing for an
unaudited caller to absorb than a newly-automatic destructive database wipe.

The check is inserted exactly once in `handle_rebuild_from_wal`, immediately after the existing
`wal_dir`/`has_jsonl_files` check and before the streaming/non-streaming/background-job branch —
so it applies uniformly to all three call-site flavors and to both `dry_run` states without
triplicating logic (satisfying FR-010's "shared path, not a caller-specific branch"). It fires
only when `from_seq == 0`; `from_seq > 0` (incremental resume) is unaffected, since that path
intentionally targets a database that already has state.

The check queries `count_nodes` on exactly the three natively-`CREATE`d labels —
`Entity`, `Episodic`, `RelatesToNode_` — rather than treating any existing node as "non-empty".
`Community`/`Saga` stub tables (#144) use `MERGE` and would not collide with a rebuild, so
including them would produce false-positive blocks on legitimately mixed databases; the check is
scoped to the actual collision source named in the issue's Background.

`dry_run: true` **always** fails fast on a non-empty database, regardless of `force_clear` — a
dry run must never mutate the database, so "clearing" has no meaning in that mode, and the spec
explicitly frames dry-run as the primary way an operator previews a rebuild before committing to
one. Surfacing the problem there, rather than silently reporting a clean-looking preview that
would fail for real, is the entire point.

The clear path (reachable only when `force_clear: true` and `dry_run: false`) mirrors
`recover_rebuild_from_workspace_wal`'s existing precedent: delete only the lbug DB file/dir and
its `.wal`/`.lock` sidecars — **never** the application's `wal_dir` — reopen via `Db::open` +
`init_schema` + `rebuild_name_index`, then hot-swap the new `Db` into `state.db` (`ArcSwapOption`,
ADR-0003) and reset `state.indices_built` so the post-clear DB looks correctly "not yet indexed"
to every other handler. The whole operation runs under `state.write_lock.write()`.

## Consequences

- `FailureSample`'s JSON shape gains `count: u64` — additive, no removal or rename, so existing
  consumers that don't read the new field are unaffected.
- `knowledge_rebuild_from_wal` gains a new `force_clear: bool` (default `false`) parameter and a
  new failure mode: a `from_seq: 0` call against a non-empty database that previously "succeeded"
  with a flood of duplicate-key `failed_samples` now fails immediately with an explicit, actionable
  error instead. Any caller (including the un-auditable `liminis-app` consumer) that was silently
  tolerating the old duplicate-key-flood outcome as "success" will now see a new, distinctly-named
  error and must either pass `force_clear: true` or clear the database itself first via
  `knowledge_clear_all`.
- A WAL that is 100% unrecognised or unparseable now produces a non-`None` `fidelity_warning`
  where it previously produced a silent `mutations_replayed: 0` with no warning at all — any
  caller treating the absence of `fidelity_warning` as a strict success signal will now see a
  warning in this specific case, which is the intended fix for defect 3.
- **Residual clear/replay race window**: the non-empty check-and-clear is not held under the same
  lock as the replay that follows it in the streaming call-site path — the clear's own
  `write_lock` guard is released before that path re-acquires the lock at its existing call site.
  A concurrent writer could theoretically repopulate the database in this gap. This is a
  pre-existing class of race in `handle_rebuild_from_wal` (not introduced by this fix) and is out
  of scope per the issue's own "ordering/statistics propagation... tracked as separate issues"
  carve-out.
- The `compute_fidelity_warning` extraction is a small, low-risk internal refactor (pure function,
  no behavior change to callers) that made the FR-008/FR-009 test coverage substantially more
  direct — most notably, it lets `legacy_skipped_lines`'s ratio-exclusion be tested at all, since
  that path is currently unreachable through the real replay engine.

## Alternatives Considered

- **Auto-clear by default for FR-005** (no `force_clear` opt-in, clear unconditionally on a
  `from_seq: 0` rebuild against a non-empty database): rejected — a newly-automatic destructive
  action on a codepath consumed by an unaudited closed-source client is a bigger behavior change
  to absorb silently than a new, clearly-named error the caller must explicitly opt past.
- **Truncated 200-char `cypher` preview as the dedup key** instead of the full template: rejected
  — risks a false merge between two distinct long templates sharing a common prefix; the full
  template was already in scope as an owned `String` at both call sites, so there was no
  meaningful cost to using it instead.
- **Treating any existing node (including `Community`/`Saga`) as "non-empty" for the FR-005
  check**: rejected — those stub tables use `MERGE`, not `CREATE`, and would not actually collide
  with a rebuild; blocking on their presence would be a false positive against the issue's own
  named defect.
- **Applying the FR-005 check only to non-dry-run calls** (exempting `dry_run` entirely, since a
  dry run structurally cannot reproduce the duplicate-key error — it never executes Cypher):
  rejected — the issue's own Edge Cases explicitly call out that dry-run is the primary preview
  path, and silently letting a dry-run "succeed" against a database that would fail for real
  defeats the purpose of previewing.
- **Adding `unrecognised_lines`/`unparseable_lines` only to the fidelity ratio's denominator**:
  rejected — this reading of "toward the total considered" leaves the ratio permanently at 0 for
  an all-unrecognised WAL, which is precisely the silent-success outcome the fix exists to
  eliminate; both sides must move together, mirroring ADR-0043's own established pattern.

## References

- Issue #239 — this fix; continues the four-issue series auditing the WAL replay pathway
  (see ADR-0043 References)
- ADR-0043 — WAL Replay: Seq-Based File Ordering and MATCH-Write No-Op Accounting (the immediately
  preceding fix to the same file/functions; this ADR's fidelity-ratio symmetric-join pattern and
  `legacy_skipped_lines`/`match_delete_no_op` exclusions build directly on it)
- ADR-0003 — ArcSwap DB Hot-Swap (the `state.db.store()` pattern the FR-005 clear path follows)
- ADR-0002 — Reader/Writer Split (`state.write_lock` semantics the clear path holds during its
  delete-and-reopen sequence)
- ADR-0026 — Episode-Cursor WAL Resume (precedent for why `match_delete_no_op` — and now, by the
  same reasoning, `legacy_skipped_lines` — stay excluded from the fidelity ratio)
- Issues #128/#129 — introduced `fidelity_warning` and `legacy_skipped_lines` after an
  84.5%-data-loss incident; this ADR's fidelity-ratio fix extends that same mechanism
- Issue #144 — added `Community`/`Saga`/etc. stub tables using `MERGE`, which is why the FR-005
  non-empty check is scoped to only the three natively-`CREATE`d labels
