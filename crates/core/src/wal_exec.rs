//! WAL-flush helpers for write handlers. See ADR-0015 for the drain-and-flush pattern.
//!
//! Every write handler records Cypher via `Conn::raw_query` / `Conn::cypher_query`,
//! then calls one of these helpers with `conn.drain_mutations()` after the writes succeed.
//! Non-mutations are silently discarded by `WalWriter::log_mutation`'s built-in filter.
//!
//! WAL failures are **non-fatal**: the DB write already committed; the WAL is a recovery
//! artifact, not a write gate. Errors are logged to stderr and not propagated.
//!
//! Which helper to use:
//! - `wal_flush_chunk`: episode processing — wraps all cyphers in ONE `with_chunk` call
//!   so they land in the WAL atomically as a unit (mirrors Python `with_chunk` semantics).
//! - `wal_flush_ungrouped`: every other write path — the assertion API, delete/merge/rebind/
//!   correction handlers, raw cypher, and maintenance helpers (backfill, canonicalize,
//!   reprocess) — one `with_chunk` per cypher so each mutation is independently flushed.

use serde_json::json;

use crate::{
    app_state::AppState,
    db::Conn,
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
    wal::WalWriter,
};

/// Persists `group_id`'s advanced `applied_seq` after a `wal_flush_ungrouped` (or
/// `wal_flush_chunk`) call returns `Some(seq)` — the same write-after-commit, non-fatal pattern
/// `episode::add_episode` models for the chunk-flush path (issue #353 FR-002; per-group since
/// #378). A no-op on `None`, matching FR-003's "leave it where it is" safe direction. Kept as a
/// shared helper (issue #383) rather than inlined at each of `wal_flush_ungrouped`'s ~17 call
/// sites, since the one thing that must never be copy-paste-wrong here is which `group_id`
/// expression gets passed to `set_applied_seq` — a single call site makes that a type-level
/// pairing with the flush call's own `group_id` argument instead of a repeated, error-prone
/// literal.
pub(crate) fn advance_applied_seq(conn: &Conn<'_>, group_id: &str, seq: Option<u64>) {
    if let Some(seq) = seq {
        if let Err(e) = conn.set_applied_seq(group_id, seq) {
            eprintln!(
                "liminis-context-graph: advance_applied_seq: failed to persist applied_seq={seq} for group {group_id} (non-fatal): {e}"
            );
        }
    }
}

fn emit_rotation_if_any(writer: &mut WalWriter, sink: &dyn TelemetrySink) {
    if let Some(info) = writer.take_rotation() {
        sink.emit(TelemetryEvent::WalRotated {
            ts_ms: now_ms(),
            from_file_seq: info.from_file_seq,
            to_file_seq: info.to_file_seq,
            closed_bytes: info.closed_bytes,
            closed_events: info.closed_events as u64,
        });
    }
}

/// Flushes `cyphers` to `group_id`'s own WAL directory as a single chunk-atomic group (issue
/// #378: routed to that group's writer, lazily created on first use via `AppState::with_wal_writer`).
///
/// Use for episode Phase C where all mutations for one chunk should land atomically.
///
/// Returns the max `seq` assigned to this chunk's lines (issue #353), or `None` if nothing was
/// actually written — an empty `mutations` list, a lock/writer-absent short-circuit, or a chunk
/// whose entries were all filtered out by `WalWriter::log_mutation` (reads / index DDL), or a
/// write failure. Callers use `Some(seq)` to advance `group_id`'s persisted `applied_seq`
/// position; `None` means "leave it where it is" — always the safe direction (FR-003).
pub(crate) fn wal_flush_chunk(
    state: &AppState,
    group_id: &str,
    mutations: Vec<(String, serde_json::Value)>,
) -> Option<u64> {
    if mutations.is_empty() {
        return None;
    }
    state
        .with_wal_writer(group_id, |writer| {
            let before = writer.global_seq();
            let result = writer.with_chunk(|w| {
                for (cypher, params) in &mutations {
                    w.log_mutation(cypher, wal_params(params), "")?;
                }
                Ok(())
            });
            match result {
                Ok(_) => {
                    emit_rotation_if_any(writer, state.sink.as_ref());
                    let after = writer.global_seq();
                    (after > before).then(|| after - 1)
                }
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: wal_flush_chunk: write failed (non-fatal): {e}"
                    );
                    None
                }
            }
        })
        .flatten()
}

/// Flushes mutations to `group_id`'s own WAL directory as individual ungrouped entries (one
/// `with_chunk` per mutation).
///
/// Use for every write path other than episode ingest: the assertion API
/// (`handle_assert_entity`, `handle_assert_relationship`, `handle_add_cross_group_edge`),
/// delete/merge/rebind/correction handlers, raw cypher (`handle_query_cypher`), and maintenance
/// helpers (backfill, canonicalize, reprocess).
///
/// Returns the highest `seq` actually, durably flushed across the batch (issue #383), or `None`
/// if nothing was — an empty `mutations` list, every mutation's `with_chunk` call failing, or
/// every mutation being filtered out by `WalWriter::log_mutation` (reads / index DDL) despite
/// each call returning `Ok`.
/// Unlike `wal_flush_chunk`'s all-or-nothing chunk, this loop does not abort on a per-mutation
/// failure, so a later mutation's success after an earlier one's failure is still credited: the
/// returned seq is a running max over every successfully-flushed mutation's seq (diffed
/// per-mutation via `writer.global_seq()`, gated on that mutation's `Result::Ok`), not a single
/// before/after diff across the whole batch. Callers use `Some(seq)` to advance `group_id`'s
/// persisted `applied_seq` position exactly as `wal_flush_chunk`'s callers do; `None` means
/// "leave it where it is" — always the safe direction (FR-003).
pub(crate) fn wal_flush_ungrouped(
    state: &AppState,
    group_id: &str,
    mutations: Vec<(String, serde_json::Value)>,
) -> Option<u64> {
    if mutations.is_empty() {
        return None;
    }
    state
        .with_wal_writer(group_id, |writer| {
            let mut max_seq: Option<u64> = None;
            for (cypher, params) in &mutations {
                let before = writer.global_seq();
                let result =
                    writer.with_chunk(|w| w.log_mutation(cypher, wal_params(params), ""));
                match result {
                    Ok(_) => {
                        emit_rotation_if_any(writer, state.sink.as_ref());
                        let after = writer.global_seq();
                        if after > before {
                            let seq = after - 1;
                            max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "liminis-context-graph: wal_flush_ungrouped: write failed (non-fatal): {e}"
                        )
                    }
                }
            }
            max_seq
        })
        .flatten()
}

/// Re-derives `group_id`'s writer's `global_seq` after a non-dry-run WAL rebuild/replay
/// completes (issue #352), so a WAL directory populated after the writer was constructed doesn't
/// leave the writer emitting `seq` values that collide with what's already on disk. Non-fatal: a
/// rebuild that already succeeded shouldn't fail because of a re-derivation I/O error (e.g. the
/// WAL dir vanished between replay and this call), matching this module's existing failure
/// posture.
///
/// Returns `true` if the resync completed without error (including the no-op case of no writer
/// configured for `group_id` yet), `false` if the on-disk scan failed. Callers pairing this with
/// `GlobalSeqResyncGuard` should only `mark_done()` on `true` — on `false` the guard's `Drop`-time
/// fallback (an on-disk-scan-only resync with no `last_committed_seq` floor) should stay armed as
/// a second chance, rather than being disarmed by a call that didn't actually update `global_seq`.
pub(crate) fn resync_global_seq_after_rebuild(
    state: &AppState,
    group_id: &str,
    last_committed_seq: Option<u64>,
) -> bool {
    state
        .with_wal_writer(group_id, |writer| {
            if let Err(e) = writer.resync_global_seq(last_committed_seq) {
                eprintln!(
                    "liminis-context-graph: resync_global_seq_after_rebuild: scan failed (non-fatal): {e}"
                );
                return false;
            }
            true
        })
        .unwrap_or(true)
}

/// Safety net for `resync_global_seq_after_rebuild`: a non-dry-run rebuild can exit early — e.g.
/// `WalReplayer::replay_opts` returning `Err` on a transaction-level failure — before reaching the
/// normal post-replay call site, which would otherwise leave `global_seq` stale after a
/// `force_clear` that already ran (issue #352 FR-002). Construct at the top of a rebuild's
/// non-dry-run scope; call `mark_done` once the normal call site has already resynced with the
/// more accurate `last_committed_seq`, so a clean run doesn't pay for a second directory scan.
/// If the scope exits any other way, `Drop` fires a `None`-floor resync — no `last_committed_seq`
/// to combine with, but the on-disk scan alone is still enough to clear past whatever was just
/// replayed. Resync is monotonic and idempotent, so firing twice is harmless, just redundant.
pub(crate) struct GlobalSeqResyncGuard<'a> {
    state: &'a AppState,
    group_id: String,
    done: bool,
}

impl<'a> GlobalSeqResyncGuard<'a> {
    pub(crate) fn new(state: &'a AppState, group_id: &str) -> Self {
        Self {
            state,
            group_id: group_id.to_string(),
            done: false,
        }
    }

    pub(crate) fn mark_done(&mut self) {
        self.done = true;
    }
}

impl Drop for GlobalSeqResyncGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            resync_global_seq_after_rebuild(self.state, &self.group_id, None);
        }
    }
}

/// Normalizes a recorded params value for the WAL: `raw_query`/`cypher_query` record
/// `Null` (DDL / non-parameterized statements have no params), which we serialize as an
/// empty object so the WAL line's `params` field stays a consistent shape.
fn wal_params(params: &serde_json::Value) -> serde_json::Value {
    if params.is_null() {
        json!({})
    } else {
        params.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex};

    use arc_swap::ArcSwapOption;

    use super::*;
    use crate::{
        app_state::OntologyDriftState, dedup_adapter::PassthroughDedupAdapter,
        embedder::MockEmbedder, extractor::MockExtractor, telemetry::NoopSink,
    };

    /// Minimal `AppState` for wal_exec's own unit tests — only `wal_root`/`wal_writers` and
    /// `sink` matter here; every other field is a cheap stand-in.
    fn test_state(wal_root: Option<PathBuf>) -> AppState {
        AppState {
            db: ArcSwapOption::from(None),
            degraded_reason: Arc::new(Mutex::new(None)),
            embedder: Arc::new(MockEmbedder::new(4)),
            extractor: Arc::new(MockExtractor),
            dedup: Arc::new(PassthroughDedupAdapter),
            write_lock: Arc::new(tokio::sync::RwLock::new(())),
            sink: Arc::new(NoopSink),
            db_path: "test.db".to_string(),
            wal_root,
            wal_max_events_per_file: 1000,
            wal_max_bytes_per_file: 0,
            embedding_model: "bge-base-en-v1.5".to_string(),
            wal_writers: Arc::new(Mutex::new(HashMap::new())),
            active_writes: Arc::new(AtomicUsize::new(0)),
            rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
            workspace_root: None,
            indices_built: Arc::new(AtomicBool::new(false)),
            cancel_token: tokio_util::sync::CancellationToken::new(),
            cancelled_chunks: Arc::new(AtomicUsize::new(0)),
            ontology: None,
            ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        }
    }

    fn write_wal_line(dir: &std::path::Path, file_name: &str, seq: u64) {
        std::fs::write(
            dir.join(file_name),
            format!(
                r#"{{"seq":{seq},"ts":"2026-08-05T00:00:00.000000+00:00","db":"default","cypher":"MERGE (n:Entity {{uuid: $uuid}})","params":{{"uuid":"x"}}}}"#
            ) + "\n",
        )
        .unwrap();
    }

    /// A guard dropped without `mark_done()` — the shape of an early `?` return from a rebuild
    /// closure after a `force_clear` already ran (issue #352 FR-002) — must still resync.
    #[test]
    fn resync_guard_fires_on_drop_when_not_marked_done() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wal-root");
        let state = test_state(Some(root.clone()));
        // Force the "liminis" writer into existence so its directory exists before the
        // out-of-band write below.
        state.with_wal_writer("liminis", |_| {}).unwrap();

        // Populated after construction; highest on-disk seq is 4 (next-seq-to-assign is 5).
        write_wal_line(&root.join("liminis"), "seeded_0000.jsonl", 4);

        {
            let _guard = GlobalSeqResyncGuard::new(&state, "liminis");
        }

        let global_seq = state
            .with_wal_writer("liminis", |w| w.global_seq())
            .unwrap();
        assert_eq!(
            global_seq, 5,
            "an unmarked guard must resync from the on-disk scan when dropped"
        );
    }

    /// A guard marked done before dropping (the happy path, where the caller already resynced
    /// explicitly with the more accurate `last_committed_seq`) must not resync a second time.
    #[test]
    fn resync_guard_is_a_noop_on_drop_when_marked_done() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wal-root");
        let state = test_state(Some(root.clone()));
        state.with_wal_writer("liminis", |_| {}).unwrap();

        resync_global_seq_after_rebuild(&state, "liminis", Some(99));
        // Written after the explicit resync above, so a broken mark_done (one that lets Drop's
        // fallback resync fire anyway) would pick this up and push global_seq past 100 — making
        // this test actually distinguish "resynced once" from "resynced twice".
        write_wal_line(&root.join("liminis"), "late_0000.jsonl", 150);
        {
            let mut guard = GlobalSeqResyncGuard::new(&state, "liminis");
            guard.mark_done();
        }

        let global_seq = state
            .with_wal_writer("liminis", |w| w.global_seq())
            .unwrap();
        assert_eq!(
            global_seq, 100,
            "a marked-done guard must not alter global_seq again on drop"
        );
    }

    /// SC-004 (FR-003 crash safety): a crash between a chunk's WAL flush and the
    /// `set_applied_seq` write that normally follows it (episode.rs, immediately after this
    /// function returns) must leave `applied_seq` trailing what's actually committed, never
    /// advancing past it. Simulated deterministically by committing a second chunk's mutation
    /// and flushing it to WAL — both actually happen — then simply not calling
    /// `set_applied_seq` for it, mirroring exactly what a `kill -9` between those two steps
    /// would leave behind.
    #[test]
    fn skipped_set_applied_seq_write_leaves_applied_seq_trailing_not_leading() {
        let db_dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(db_dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        let wal_root = tempfile::tempdir().unwrap();
        let state = test_state(Some(wal_root.path().to_path_buf()));

        // Chunk 1: commit, flush to WAL, and record applied_seq — the normal, successful path.
        conn.raw_query(
            "CREATE (:Episodic {uuid: 'ep-1', name: 'n', group_id: 'g', \
             created_at: timestamp('2026-01-01'), source: 'text', source_description: '', \
             content: 'c', valid_at: timestamp('2026-01-01')})",
        )
        .unwrap();
        let seq1 = wal_flush_chunk(&state, "liminis", conn.drain_mutations())
            .expect("chunk 1 must assign a seq");
        conn.set_applied_seq("liminis", seq1).unwrap();

        // Chunk 2: commit and flush — both really happen — but the position write that would
        // normally follow is deliberately skipped, simulating a crash right after the flush.
        conn.raw_query(
            "CREATE (:Episodic {uuid: 'ep-2', name: 'n', group_id: 'g', \
             created_at: timestamp('2026-01-01'), source: 'text', source_description: '', \
             content: 'c', valid_at: timestamp('2026-01-01')})",
        )
        .unwrap();
        let seq2 = wal_flush_chunk(&state, "liminis", conn.drain_mutations())
            .expect("chunk 2 must assign a seq");
        assert!(seq2 > seq1, "chunk 2's seq must be strictly higher");
        // No conn.set_applied_seq(seq2) call here — the simulated crash.

        // Both episodes are actually committed in the graph...
        assert_eq!(
            conn.count_nodes("Episodic").unwrap(),
            2,
            "both chunks' mutations must be committed, crash or not"
        );
        // ...but applied_seq must still report only chunk 1's position — never chunk 2's,
        // which would wrongly signal chunk 2's mutations are a recorded, skippable position.
        assert_eq!(
            conn.get_applied_seq("liminis").unwrap(),
            Some(seq1),
            "a skipped position write must leave applied_seq trailing the last chunk that \
             actually recorded it, never advancing to an unrecorded chunk's seq"
        );
    }

    fn mutation(uuid: &str) -> (String, serde_json::Value) {
        (
            "CREATE (n:Entity {uuid: $uuid})".to_string(),
            json!({ "uuid": uuid }),
        )
    }

    /// Edge Case: an empty mutation batch must not advance anything (consistent with
    /// `wal_flush_chunk`'s existing empty-chunk behavior) — issue #383.
    #[test]
    fn wal_flush_ungrouped_empty_batch_returns_none() {
        let wal_root = tempfile::tempdir().unwrap();
        let state = test_state(Some(wal_root.path().to_path_buf()));

        let result = wal_flush_ungrouped(&state, "g1", Vec::new());
        assert_eq!(
            result, None,
            "an empty mutation batch must return None, never a spurious seq"
        );
    }

    /// User Story 1 / SC-001: a batch of successfully-flushed mutations must return the
    /// highest seq actually assigned, exactly as `wal_flush_chunk` does for its own callers —
    /// issue #383's core fix.
    #[test]
    fn wal_flush_ungrouped_returns_max_seq_on_full_success() {
        let wal_root = tempfile::tempdir().unwrap();
        let state = test_state(Some(wal_root.path().to_path_buf()));

        let mutations = vec![mutation("a"), mutation("b"), mutation("c")];
        let seq = wal_flush_ungrouped(&state, "g1", mutations).expect("all 3 mutations must flush");
        assert_eq!(
            seq, 2,
            "3 mutations assigned seqs 0,1,2 — the returned value must be the highest, 2"
        );
    }

    /// User Story 3 / SC-002: a write to one group must never move another group's position.
    /// `wal_flush_ungrouped` itself doesn't touch `applied_seq` (its callers do, via
    /// `advance_applied_seq`), but the per-group isolation must already hold one layer down, at
    /// the returned-seq/writer level — group B's writer and returned seq must be completely
    /// unaffected by group A's flush.
    #[test]
    fn wal_flush_ungrouped_is_isolated_per_group() {
        let wal_root = tempfile::tempdir().unwrap();
        let state = test_state(Some(wal_root.path().to_path_buf()));

        // Establish group B's baseline position first.
        let b_seq_before = wal_flush_ungrouped(&state, "groupb", vec![mutation("b1")]);
        assert_eq!(b_seq_before, Some(0));

        // Flush several mutations into group A only.
        let a_seq = wal_flush_ungrouped(
            &state,
            "groupa",
            vec![mutation("a1"), mutation("a2"), mutation("a3")],
        );
        assert_eq!(a_seq, Some(2));

        // Group B's own writer state (global_seq) must be byte-identical to before group A's
        // write — group A's activity must not leak into group B's sequence space at all.
        let b_global_seq_after = state.with_wal_writer("groupb", |w| w.global_seq()).unwrap();
        assert_eq!(
            b_global_seq_after, 1,
            "group A's flush must not advance group B's writer state"
        );
    }

    /// SC-004 / FR-003 crash safety, specific to `wal_flush_ungrouped`'s per-mutation loop: a
    /// mutation whose `with_chunk` I/O fails (simulated deterministically by making the target
    /// WAL file unwritable) must not be credited — the returned seq must reflect only the
    /// durably-written prefix, never the failed mutation's phantom `global_seq` bump. This is
    /// the "never lead" half of the safety property `wal_flush_chunk` already has and
    /// `wal_flush_ungrouped` did not (issue #383).
    #[test]
    #[cfg(unix)]
    fn wal_flush_ungrouped_partial_failure_credits_only_durable_prefix() {
        use std::os::unix::fs::PermissionsExt;

        let wal_root = tempfile::tempdir().unwrap();
        let state = test_state(Some(wal_root.path().to_path_buf()));

        // Mutation 1 succeeds normally, creating group g1's WAL file.
        let seq1 = wal_flush_ungrouped(&state, "g1", vec![mutation("a")])
            .expect("first mutation must flush durably");
        assert_eq!(seq1, 0);

        // Find the file that was just written and strip write permission from it, so the next
        // attempt to append (same file — default max_events_per_file is large, so no rotation
        // happens between calls) fails with a real I/O error rather than a simulated one.
        let group_dir = wal_root.path().join("g1");
        let wal_file = std::fs::read_dir(&group_dir)
            .unwrap()
            .find_map(|e| {
                let entry = e.unwrap();
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
                    .then(|| entry.path())
            })
            .expect("first flush must have created exactly one WAL file");
        let mut perms = std::fs::metadata(&wal_file).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&wal_file, perms).unwrap();

        // Mutation 2 attempts to append to that now-read-only file and must fail. Despite
        // `WalWriter::log_mutation` having already bumped `global_seq` for it before the I/O
        // failure (the phantom-advance mechanic documented on `wal_flush_chunk`), the returned
        // seq from this call must not credit it.
        let seq2 = wal_flush_ungrouped(&state, "g1", vec![mutation("b")]);
        assert_eq!(
            seq2, None,
            "the only mutation in this call failed to flush durably — must return None, \
             never the phantom-bumped seq that was never actually written"
        );

        // Restore write access and confirm a subsequent, real success now returns a strictly
        // higher seq — a failure must trail, never permanently corrupt, the position.
        let mut perms = std::fs::metadata(&wal_file).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&wal_file, perms).unwrap();
        let seq3 = wal_flush_ungrouped(&state, "g1", vec![mutation("c")])
            .expect("a later, genuinely successful flush must still succeed");
        assert!(
            seq3 > seq1,
            "recovery after a transient failure must still make forward progress"
        );

        // The WAL file itself must contain only the durably-written mutations (a, c) — never
        // b, which failed.
        let contents = std::fs::read_to_string(&wal_file).unwrap();
        assert_eq!(
            contents.matches("\"uuid\":\"a\"").count() + contents.matches("\"uuid\":\"c\"").count(),
            2,
            "only the two durably-flushed mutations should be on disk"
        );
        assert_eq!(
            contents.matches("\"uuid\":\"b\"").count(),
            0,
            "the failed mutation must never have reached disk"
        );
    }
}
