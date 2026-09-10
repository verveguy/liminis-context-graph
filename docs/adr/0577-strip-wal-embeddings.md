# ADR-0577: `knowledge_strip_wal_embeddings` — Two-Pass In-Place Stripping of Pre-0.14 WAL Vectors

**Status**: Accepted
**Date**: 2026-09-09
**Issue**: #577
**Relates to**: ADR-0526 (vectors are a local cache — stopped writing them, made replay ignore
them; this ADR reclaims what that decision left behind on disk), ADR-0440 (recompute embeddings
on replay — the prerequisite that makes reclaiming this space safe), ADR-0378 (multi-stream WAL,
per-group directories — the layout this operation discovers and traverses)

## Context

ADR-0526 stopped the WAL writer from emitting embedding-vector `params` fields and made replay
ignore any vector value it finds in an older WAL, since it always recomputes from co-located
source text instead. That change was explicitly non-rewriting: a pre-0.14 WAL still on disk keeps
carrying vectors that were, on the reference #217 corpus, 89.9% of its bytes (66.9 MB of 74.4 MB)
— every one of them parsed and discarded on every replay, rebuild, and hydration, and forever if
the WAL is checked into version control (as `liminis`'s `demo-notebook` deliberately does, since
its WAL is the durable record). There was no way to reclaim that space short of regenerating the
whole WAL, which means re-running extraction — the expensive step the WAL exists to avoid.

Because replay already ignores a stored vector unconditionally, removing one from an existing WAL
line cannot change what replay produces. This is deletion of bytes the system has already decided
never to read, which is what makes the operation safe to run against a workspace's only durable
record without a backup-first ritual (see Consequences and the dependency called out below).

## Decision

### 1. A pure filesystem transform, not a DB operation

`crates/core/src/wal_strip.rs`'s `strip_wal_embeddings(wal_root, group_id, dry_run)` never opens
a database connection — unlike `knowledge_dump_wal`, which needs a readable DB to snapshot graph
contents, this operation only reads and rewrites `.jsonl` files already on disk. It is therefore
added to `handlers.rs`'s `exempt_in_degraded` list alongside `knowledge_wal_mark_list`/`_delete`:
there is no technical reason to require the DB to be available, and an operator recovering a
degraded workspace can reclaim WAL space as part of that same recovery session.

### 2. Two-pass per file: a read-only pass decides whether any I/O is needed at all

For each `.jsonl` file, `scan_file` streams every line (`BufRead::read_until(b'\n', ..)`, so a
line's exact trailing-newline presence and raw bytes are preserved, never `.lines()`, which would
silently lose that distinction) and applies the same per-line transform pass 2 would, but writes
nothing anywhere. This single read determines, cheaply: whether any line actually carries an
embedding-vector key (`needs_rewrite`), what the file's would-be output size and rewritten-record
count are, and whether any embedding-vector value is malformed (see Decision 4).

If `needs_rewrite` is false, the file is already clean — nothing is opened, nothing is written,
and the original's mtime and bytes are left exactly as they were. This is what makes a re-run over
an already-stripped WAL (or a WAL that was always post-0.14 and never had vectors) genuinely
zero-I/O per file, not merely zero-net-change: the idempotent common case pays only the cost of
one read pass, never a tmp-file create.

Only a file that actually needs rewriting pays for a second pass (`rewrite_file`), which
re-streams the file and performs the real write. This doubles parse cost for files that do need
rewriting — accepted, since stripping is a one-time (or rare, scripted) admin operation, not a hot
path, and the alternative (always opening a tmp file speculatively, discarding it if nothing
changed) would cost real I/O on every routine idempotent re-run instead.

### 3. Byte-for-byte pass-through for every line that doesn't change

A line with no embedding-vector key present in its `params` is written to the tmp file (or,
during the read-only pass, contributes to the byte-length estimate) using its **original raw
bytes**, never re-serialized through `serde_json`. Re-serializing every line unconditionally —
even ones with nothing to strip — risks reformatting drift (number formatting, key ordering,
whitespace) that would silently break the byte-identical guarantee a re-run depends on. Only a
line that actually loses a key is deserialized to `WalLine`, passed through
`wal::strip_vector_params` (widened from private to `pub(crate)` for this reuse — the
alternative, reimplementing the four-key removal independently, is exactly the second strip site
`wal.rs`'s own doc comments warn against), and re-serialized; `serde_json`'s workspace-level
`preserve_order` feature keeps the remaining `params` keys in their original relative order after
the removal.

### 4. Malformed value: fail the whole file, before anything is written. Unparseable line: pass through, never fail.

These are two different, differently-scoped failures:

- A line that parses as a well-formed `WalLine` but whose embedding-vector value is not a
  well-formed JSON array of numbers is **malformed**. This aborts the *file* — `scan_file`
  returns an error, and because pass 1 is read-only, the file is guaranteed to still be exactly
  as it was found. The error is reported per-file in the response and the run continues with the
  next file. Failing the whole file (not silently skipping just that line) is deliberate: a
  malformed vector value signals something is already wrong with this file's shape, and guessing
  at how to reconcile it with the rest of the file's otherwise-normal lines risks compounding the
  problem rather than surfacing it.
- A line that fails to parse as UTF-8, or doesn't deserialize as a `WalLine` at all, is
  **unparseable** — a structurally different, weaker failure. It passes through byte-for-byte
  unchanged and is counted separately (`unparseable_lines`), mirroring `replay.rs`'s existing
  tolerant-skip precedent for bad lines (`[WAL WARN] skipping unparseable line`). It never aborts
  the file: a WAL that already tolerates the occasional bad line on replay must not become
  stricter under this operation.

### 5. Atomic replace: tmp file in the same directory, rename on success only

`rewrite_file` writes to `<name>.<uuid>.tmp` in the file's own directory (same filesystem,
required for an atomic rename), flushes and `sync_all()`s it, then `fs::rename`s it over the
original — the single commit point. Any failure before the rename removes the tmp file and leaves
the original completely untouched. The `.tmp` suffix (not a `.jsonl` variant) guarantees a
crash-orphaned tmp file is never picked up by a later `.jsonl` file-enumeration glob, or mistaken
for a WAL record file. Unlike `.wal-bounds.*.tmp`, a leftover tmp file here is not actively swept:
this is a rare admin call, not a per-request hot path, so at most one small leaked file per crash
is an acceptable cost — its extension already guarantees it is harmless, which is the actual
requirement.

### 6. Discovery: multi-stream layout first, legacy flat layout as defense-in-depth

`wal_group::migrate_wal_root_if_needed(wal_root)` runs first, best-effort — it is itself
idempotent and crash-safe, and `AppState::from_env` already runs it non-fatally at startup, so
this is defense-in-depth for a WAL root that startup hasn't reached yet (e.g. it failed
non-fatally, or the caller supplied a `wal_root` startup never saw). Discovery then combines
`wal_group::list_group_wal_dirs(wal_root)` (every multi-stream group directory, ADR-0378) with a
direct scan of `wal_root` itself (any legacy flat-layout `.jsonl` files migration didn't reach).
An explicit `group_id` narrows processing to that one group's directory via
`wal_group::group_wal_dir`; a directory that doesn't exist (an unknown `group_id`, or no WAL for
that group yet) produces an empty report, not an error — this is a normal state, not a failure. A
directory that exists but can't be read (permissions) is reported as a per-directory error and
does not abort the rest of the run, matching the same per-file error handling in Decision 4.

### 7. Concurrency: the whole-WAL-directory write lock, held for the whole call

`state.write_lock.write().await`, held for the operation's entire duration, is the same exclusion
mechanism `knowledge_dump_wal` and `knowledge_prepare_checkpoint` already use for a whole-WAL-root
filesystem operation — no new locking primitive is introduced. This excludes both a concurrent
`knowledge_process_chunk` (or any other live write) and a second concurrent invocation of this
same tool from interleaving with the in-place rewrite.

### 8. Resyncing a live writer's rotation bookkeeping after a rewrite

The write lock (Decision 7) only excludes a concurrent write from interleaving with a rewrite in
progress; it doesn't, by itself, keep a live `WalWriter`'s in-memory `bytes_in_current_file`
counter (`wal.rs`) in sync with the file it just rewrote. Nothing excludes a group's
currently-open file from this operation's `.jsonl` glob, so if that file needed stripping, it
shrinks on disk while the writer's cached count — updated only by the writer's own appends —
stays at the pre-strip size. Left alone, this doesn't lose data (the writer still appends
correctly by path), but it drifts `max_bytes_per_file` rotation decisions, rotating earlier than
configured until the next natural rotation resets the counter from zero.

The fix is `WalWriter::resync_current_file_bytes`, called on every live writer in
`state.wal_writers` from `handle_strip_wal_embeddings` after the rewrite pass completes and before
`write_lock` is released — it re-`stat`s the writer's `current_file` (if any) and overwrites
`bytes_in_current_file` with the actual on-disk length. It's unconditional (not gated on which
group was touched) because it's cheap — bounded by the number of live writers in this process, not
WAL size — and correct as a no-op when nothing changed. Under the current write path this drift is
not actually reachable today: `WalWriter::log_mutation` strips every `VECTOR_PARAM_KEYS` entry
before a line is ever buffered (issue #526), and a freshly-constructed writer always opens a brand
new, timestamped file rather than resuming an existing one — so a live writer's own `current_file`
can never itself contain an embedding for this operation to strip. The resync is deliberately
unconditional and cheap defensive-in-depth against that invariant changing (e.g. a future
WAL-resume-on-restart feature) rather than something this issue's tests can trigger end-to-end
through today's write path; `wal.rs`'s own unit tests cover the mechanism directly by simulating an
out-of-band shrink of a writer's open file.

### 9. The literal-inlined-vector shape stays explicitly out of scope

ADR-0526 § Decision 4 already confirmed one WAL shape this key-removal strategy cannot reach: an
externally-produced (Python/graphiti-driver) `CREATE` line that inlines a vector as a raw Cypher
literal (`fact_embedding: [0.1, 0.2, 0.3, 0.4]`) with `"params":{}` — there is no JSON key for a
key-removal strategy to touch at all. ADR-0526 decided this shape is "out of scope for recompute
by construction," pinned by `test_literal_inlined_fact_embedding_replays_unchanged`
(`wal_replay.rs`, fixture `python_produced.jsonl`). This issue makes the same call for the same
reason: a WAL mixing in that externally-produced content retains those bytes regardless, and
reaching them would mean Cypher-text surgery — a fundamentally higher-risk operation than JSON-key
removal — for a shape this codebase doesn't itself produce. `knowledge_strip_wal_embeddings`
leaves it untouched, matching precedent rather than silently under-delivering against the 89.9%
figure for a WAL containing such lines.

## Rejected Alternatives

**Always open a tmp file and rename-on-change, skipping the read-only first pass.** Simpler (one
pass, one code path), but pays real I/O — opening, writing nothing of substance, and discarding a
tmp file — on every already-clean file, defeating the "routine, scriptable, safe to re-run"
framing this operation is explicitly designed around (a workspace stripped once and then re-run
as a matter of course after every upgrade should cost nothing on the second and later runs).

**Re-serialize every line unconditionally through `serde_json`, not just the ones that change.**
Simpler than tracking "did this line change," but risks silent reformatting drift on every
untouched line — number formatting or key-order differences that would make a re-run's output not
byte-identical to what was already on disk, breaking the idempotency guarantee this operation's
safety case depends on.

**Extend scope to cover the literal-inlined-vector shape via Cypher-text pattern matching.**
Considered, since it would improve the achievable reduction on a WAL mixing in
externally-produced content. Rejected: text surgery on a Cypher template is a fundamentally
different risk class than JSON-key removal (a malformed match could corrupt the query itself, not
just drop a value), for a shape this codebase's own writer never produces and ADR-0526 already
decided was out of scope for a closely related operation (recompute). Consistency with that
existing precedent was judged more valuable than closing this one corpus-composition gap.

**Sweep leftover tmp files the way `.wal-bounds.*.tmp` is swept on the full-scan fallback path.**
Rejected as unnecessary complexity for this operation's actual failure mode: `.wal-bounds.json` is
rewritten on nearly every WAL-scanning call, so its tmp debris can accumulate meaningfully over
time; this operation runs rarely (a one-time post-upgrade reclaim, or an occasional scripted
re-run), so at most one small leaked `.tmp` file per crash is an acceptable, self-limiting cost,
and its extension alone already satisfies the actual requirement (never mistaken for a WAL record
file).

## Consequences

- **A pre-0.14 WAL shrinks by a proportion consistent with the documented 89.9% figure** when
  its content is dominated by lcg-authored, JSON-param-bound vectors (SC-001) — verified against
  the #217 reference corpus fixture (`crates/core/tests/fixtures/real_corpus_wal/wal/`,
  `crates/core/tests/wal_strip_embeddings.rs`'s `#[ignore]`d real-corpus test).
- **Replaying a stripped WAL produces an identical graph to replaying the original** (SC-002) —
  entity/episodic/relationship node counts match exactly between the two, confirmed by replaying
  both into separate fresh databases via the existing `WalReplayer`. This is a direct consequence
  of ADR-0526's replay behavior, not something this operation itself has to maintain: since
  replay never reads a stored vector regardless of whether it's present, removing one is
  observationally inert to replay's outcome by construction.
- **Re-running the operation on an already-stripped WAL is a true no-op** (SC-003/SC-004) — zero
  files opened for writing, so bytes and mtime are left exactly as found, verified with an
  explicit re-run test asserting byte-for-byte and mtime equality.
- **This operation's safety depends on ADR-0526's "a vector found in an older WAL is ignored"
  replay behavior remaining true.** If a future change ever made replay read a stored vector value
  again (reverting ADR-0526, or introducing a new code path that binds `params` verbatim without
  going through the recompute path), this operation would become lossy for every WAL it has
  already stripped — there would be no way to recover the removed vectors short of re-extraction,
  exactly the cost this operation exists to avoid paying. Any change to that replay behavior must
  account for this dependency.
- **The literal-inlined-vector WAL shape (`python_produced.jsonl`) is unaffected by this
  operation** — a WAL containing such lines will not reach the full 89.9% reduction, by design
  (Decision 8). This is a caveat on SC-001's achievable reduction for mixed-origin WALs, not a
  defect.
- **`knowledge_strip_wal_embeddings` is an MCP-only tool** — no standalone CLI/binary form, no
  progress-streaming support in this initial implementation (a large workspace's per-file
  granularity means a caller can still observe progress by polling `files_processed` against a
  known total, if needed) — matching `knowledge_backfill_summary_embeddings`'s invocation
  pattern. Both may be added later without changing this operation's response shape.
- **The sibling `liminis-app` repository's `service_protocol.py` needs this method added by
  hand** — this repo's PR cannot make that change itself; a code comment at the handler's call
  site flags it, matching the precedent already left by `knowledge_dump_wal` and
  `knowledge_canonicalize_relations`.

