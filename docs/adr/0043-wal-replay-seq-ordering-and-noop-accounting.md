# ADR-0043: WAL Replay — Seq-Based File Ordering and MATCH-Write No-Op Accounting

**Status**: Accepted
**Date**: 2026-07-25
**Issue**: #237

## Context

Two independent defects in `WalReplayer::replay_opts` (`crates/core/src/replay.rs`) allowed WAL
replay to silently lose data, with no error, no counter, and no log an operator would notice:

1. **File replay order was derived from full-filename lexicographic comparison**
   (`files.sort_by(|a, b| a.file_name().cmp(&b.file_name()))`). WAL filenames are
   `YYYYMMDD_HHMMSS_<session-id>_<file_seq:04>.jsonl` (`wal.rs`'s `make_new_file_path`), where
   `<session-id>` is a random 6-hex-char string generated fresh by every `WalWriter::new` call.
   Two files written by different sessions within the same wall-clock second — a crash-loop, a
   fast `knowledge_recover`, or an NTP step backwards — sort by their random session id ahead of
   their true write order, since the timestamp prefix ties. Nothing cross-checked each line's
   `seq` field for monotonicity across files; `seq` was used only for the `from_seq` resume
   filter.
2. **A `MATCH ... SET`/`MATCH ... DETACH DELETE` that matched zero rows was counted as a
   success.** `execute_prepared` returns `Ok` for a zero-row match (bare writes with no `RETURN`
   report `get_num_tuples() == 0` regardless of whether the `MATCH` found anything), so
   `stats.lines_replayed` incremented unconditionally. An out-of-order write (case 1) landing on
   a not-yet-created target, or any other legitimate zero-row match, was indistinguishable from
   an effective write in the returned stats.

Separately, `Db::open_or_rebuild` — the library's fresh-install/rebuild entry point — discarded
the `ReplayStats` returned by `WalReplayer::replay` entirely, so a schema gap that failed 100% of
a mutation category returned `Ok(())` with zero observable signal. That defect (FR-001/FR-002)
is local to `db.rs` and is not the subject of this ADR beyond noting it shares the same
underlying stats plumbing this ADR extends.

### Why `file_seq` cannot be the ordering key

The filename's `file_seq` component looked like an obvious ordering key, but it is **per-session**
— `WalWriter::file_seq` starts at 0 in every `WalWriter::new` call, so two files from different
sessions can carry an identical `file_seq` value, and `file_seq` alone cannot establish relative
order *between* sessions. Sorting by parsed `file_seq` would silently reintroduce this same
ordering defect in a different filename column. The only value that is globally monotonic across
sessions is each WAL line's `seq` field: `WalWriter::new` reseeds its `global_seq` counter via
`scan_max_seq`, which takes the max `seq` across every existing file regardless of that function's
own (filename-based) iteration order — a property confirmed during specification and left
unchanged by this fix (see the issue's Assumptions and Out of Scope).

## Decision

### File ordering: sort by each file's first-line `seq`

`replay_opts` now reads each candidate file's first non-empty, parseable line and uses its `seq`
value as the sort key (`first_seq_in_file`). Files whose first-line `seq` can't be determined
(unreadable file, empty file, or an unparseable first line) sort **after** all determinate files,
grouped by filename among themselves, with a `[WAL WARN]` log line — an honestly-approximate
placement, not a claim of correctness.

This does mean each file's first line is read twice (once to compute the sort key, once during
the main replay pass) — a small, bounded I/O cost (one line, not a full-file scan) that is not a
concern at realistic WAL-directory sizes.

### Seq monotonicity check: belt-and-suspenders, not redundant

Because the file-ordering fix above is a *heuristic* (first-line `seq`, with a filename fallback
for indeterminate files), `replay_opts` also tracks a running `max_seq_seen` across the whole
call and checks every line's `seq` against it, in processing order — independent of which file it
came from. A regression (`seq <= max_seen`) increments a new `ReplayStats::seq_regressions`
counter and logs `[WAL WARN] seq regression: ...`, but the mutation still proceeds into the
normal execution path. Refusing to apply it would convert a rare ordering-heuristic miss into new
data loss — exactly the failure mode this issue exists to eliminate. Under normal operation `seq`
is a single global monotonic counter, so a regression should only ever fire when the file-ordering
heuristic guessed wrong (e.g. an indeterminate-file fallback) — the two mechanisms are
complementary: the sort tries to get the order right, and the monotonicity check catches it
when the sort's best-effort guess is wrong, rather than silently trusting it.

`max_seq_seen` is local to one `replay_opts` call, so it does not fire during `recovery.rs`'s
intentional overlapping WAL-tail-resume replay (ADR-0026) — that resume starts a fresh call with
its own local counter, and the overlap it re-applies is itself a distinct, already-accounted-for
kind of no-op (see below).

### No-op detection: `RETURN count(*)` probe, not a widened `execute_prepared`

The natural-seeming fix — widen `execute_prepared`'s return type to expose
`lbug::QueryResult::get_num_tuples()` — does not work: a bare `MATCH ... SET`/`DETACH DELETE`
with no `RETURN` clause always reports 0 tuples, whether or not the `MATCH` found rows. There is
no queryable signal on the un-modified template.

The fix rewrites the template — only for batches whose rows are `MATCH`-prefixed — to append
`RETURN count(*)` (`with_match_count_probe`), then reads the single resulting row via a new
`Conn::execute_prepared_returning_count`. Verified empirically against lbug 0.17: an existing
match returns `[1]` (well, the true match count), a non-match returns `[0]`, and the mutation
still applies correctly either way — `count(*)` needs no variable name, so the rewrite is
syntactically uniform across every current `MATCH`-prefixed template in the codebase.

This is scoped entirely to `flush_batch`'s replay path. `execute_prepared`'s existing signature —
used by the live-write hot path (`Conn::exec_params`) — is untouched, so this change carries no
risk to production writes outside of replay.

A batch only ever contains rows sharing one post-normalization template, and `is_match_prefixed`
is derived purely from that template's shape — so checking the first row's flag characterizes
the whole batch exactly, not approximately.

`execute_prepared_returning_count` falls back to `1` (i.e. "treat as matched") if the probe's
result shape isn't a recognized numeric type — failing toward *not* inflating the no-op count
rather than toward silently losing writes, in case a future lbug version changes `count(*)`'s
result shape.

### Fidelity ratio: `match_prefixed_no_op` joins both sides

`ReplayStats::fidelity_warning` already computed `failed_lines / (lines_replayed +
failed_lines)`. Since the no-op fix above *removes* no-ops from `lines_replayed` (they used to be
folded in), they must also join the denominator, or the ratio would never reflect them at all —
defeating the purpose of surfacing them. The updated computation is:

```text
total       = lines_replayed + failed_lines + match_prefixed_no_op
ineffective = failed_lines + match_prefixed_no_op
ratio       = ineffective / total
```

`legacy_skipped_lines` stays excluded from both sides, exactly as before this fix.

### `match_prefixed_no_op` vs. ADR-0026's resume-overlap no-op — deliberately distinct

`recovery.rs`'s WAL-tail-resume replay intentionally re-applies an overlapping `seq` range on
every startup recovery (ADR-0026); it treats that overlap as idempotent because `MERGE`s are
no-ops and `CREATE`-form statements collide harmlessly. Neither of those statement forms is
`MATCH`-prefixed, so ADR-0026's expected, harmless overlap never touches
`match_prefixed_no_op` — this counter is reserved for the data-loss scenario this issue targets
(an out-of-order or otherwise unintended zero-row `MATCH`-write), not for the resume path's
by-design idempotent replay. A future reader extending either mechanism should keep this
distinction in test naming/comments rather than conflating the two kinds of "no-op."

## Consequences

- `WalReplayer::replay`/`replay_opts` — the single implementation shared by `open_or_rebuild`,
  `knowledge_rebuild_from_wal`, `recovery.rs`'s WAL-tail-resume, and `handlers.rs`'s
  `recover_rebuild_from_workspace_wal` — gets both fixes automatically at every call site; no
  call-site changes were needed beyond `open_or_rebuild`'s own stats-plumbing fix (FR-001/002).
- `ReplayStats` gained two fields (`match_prefixed_no_op`, `seq_regressions`); existing callers
  that construct or read this struct only additively are unaffected. Neither field is threaded
  into `TelemetryEvent::WalReplayComplete` or the ad hoc JSON responses in `handlers.rs` — this
  was a deliberate scoping decision (see the Plan stage discussion on issue #237); a future issue
  can wire them through if an operator-facing need arises.
- Each file's first line is now read twice during replay (sort key + main pass) — negligible at
  realistic WAL-directory scale, but a future perf-sensitive change to replay's I/O pattern should
  account for this.
- A future contributor adding a new `MATCH`-prefixed write template must not assume it already
  ends in `RETURN` — `with_match_count_probe` appends `RETURN count(*)` unconditionally to every
  `MATCH`-prefixed template's batch (checked against all current call sites at the time of this
  fix: `db.rs`, `canonicalize.rs`, `episode.rs`, `reprocess_relations.rs`; none end in `RETURN`).
  If one ever did, the worst case is a `prepare()` failure classified as `failed_lines` — an
  existing, visible category, not a silent regression.

## Alternatives Considered

- **Sort files by parsed `file_seq` from the filename**: rejected — `file_seq` resets to 0 on
  every new `WalWriter` session, so it cannot order files written by different sessions. Using it
  would silently reintroduce this exact defect in a different filename column.
- **Widen `execute_prepared`'s return type to expose `get_num_tuples()` everywhere**: rejected —
  empirically, a bare `MATCH ... SET`/`DETACH DELETE` with no `RETURN` always reports 0 tuples
  regardless of match outcome, so this signal doesn't exist without a template rewrite. It would
  also touch the live-write hot path (`exec_params`) for no benefit, since that path doesn't need
  the no-op signal.
- **Refuse to apply a mutation once a `seq` regression is detected**: rejected — under normal
  operation a regression only fires when the file-ordering heuristic guessed wrong on an
  indeterminate file; refusing the write would convert a rare heuristic miss into guaranteed data
  loss, which is worse than the rare-miss risk itself.
- **Thread the new counters into `TelemetryEvent::WalReplayComplete` and the JSON IPC
  responses**: deferred as out of scope for this issue — nothing in the functional requirements
  required it, and it would have touched three additional call sites beyond what this fix needed.

## References

- Issue #237 — this fix; first in a four-issue series auditing the WAL replay pathway
- ADR-0023 — Legacy-WAL Translation Layer (the `strip_vecf32`/`expand_bulk_property_set`
  normalization this replayer already performs ahead of batching)
- ADR-0024 — Bound-Parameter DB Access (the prepare-once/execute-many pattern `flush_batch`
  builds on)
- ADR-0026 — Episode-Cursor WAL Resume (the intentional, idempotent replay-overlap this ADR's
  no-op counter is explicitly designed not to conflate with)
- ADR-0027 — Autonomous WAL-Corruption Self-Recovery on Startup (one of the four call sites this
  fix's ordering/no-op behavior propagates to automatically)
- Issues #128/#129 — introduced `fidelity_warning` and `legacy_skipped_lines` after an
  84.5%-data-loss incident; this ADR extends that same fidelity mechanism
- Issue #139 — introduced the batched-UNWIND/prepare-once replay design (`flush_batch`) this
  ADR's no-op detection is scoped inside
