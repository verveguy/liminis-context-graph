# ADR-0375: WAL Seq Bounds Manifest

**Status**: Accepted
**Date**: 2026-08-12 (extended to `wal_min_seq` same day, once #367 merged — see "Extension" below)
**Issue**: #375
**Relates to**: ADR-0353 (persist and expose `applied_seq`/`max_seq`), ADR-0026 (episode-cursor
WAL resume), #365/PR #367 (`wal_min_seq`/`knowledge_wal_mark_list`)

## Context

`wal_max_seq` (`crates/core/src/wal.rs`) opens and reads (a bounded tail read, not a full read,
per ADR-0353) every `.jsonl` file in the WAL directory on every call. `knowledge_status` calls it
on every invocation. At the production scale ADR-0026 documents (~43,821 WAL files), that's tens
of thousands of file opens for a routine, frequently-polled call.

ADR-0353 considered and explicitly rejected caching this value: *"`max_seq` is read fresh on
every `knowledge_status` call, not cached — caching would silently miss exactly the
external-WAL-write case #351 exists to detect (another process publishing new WAL content between
status calls)."* That concern is real — the orac/zen deployment model shares one WAL directory,
published by another process, across nodes — and any design here has to honor it: a stale
`max_seq` would corrupt the `applied_seq == max_seq` "caught up" check ADR-0353 built the whole
feature around.

This ADR revises the *mechanism* ADR-0353 chose, not the *guarantee* it was protecting.
`wal_max_seq` must still always return the true value; it just no longer has to re-derive it from
scratch on every call.

## Decision

### A manifest sidecar, not an in-process cache

Add `<wal_dir>/.wal-bounds.json`, a small JSON file recording:

```rust
struct WalBoundsManifest {
    file_set_fingerprint: u64,     // hash of the sorted .jsonl file *name set* at write time
    max_seq_file: Option<String>,  // file name (not path) holding max_seq
    max_seq_file_size: u64,    // that file's byte size at manifest-write time
    max_seq: Option<u64>,      // the literal highest seq present
    min_seq_file: Option<String>,  // file name (not path) holding min_seq
    min_seq_file_size: u64,    // that file's byte size at manifest-write time
    min_seq: Option<u64>,      // the literal lowest seq present
}
```

(`min_seq_file`/`min_seq`/`min_seq_file_size` were added same-day, once #367 merged — see
"Extension" below.)

Rejected an `AppState`-resident in-memory cache instead: it would reintroduce exactly the
divergence ADR-0353 rejected for a live, long-running process (nothing invalidates it when another
process writes to the same directory), and it wouldn't help a cold `knowledge_status` call against
a freshly-attached directory anyway. A disk-resident manifest, keyed to the directory it describes
and paired with a cheap staleness check (below), is observable — and correctable — by any process
that opens the directory, not just the one that wrote it.

The `.json` extension (not `.jsonl`) keeps it invisible to every existing non-recursive scan
(FR-003): `replay.rs`'s file lister, `count_jsonl_files`, and the full-scan helper all filter on
exact extension `"jsonl"`.

### Staleness detection: a file-name-set fingerprint + one file's size, not directory mtime

A fast-path read is trusted only if:

1. **The current `.jsonl` file *name set* still hashes to the manifest's `file_set_fingerprint`**
   (one `read_dir` pass — the same cost as a plain count — no file opens). A mismatch means a file
   was added, removed, or renamed since the manifest was written — rotation, an external writer, or
   a foreign/older process touching the directory — and any cached bound can no longer be trusted.
   A plain file *count* was considered and rejected here: it cannot distinguish "nothing changed"
   from a same-count swap (one file removed, a different one added), which would leave a stale
   `max_seq` cached even though the true max may have moved to the newly added file. Hashing the
   sorted name set costs the same one `read_dir` pass and closes that gap entirely.
2. **If the fingerprint matches**, stat the one file the manifest says held `max_seq` and compare
   byte size. Every other `.jsonl` file in the directory is immutable once rotated away; the *only*
   file that can grow without changing the file name set is the one a live writer still has open.
   An unchanged size means nothing relevant changed; a changed size means that file grew, and its
   tail alone (not the whole directory) needs re-reading. The recorded size is captured
   immediately after the read that produced the cached seq — inside the scan loop, not after it
   finishes visiting every file — so a live writer appending to that same file later in a large
   scan can't inflate the recorded size beyond what the cached seq actually reflects; capturing it
   post-loop would have silently poisoned this very invariant.

Directory `mtime` was considered and rejected: writing the manifest *into* `wal_dir` bumps the
directory's own mtime, so an mtime comparison captured before that write would make the cache
self-invalidate on the very next call — permanently paying the "warm" cost of a stat plus
immediately falling back to a full scan anyway.

### Maintenance: lazy, read-side, best-effort

The manifest is read and (re)written entirely inside `wal_max_seq`, not on `WalWriter`'s
write/rotation path (`flush_pending`, `rotate`). This keeps ingest completely untouched — no new
failure mode on the hot write path — consistent with FR-004 treating this as an optimization layered
over the existing scan, never a new source of truth. `wal_max_seq` callers (chiefly
`knowledge_status`) naturally keep a directory's manifest warm as a side effect of calling it.

A write is best-effort and non-fatal: `write_wal_bounds_manifest` swallows I/O errors after
serialization succeeds. The tmp file used for the atomic rename is UUID-suffixed per write (not a
single fixed name, unlike `ontology_sidecar.rs`'s startup-time writer), because `wal_max_seq` can
be called concurrently by multiple async tasks in one process — a fixed tmp name would let
concurrent writers stomp each other's in-flight write. Because a UUID-suffixed name is never
reused, a crash between `fs::write` and `fs::rename` would otherwise orphan that tmp file forever;
`write_wal_bounds_manifest` cleans up its own tmp file on a failed write or rename, and sweeps any
`.wal-bounds.*.tmp` sibling older than five minutes — but only from the full-scan fallback path,
not from the growth-reconciliation write inside `wal_max_seq_fast_path`. The sweep itself is a
`read_dir` pass; the full-scan path is already paying a whole-directory cost, so the sweep is free
there, but the reconciliation write runs on every call during active ingestion and must not double
the directory-listing cost this issue exists to bound. Crash debris accumulates at most one file
per crash, so skipping the sweep on the far more frequent reconciliation write doesn't let it grow
unbounded — the next full-scan fallback (e.g. after any rotation) sweeps it.

### Treating the manifest itself as untrusted input

Since the manifest tolerates being hand-edited, corrupted, or produced by a foreign writer (that's
what "any parse failure falls back" means in practice), the fast path validates it defensively
before acting on it, rather than trusting its shape:

- **`max_seq_file`/`min_seq_file` are validated as exactly one `Component::Normal` with a
  `.jsonl` extension** before being joined onto `wal_dir`: the value's `components()` must yield a
  single `Normal` component that round-trips to the original string. This rejects separators,
  `..`/`.`, roots, and — critically — a Windows drive prefix without a root (`C:evil.jsonl`),
  which contains no separator yet makes `Path::join` replace the base path outright. A
  separator-and-`..` check alone would accept it. An unvalidated join would let a corrupted or
  crafted manifest value escape the WAL directory (e.g. `../../etc/passwd`).
- **A `read_dir` failure while computing the current file-set fingerprint is treated as "can't
  confirm freshness," not as an empty directory.** Coercing the error into an empty result could
  spuriously match an empty-WAL manifest's fingerprint and serve a stale cached value straight
  past a real I/O error.
- **A re-read of the tracked max-seq file that reports a seq *lower* than the cached value is
  rejected**, falling back to a full scan instead. The tracked file only grows via ordinary
  append, so a decrease means something other than routine growth happened (e.g. truncation) and
  the manifest can no longer be trusted at face value. The min-seq side carries the equivalent
  defense via a size check, not a re-read — see "Extension" below.

Any miss, mismatch, or parse failure falls back to a full scan (`scan_wal_bounds_detailed` — see
the Extension section), which both computes the correct answer and produces a fresh manifest to
serve subsequent calls — so a corrupt or absent manifest self-heals on the very next call rather
than degrading permanently to full-scan-every-time (the corrupt-manifest edge case in the spec).

### Rejected: reusing `WalWriter.global_seq`

`WalWriter` already tracks `global_seq` in memory, seeded via a full scan at construction. It was
considered as a shortcut for a live process's own writer, but rejected: `log_mutation` increments
`global_seq` before its enclosing chunk commits, and a rolled-back chunk does not lower it back
(issue #352's fix intentionally never lowers `global_seq`) — so immediately after a rollback,
`global_seq - 1` can exceed the true max seq actually persisted to disk. `wal_max_seq` is also
deliberately decoupled from `AppState`/`WalWriter` today (it's a free function taking `&Path`);
coupling it to a live writer instance would be a bigger, riskier change than this issue calls for,
and would only help callers that happen to hold one.

## Accepted limitation

Two independent, long-lived `WalWriter` processes truly sharing one physical WAL directory, where
one process's file keeps growing between another process's calls without ever rotating, could have
that growth missed by the other process's cached read until that file's next rotation (bounded by
`max_events_per_file`/`max_bytes_per_file`, not unbounded). This is judged acceptable because it
doesn't match the deployment topology ADR-0353's own Context describes: orac/zen's shared WAL is
*distributed and git-published* — synced files arrive complete and immutable, not appended to via
live, concurrent, byte-level writes into one shared directory from multiple processes. It is also
consistent with this issue's own scope note: no requirement to coordinate bounds across multiple
WAL directories, and FR-008 explicitly accepts "one full-scan reconciliation" per out-of-band
change as the correctness mechanism, which this design provides for every case that changes the
`.jsonl` file name set (which any rotation, addition, or removal does).

An earlier draft of this design used a plain file *count* rather than a name-set fingerprint for
staleness detection, which left a gap: a same-count swap (one file removed, a different one added,
net count unchanged) would not have been detected, silently serving a stale `max_seq` if the
removed/added pair didn't touch the tracked max-seq file. Hashing the sorted file name set instead
(see Staleness detection, above) closes this at no extra cost — same single `read_dir` pass — so
this is not a residual limitation.

## Consequences

- **`wal_max_seq`'s signature and return semantics are unchanged** (`Result<Option<u64>, Error>`,
  the literal highest seq present, `None` for an empty/absent WAL directory) — no caller
  (`knowledge_status`) needs to change.
- **New on-disk artifact per WAL directory**: `.wal-bounds.json`. Read-only mounts, or directories
  a process can observe but never write to, simply never benefit from the fast path — every call
  falls back to a full scan, which is a correctness-preserving degradation, not a failure.
- **`docs/operations.md`'s `wal.max_seq` paragraph is updated** to describe the fast-path-with-
  reconciliation mechanism, while preserving the behavioral guarantee ADR-0353 established: the
  true value, with an externally-updated WAL observed on the next call (now: at worst after one
  reconciling full scan, not unconditionally on every call).
- **`wal_min_seq` and `knowledge_wal_mark_list`'s reachability check are in scope as of the
  Extension below** — both now share this same manifest and fast path. `checkpoint.rs::list`
  needed no code changes at all: it already called the `wal_min_seq`/`wal_max_seq` free functions
  directly, so extending those functions' internals was sufficient to satisfy FR-009's original
  cost bound.

## Extension: `wal_min_seq` (same day, once #367 merged)

This issue's Implement stage forked before #365/PR #367 merged to `main`, so `wal_min_seq` did not
exist yet and FR-009 was deferred per the spec's self-adjusting scope (see Sequencing note in the
issue body). #367 merged shortly after this ADR's initial Accepted state, making the deferred half
addressable; this section documents that follow-up rather than filing a second ADR for what is the
same mechanism applied to the second bound.

**The manifest gained two fields**: `min_seq_file` and `min_seq`, described above. Establishing
either bound via a full-scan fallback now populates *both* — `scan_wal_bounds_detailed` reads
every file's head (`first_seq_in_file`) and tail (`read_last_seq`) in the same pass, replacing the
narrower `scan_max_seq_detailed` `wal_max_seq`'s fallback used to call directly. A call to
`wal_min_seq` after `wal_max_seq` has already established the manifest (or vice versa) hits the
fast path immediately, with no second full scan — and the reverse direction holds too.

`scan_max_seq_detailed` itself was kept, unchanged in purpose, but narrowed to return a plain
`Option<u64>`: it now serves only `WalWriter::new`/`resync_global_seq`, hot paths that have never
needed `min_seq` and shouldn't pay `first_seq_in_file`'s extra per-file read to get it. Only the
manifest-fallback path (`wal_max_seq`/`wal_min_seq`) pays the doubled per-file I/O of computing
both bounds together, and only on a fallback — not on every call.

**`min_seq_file_size` exists for shrink detection only, not growth detection like `max_seq_file`'s
size check.** A WAL file's first line is fixed the moment it's written, and ordinary growth (the
only thing that can legitimately happen to an already-scanned, already-rotated file) only ever
*appends* to its end, which cannot change its own first line — so growth in `min_seq_file` never
needs a re-read, unlike `max_seq_file`. A same-or-larger current size is trusted without a stat
even mattering beyond the comparison itself: `wal_min_seq_fast_path` still pays one `stat` per
call (cheaper than `wal_max_seq`'s occasional single-file *re-read*, but not free), specifically to
catch the one case ordinary growth can't produce — a *smaller* current size than recorded, meaning
the file was truncated or replaced since it was scanned, which can change its first line and must
force a full-scan fallback rather than keep trusting the cached `min_seq`. An earlier version of
this design omitted this stat entirely, reasoning that "only append growth can happen" — but that
is exactly the assumption `wal_max_seq_fast_path` already treats as untrustworthy enough to
actively defend against (via its own monotonicity check) for the max-seq file, and a single WAL
file can be the tracked file for both bounds, so the same file the max-seq path already defends
against silent corruption of was, until this fix, left undefended on the min-seq side. Reviewer
finding on PR #376 (`handarbeit-pruefer`) traced this asymmetry precisely; the fix closes it while
still avoiding a re-read for the common case.

**Reconciliation writes preserve the other bound's fields unchanged, never recompute them.** When
`wal_max_seq_fast_path` re-derives `max_seq` after detecting growth in the tracked file, it writes
back `manifest.min_seq`/`min_seq_file` exactly as read — untouched — rather than attempting to
verify or refresh them. This is safe (the fingerprint match already guarantees the file name set,
and therefore the min-seq file's first line, hasn't changed) and avoids a subtler bug: updating one
bound's on-disk size/seq fields from within the other bound's reconciliation path risks the two
functions' independent staleness signals drifting out of sync with each other. Keeping each
function's fast path strictly read-only with respect to the other bound's fields sidesteps that
class of bug entirely.

**Inherited limitation, not a new one.** `wal_min_seq_fast_path`'s `(None, None)` case — "no file
had a parseable line as of manifest-write time" — carries the same accepted gap
`wal_max_seq_fast_path`'s `(None, None)` case already has: a file that later gains a first
parseable line via append, without any change to the file *name* set, is not detected by the
fingerprint check. This is pre-existing in the merged `wal_max_seq` design (not introduced by this
extension) and considered out of scope to close here — it would need a new signal beyond what this
issue's manifest mechanism reuses.
