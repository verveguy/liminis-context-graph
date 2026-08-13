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
//! - `wal_flush_ungrouped`: delete/corrections/cypher handlers — one `with_chunk` per
//!   cypher so each mutation is independently flushed.

use serde_json::json;

use crate::{
    app_state::AppState,
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
    wal::WalWriter,
};

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
                    eprintln!("liminis-context-graph: wal_flush_chunk: write failed (non-fatal): {e}");
                    None
                }
            }
        })
        .flatten()
}

/// Flushes mutations to `group_id`'s own WAL directory as individual ungrouped entries (one
/// `with_chunk` per mutation).
///
/// Use for delete handlers, corrections, and `handle_query_cypher`.
pub(crate) fn wal_flush_ungrouped(
    state: &AppState,
    group_id: &str,
    mutations: Vec<(String, serde_json::Value)>,
) {
    if mutations.is_empty() {
        return;
    }
    state.with_wal_writer(group_id, |writer| {
        for (cypher, params) in &mutations {
            let result = writer.with_chunk(|w| w.log_mutation(cypher, wal_params(params), ""));
            match result {
                Ok(_) => emit_rotation_if_any(writer, state.sink.as_ref()),
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: wal_flush_ungrouped: write failed (non-fatal): {e}"
                    )
                }
            }
        }
    });
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
}
