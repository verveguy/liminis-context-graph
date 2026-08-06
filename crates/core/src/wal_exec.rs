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

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::{
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
    wal::WalWriter,
};

fn emit_rotation_if_any(writer: &mut WalWriter, sink: &Arc<dyn TelemetrySink>) {
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

/// Flushes `cyphers` to WAL as a single chunk-atomic group.
///
/// Use for episode Phase C where all mutations for one chunk should land atomically.
pub(crate) fn wal_flush_chunk(
    wal: &Arc<Mutex<Option<WalWriter>>>,
    mutations: Vec<(String, serde_json::Value)>,
    sink: &Arc<dyn TelemetrySink>,
) {
    if mutations.is_empty() {
        return;
    }
    let mut guard = match wal.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("liminis-context-graph: wal_flush_chunk: lock poisoned: {e}");
            return;
        }
    };
    if let Some(ref mut writer) = *guard {
        let result = writer.with_chunk(|w| {
            for (cypher, params) in &mutations {
                w.log_mutation(cypher, wal_params(params), "")?;
            }
            Ok(())
        });
        match result {
            Ok(_) => emit_rotation_if_any(writer, sink),
            Err(e) => {
                eprintln!("liminis-context-graph: wal_flush_chunk: write failed (non-fatal): {e}")
            }
        }
    }
}

/// Flushes mutations to WAL as individual ungrouped entries (one `with_chunk` per mutation).
///
/// Use for delete handlers, corrections, and `handle_query_cypher`.
pub(crate) fn wal_flush_ungrouped(
    wal: &Arc<Mutex<Option<WalWriter>>>,
    mutations: Vec<(String, serde_json::Value)>,
    sink: &Arc<dyn TelemetrySink>,
) {
    if mutations.is_empty() {
        return;
    }
    let mut guard = match wal.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("liminis-context-graph: wal_flush_ungrouped: lock poisoned: {e}");
            return;
        }
    };
    if let Some(ref mut writer) = *guard {
        for (cypher, params) in &mutations {
            let result = writer.with_chunk(|w| w.log_mutation(cypher, wal_params(params), ""));
            match result {
                Ok(_) => emit_rotation_if_any(writer, sink),
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: wal_flush_ungrouped: write failed (non-fatal): {e}"
                    )
                }
            }
        }
    }
}

/// Re-derives `global_seq` after a non-dry-run WAL rebuild/replay completes (issue #352), so a
/// WAL directory populated after the writer was constructed doesn't leave the writer emitting
/// `seq` values that collide with what's already on disk. Non-fatal: a rebuild that already
/// succeeded shouldn't fail because of a re-derivation I/O error (e.g. the WAL dir vanished
/// between replay and this call), matching this module's existing failure posture.
///
/// Returns `true` if the resync completed without error (including the no-op case of no writer
/// configured), `false` if the on-disk scan failed. Callers pairing this with
/// `GlobalSeqResyncGuard` should only `mark_done()` on `true` — on `false` the guard's `Drop`-time
/// fallback (an on-disk-scan-only resync with no `last_committed_seq` floor) should stay armed as
/// a second chance, rather than being disarmed by a call that didn't actually update `global_seq`.
pub(crate) fn resync_global_seq_after_rebuild(
    wal: &Arc<Mutex<Option<WalWriter>>>,
    last_committed_seq: Option<u64>,
) -> bool {
    let mut guard = match wal.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("liminis-context-graph: resync_global_seq_after_rebuild: lock poisoned: {e}");
            return false;
        }
    };
    if let Some(ref mut writer) = *guard {
        if let Err(e) = writer.resync_global_seq(last_committed_seq) {
            eprintln!(
                "liminis-context-graph: resync_global_seq_after_rebuild: scan failed (non-fatal): {e}"
            );
            return false;
        }
    }
    true
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
    wal: &'a Arc<Mutex<Option<WalWriter>>>,
    done: bool,
}

impl<'a> GlobalSeqResyncGuard<'a> {
    pub(crate) fn new(wal: &'a Arc<Mutex<Option<WalWriter>>>) -> Self {
        Self { wal, done: false }
    }

    pub(crate) fn mark_done(&mut self) {
        self.done = true;
    }
}

impl Drop for GlobalSeqResyncGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            resync_global_seq_after_rebuild(self.wal, None);
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
    use super::*;

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
        let writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        let wal: Arc<Mutex<Option<WalWriter>>> = Arc::new(Mutex::new(Some(writer)));

        // Populated after construction; highest on-disk seq is 4 (next-seq-to-assign is 5).
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 4);

        {
            let _guard = GlobalSeqResyncGuard::new(&wal);
        }

        let global_seq = wal.lock().unwrap().as_ref().unwrap().global_seq();
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
        let writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        let wal: Arc<Mutex<Option<WalWriter>>> = Arc::new(Mutex::new(Some(writer)));

        resync_global_seq_after_rebuild(&wal, Some(99));
        // Written after the explicit resync above, so a broken mark_done (one that lets Drop's
        // fallback resync fire anyway) would pick this up and push global_seq past 100 — making
        // this test actually distinguish "resynced once" from "resynced twice".
        write_wal_line(tmp.path(), "late_0000.jsonl", 150);
        {
            let mut guard = GlobalSeqResyncGuard::new(&wal);
            guard.mark_done();
        }

        let global_seq = wal.lock().unwrap().as_ref().unwrap().global_seq();
        assert_eq!(
            global_seq, 100,
            "a marked-done guard must not alter global_seq again on drop"
        );
    }
}
