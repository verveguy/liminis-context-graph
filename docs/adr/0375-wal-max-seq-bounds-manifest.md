# ADR-0375: `wal_max_seq` Bounds Manifest

**Status**: Accepted
**Date**: 2026-08-12
**Issue**: #375
**Relates to**: ADR-0353 (persist and expose `applied_seq`/`max_seq`), ADR-0026 (episode-cursor
WAL resume)

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
}
```

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
   tail alone (not the whole directory) needs re-reading.

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
`write_wal_bounds_manifest` cleans up its own tmp file on a failed write or rename, and also sweeps
any `.wal-bounds.*.tmp` sibling older than five minutes on every call, bounding accumulation from
past crashes without needing a fixed name.

### Treating the manifest itself as untrusted input

Since the manifest tolerates being hand-edited, corrupted, or produced by a foreign writer (that's
what "any parse failure falls back" means in practice), the fast path validates it defensively
before acting on it, rather than trusting its shape:

- **`max_seq_file` is validated as a single path component with a `.jsonl` extension** (no `/`,
  `\`, or literal `..`) before being joined onto `wal_dir`. An unvalidated join would let a
  corrupted or crafted manifest value escape the WAL directory (e.g. `../../etc/passwd`).
- **A `read_dir` failure while computing the current file-set fingerprint is treated as "can't
  confirm freshness," not as an empty directory.** Coercing the error into an empty result could
  spuriously match an empty-WAL manifest's fingerprint and serve a stale cached value straight
  past a real I/O error.
- **A re-read of the tracked max-seq file that reports a seq *lower* than the cached value is
  rejected**, falling back to a full scan instead. The tracked file only grows via ordinary
  append, so a decrease means something other than routine growth happened (e.g. truncation) and
  the manifest can no longer be trusted at face value.

Any miss, mismatch, or parse failure falls back to the pre-existing full scan
(`scan_max_seq_detailed`), which both computes the correct answer and produces a fresh manifest to
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
- **`wal_min_seq`/`knowledge_wal_mark_list` (#365) are out of scope here** — neither exists on
  `main` as of this issue's implementation (PR #367 not yet merged). The same manifest shape
  extends naturally to a minimum-seq field later; FR-009's cost bound for
  `knowledge_wal_mark_list` is deferred to a follow-up issue once #365 lands.
