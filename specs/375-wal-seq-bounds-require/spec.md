# Feature Specification: Cheap WAL seq bounds — eliminate the full-directory scan in `wal_max_seq`/`wal_min_seq`

**Feature Branch**: `fabrik/issue-375`
**Created**: 2026-08-11
**Status**: Draft
**Input**: User description: "`wal_max_seq` and `wal_min_seq` both open and read every `.jsonl` file in the WAL directory. At the scale ADR-0026 documents (43,821 WAL files, seq 4,641,989), any call that needs a WAL seq bound performs tens of thousands of file opens. `knowledge_status` calls `wal_max_seq` on every invocation, and `knowledge_wal_mark_list` (#365) calls both `wal_min_seq` and `wal_max_seq` for its reachability check. Need a cheap bounds mechanism that still returns the true min/max, not a filename-sorted approximation."

## Background

`wal_max_seq` (`crates/core/src/wal.rs`) delegates to `scan_max_seq`, which lists every `.jsonl`
file in the WAL directory and calls `read_last_seq` on each (a bounded tail-read per file, not a
full read — but still one file open per WAL file). `wal_min_seq` (added on the `fabrik/issue-365`
branch / PR #367, not yet merged to `main` as of this writing) is the structural mirror —
`first_seq_in_file` per file, cheaper per file since the first line sits at offset 0, but the same
O(number of WAL files) open count.

Neither trusts filename sort order, and correctly so: `wal.rs`'s own tests pin the fact that the
lexicographically-first file can hold a higher (or lower) seq than a later-sorting file, under
clock skew or concurrent writers sharing a directory. So the full scan is not gratuitous — it is
the only correct answer given the current on-disk layout, which records no seq bounds anywhere
except inside the files themselves.

**Where it costs**: `knowledge_status` calls `wal_max_seq` on every invocation
(`handlers.rs`, `knowledge_status`'s WAL-fields assembly) — this is the larger exposure, since
`knowledge_status` is a routine, frequently-polled call. `knowledge_wal_mark_list` (#365) calls
both `wal_min_seq` and `wal_max_seq` for its reachability check — roughly 2× the file opens, on a
much rarer call path.

**Why this is being filed now**: #365's FR-009 set an explicit cost bound on the checkpoint
reachability check — it "MUST NOT make every `list` call scale with total WAL size" — on the
stated premise that "the maximum is already computed cheaply elsewhere via `wal_max_seq`; an
equivalently cheap minimum is expected." That premise was factually wrong: `wal_max_seq` was
never cheap in the sense FR-009 assumed. #365's implementation mirrored the existing (already
O(N)) behavior and documented it as an accepted cost class rather than flagging the unmet MUST
(see the review discussion on PR #367). This is not a defect introduced by #365; it is a
pre-existing cost that #365 surfaced and doubled on one path. Fixing it properly benefits
`knowledge_status` first and checkpoint reachability second.

**Sequencing note**: `wal_max_seq` exists on `main` today; `wal_min_seq` and
`knowledge_wal_mark_list` exist only on the `fabrik/issue-365` branch (PR #367, approved and
mergeable but not yet merged at spec time). This issue's Implement stage forks from `main`, so
whether `wal_min_seq` is available to optimize depends on merge order outside this issue's
control. The requirements below are written so the issue is fully actionable either way — see
FR-009 and Assumptions.

**Direction (not prescriptive — left to Research/Plan)**: a cheap bounds mechanism needs the WAL
directory to record its own seq range somewhere other than inside the line data. Options worth
weighing:

- A manifest file in the WAL directory (e.g. `<wal_dir>/.manifest.json`) maintained on rotation,
  recording per-file `{first_seq, last_seq}`. Must be rebuildable from a full scan when absent or
  stale, and must live somewhere invisible to the existing non-recursive `.jsonl` scans
  (`replay.rs`'s file lister, `wal.rs`'s `.jsonl` file counter, `wal.rs`'s `scan_max_seq`) — the
  same constraint `.checkpoints/` (#365) satisfies.
- Encoding seq bounds in WAL filenames at rotation time. Cheaper still (no file reads at all,
  just `read_dir`), but changes the filename contract and does not help WAL directories written by
  older versions or by the Python producer.
- Caching the bounds in memory, invalidated on write/rotation. Helps a long-lived process but not
  a cold `knowledge_status` call on a freshly-attached directory.

Whatever is chosen must preserve the correctness property the current implementation has and the
tests pin: the true min/max, not the filename-sorted first/last.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Fast `knowledge_status` at production WAL scale (Priority: P1)

An operator or automated poller calls `knowledge_status` against a workspace whose WAL directory
has accumulated tens of thousands of `.jsonl` files (the ADR-0026 scale). The call returns quickly
and its latency does not grow with the number of WAL files present.

**Why this priority**: `knowledge_status` is the routine, frequently-polled call this issue exists
to fix; it is the larger of the two cost exposures described in Background.

**Independent Test**: Against a WAL directory seeded with a large number of `.jsonl` files (e.g.
thousands), call `knowledge_status` twice in a row and confirm the second call's `wal.max_seq`
computation does not open every `.jsonl` file — verified by an I/O-count assertion in a test
double or fixture, not by wall-clock timing alone.

**Acceptance Scenarios**:

1. **Given** a WAL directory with N `.jsonl` files and a previously established fast-path bound
   for that directory, **When** `knowledge_status` is called, **Then** the reported
   `wal.max_seq` is computed without opening every `.jsonl` file, and its value equals the true
   highest seq present on disk.
2. **Given** a WAL directory with no fast-path metadata yet (first access, or one predating this
   feature), **When** `knowledge_status` is called, **Then** it still returns the correct
   `wal.max_seq` (falling back to today's full scan is acceptable for this one call).

---

### User Story 2 - Fast checkpoint reachability at production WAL scale (Priority: P2)

An operator calls `knowledge_wal_mark_list` against the same large WAL directory to see which
named checkpoints are currently reachable. The reachability check does not scale with WAL file
count, matching the cost bound #365's FR-009 already committed to.

**Why this priority**: lower priority than User Story 1 because `knowledge_wal_mark_list` is a
much rarer call path, and because it depends on `wal_min_seq` existing in the codebase (see
Assumptions) — a precondition outside this issue's control.

**Independent Test**: Against a WAL directory seeded with a large number of `.jsonl` files and at
least one recorded checkpoint, call `knowledge_wal_mark_list` and confirm its reachability check
does not open every `.jsonl` file.

**Acceptance Scenarios**:

1. **Given** `wal_min_seq` exists in the codebase and a WAL directory with an established
   fast-path bound, **When** `knowledge_wal_mark_list` is called, **Then** its reachability check
   computes both the min and max seq bound without opening every `.jsonl` file.

---

### Edge Cases

- WAL directory with no fast-path metadata established yet (first-ever access, or one that
  predates this feature): must still return the correct bound, at worst by falling back to
  today's full scan for that one call.
- Fast-path metadata present but stale, corrupt, or unparseable (e.g. truncated by a crash mid
  write): must fall back to a full scan rather than return a wrong bound, and should recover
  (re-establish valid metadata) rather than permanently degrade to full-scan-every-time.
- A WAL directory that is empty (no `.jsonl` files at all): both `wal_max_seq` and `wal_min_seq`
  continue to report "empty" (`None`/no bound), matching current behavior.
- A WAL directory modified by a process that does not participate in the new fast-path mechanism
  (the Python producer, or an older binary) between two calls: the next call must still return the
  true bound, not a value the outside modification invalidated silently.
- Concurrent writers sharing one WAL directory under clock skew, where the lexicographically-first
  or -last file by filename does not hold the true min/max seq: the existing `wal.rs` tests
  covering this must continue to pass unchanged — the new mechanism must not regress to trusting
  filename sort order.
- A single WAL file with a corrupt or truncated leading/trailing line: existing tolerance
  (skip the bad line, keep scanning) must be preserved by any fallback path.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: In the common case (a WAL directory whose fast-path bound is already established
  and has not been invalidated by an out-of-band write since), computing `wal_max_seq` MUST NOT
  open and read every `.jsonl` file in the WAL directory.
- **FR-002**: Both `wal_max_seq` and `wal_min_seq` (when `wal_min_seq` exists in the codebase —
  see FR-009) MUST continue to return the true minimum/maximum seq present on disk, never a
  filename-sort approximation. The existing `wal.rs` tests covering out-of-order filenames and
  corrupt leading/trailing files MUST continue to pass unmodified.
- **FR-003**: Any new on-disk metadata this issue introduces MUST be invisible to the existing
  non-recursive `.jsonl` directory scans (the replayer's file lister, the WAL file counter, and
  `scan_max_seq`) — i.e., it must not appear as a `.jsonl` file directly in the WAL directory root.
- **FR-004**: Any new on-disk metadata MUST be automatically rebuildable via a full directory scan
  when it is absent, stale, or fails to parse. It is an optimization layered over the existing
  scan, never a new source of truth that can diverge from the WAL content on disk.
- **FR-005**: Behavior — both correctness and the presence of a working fallback — MUST be
  unchanged for WAL directories that predate this mechanism (no metadata present at all). The
  first access after upgrade may pay today's full-scan cost once; it must not fail or return a
  wrong value.
- **FR-006**: Behavior MUST be unchanged for WAL directories written by the Python producer, which
  is not expected to create or maintain any new Rust-side metadata format.
- **FR-007**: `knowledge_status` MUST NOT scale with WAL file count in the common case — its
  `wal.max_seq` computation must not degrade proportionally to WAL file count once the fast path
  is established for that directory.
- **FR-008**: The mechanism MUST tolerate a WAL directory being modified by a process outside its
  own bookkeeping (another instance, an older binary, or the Python producer) — the next bound
  computation after such a modification MUST still return the true min/max, not a value made
  stale by the outside write. It is acceptable for such a modification to force one full-scan
  reconciliation, so long as the result is correct.
- **FR-009**: If `wal_min_seq` exists in the codebase at implementation time (i.e., #365/PR #367
  has merged to `main`), `knowledge_wal_mark_list`'s reachability check MUST also stop scaling
  with WAL file count in the common case, satisfying #365's original FR-009 cost bound. If
  `wal_min_seq` does not yet exist at implementation time, this requirement is deferred — this
  issue MUST NOT be blocked on implementing against code that does not exist yet, and a follow-up
  issue should extend the same mechanism to `wal_min_seq`/`knowledge_wal_mark_list` once #365
  lands.

### Key Entities

- **WAL seq bound**: the minimum or maximum WAL `seq` value present across all `.jsonl` files in a
  WAL directory, as currently returned by `wal_min_seq`/`wal_max_seq`.
- **Fast-path metadata** (mechanism TBD at Research/Plan): whatever on-disk or in-memory record
  this issue introduces to avoid re-deriving a seq bound from every WAL file on every call.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A `knowledge_status` call against a WAL directory whose fast-path bound is already
  established completes without opening any `.jsonl` file for the purpose of computing
  `wal.max_seq`.
- **SC-002**: (Conditional on `wal_min_seq` existing at implementation time, per FR-009.) A
  `knowledge_wal_mark_list` call against a WAL directory whose fast-path bound is already
  established completes its reachability check without opening every `.jsonl` file.
- **SC-003**: All existing `wal.rs` tests covering out-of-order filenames, clock skew, corrupt
  leading files, and truncated final lines continue to pass unchanged.
- **SC-004**: A freshly-attached WAL directory with no prior fast-path metadata, and a WAL
  directory written entirely by the Python producer, both return identical `wal_max_seq` /
  `wal_min_seq` values before and after this change.
- **SC-005**: `knowledge_status` latency against a WAL directory at the ADR-0026 production scale
  (tens of thousands of files) is dominated by a small, bounded number of I/O operations in
  steady state, not by a count proportional to the number of WAL files.

## Assumptions

- The exact mechanism (on-disk manifest, filename encoding, in-memory cache, or a combination) is
  a Research/Plan decision, not fixed by this spec — the three options listed under Background are
  starting points, not a prescribed choice.
- `wal_min_seq`/`knowledge_wal_mark_list` may or may not exist on `main` by the time this issue
  reaches Implement, depending on whether #365/PR #367 has merged first. FR-009 makes this
  issue's scope self-adjusting: fix `wal_max_seq`/`knowledge_status` unconditionally; extend the
  same fix to `wal_min_seq`/`knowledge_wal_mark_list` only if that code is present.
- "Common case" means no out-of-band modification of the WAL directory has occurred since the
  fast-path bound was last established for it — an out-of-band modification is allowed to cost one
  full-scan reconciliation, not treated as corruption.
- Scope is a single WAL directory / workspace at a time; no requirement to coordinate or share
  bounds across multiple WAL directories.

## Out of Scope

- Redesigning or relocating the `.checkpoints/` mechanism itself (#365's concern, not this issue's).
- Extending fast bounds computation to any consumer other than `wal_max_seq`/`wal_min_seq` and
  their existing callers (`knowledge_status`, `knowledge_wal_mark_list`).
- Making the Python producer aware of, or able to write, any new Rust-side metadata format.
- Mid-range gap detection in checkpoint reachability (already explicitly out of scope per #365's
  FR-009, which accepts a min/max bound check that doesn't detect a gap in the middle of the
  range).

## Source References

- `crates/core/src/wal.rs` — `wal_max_seq`, `scan_max_seq`, `read_last_seq` (present on `main`);
  `wal_min_seq`, `first_seq_in_file` (present on `fabrik/issue-365` / PR #367 only, as of this
  writing).
- `crates/core/src/handlers.rs` — `knowledge_status`'s WAL-fields assembly, which calls
  `wal_max_seq` on every invocation.
- `crates/core/src/replay.rs` — the WAL replayer's non-recursive `.jsonl` file lister, one of the
  three existing scans any new metadata file must stay invisible to.
- ADR-0026 — the ~43,821-file production scale this issue is measured against.
- ADR-0353 — background on why `wal_max_seq` is read fresh (not cached) on every `knowledge_status`
  call, and the existing tail-read mitigation.
- #365 / PR #367 — added `wal_min_seq`/`knowledge_wal_mark_list`; FR-009's cost bound and its
  incorrect premise about `wal_max_seq`'s existing cost.
