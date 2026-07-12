# ADR-0026: Episode-Cursor WAL Resume for Checkpoint Recovery

**Status**: Accepted
**Date**: 2026-06-18
**Relates to**: ADR-0009 (degraded-mode startup & in-process recovery), ADR-0025 (auto-heal index build)
**Implemented by**: liminis-context-graph#151 (engine autonomous recovery); surfaced in the app by liminis#864; progress logging in liminis-context-graph#152

## Context

ADR-0009 established that a corrupt lbug `db.wal` puts the service into degraded mode, and that `knowledge_recover {strategy:"drop_lbug_wal"}` renames the torn WAL aside and reopens `liminis.db` **at its last lbug checkpoint**. The main DB file is not bricked — lbug simply refuses to auto-truncate a torn WAL tail (a robustness gap versus Postgres/SQLite).

But recovering to the last checkpoint is not the same as recovering to the WAL head:

1. **The checkpoint can lag the WAL.** lbug checkpoints asynchronously on its own schedule. At crash time the most-recent mutations live in the un-checkpointed `db.wal` tail, which `drop_lbug_wal` discards. So the reopened DB can be silently behind the workspace WAL (`.lcg/wal/*.jsonl`) by some number of mutations.
2. **No indexes.** A `drop_lbug_wal` reopen does not rebuild FTS/HNSW (see ADR-0025 for why fresh DBs lack indexes), so search is degraded until a replay rebuilds them.
3. **No resume cursor exists.** There is no persisted "last-applied WAL seq" anywhere — `knowledge_prepare_checkpoint` only rotates and counts WAL files, and replay progress is delivered as an in-memory callback to the caller, never persisted. So after a crash, nothing records how far the checkpoint got, and `rebuild_from_wal`'s `from_seq` parameter has no safe value to derive from.

A full `rebuild_from_workspace_wal` (wipe + replay from seq 0) always works but is expensive — on a large workspace it is multi-hour.

## Decision

**Use the last `Episodic` node in the recovered DB as the WAL resume cursor.**

Episodes are ingested in WAL order, so the last episode durably present in the checkpoint marks how far the checkpoint got. The recovery sequence is:

1. `drop_lbug_wal` → reopen at the last checkpoint (ADR-0009).
2. **Derive the resume point:** read the last episode (`retrieve_episodes(group, 1)`), take its `uuid`, scan `.lcg/wal/*.jsonl` for that uuid, and read that line's `"seq":N`.
3. `rebuild_from_wal { from_seq: N }` → drops FTS, replays `seq ≥ N` onto the existing DB, then rebuilds FTS + HNSW via `build_indices_and_constraints`.

`N` (the last episode's seq) is a **safe, conservative** cursor: the checkpoint is guaranteed `≥ N` because the episode is queryable, so resuming there can never skip un-applied mutations. The overlap (re-applying the part of the WAL the checkpoint already had) re-applies idempotently — `MERGE`s are no-ops and create-form statements collide harmlessly — so the only cost of resuming slightly early is a little redundant work, never corruption or loss.

### Why episode-cursor instead of a persisted seq cursor

The obvious alternative is to persist a "last-applied seq" durably (e.g. write it into a metadata row inside each batch's transaction). The episode-cursor is strictly better for recovery because it is:

- **Derivable from data** — no new schema, no write-path changes, no per-batch overhead.
- **Retroactive** — it works on databases that crashed *before* any cursor mechanism existed (it recovered exactly such a database; see below).

The trade-off is granularity: the cursor lands on an episode boundary rather than the exact crash seq, which is why the overlap-tolerance argument above matters.

### Fallback

If `drop_lbug_wal` fails (the disk event also tore the main DB file) or no last episode can be located in the WAL, fall back to full `rebuild_from_workspace_wal`.

## Consequences

- **Validated end-to-end (2026-06-18).** A production workspace whose reload was interrupted by a disk-full event (torn `db.wal`) was recovered in **~107s vs an estimated ~7h** full replay. `drop_lbug_wal` reopened the 583 MB DB at its checkpoint in 4.4s; the last episode (`seq 4,641,989`) was located in WAL file 43,820/43,821 (**99.998%** complete — the checkpoint had drained almost the entire WAL); `rebuild_from_wal {from_seq:4641989}` replayed **8,719 mutations with 0 failures**, recovering exactly **1 missing episode** (+2 entities, +32 relationships) and rebuilding all indexes. FTS search was confirmed working afterward.
- Recovery prefers cheap checkpoint-resume over full replay whenever the checkpoint is usable.
- The engine can run this autonomously on startup (liminis-context-graph#151) so the service self-heals without an external orchestrator — important when liminis-context-graph runs as a standalone service outside the app.

### Gotchas worth remembering

- WAL filenames (`YYYYMMDD_HHMMSS_hex_NNNNN.jsonl`) sort lexicographically = chronologically; the `_NNNNN` suffix is a **global** counter that does not reset per batch, so it doubles as a file index.
- The `rebuild_from_wal` result's `indexes_created` field counts `CREATE INDEX` lines **in the WAL** (always 0) — it is **not** the post-replay index build. Do not surface it as "indexes not built."
- `rebuild_from_wal` does **not** wipe the DB; it replays onto whatever is there. That is exactly what makes `from_seq` resume work, but it means a *full* (`from_seq:0`) rebuild onto a non-empty DB produces duplicate-PK churn — wipe first for a clean full rebuild.
