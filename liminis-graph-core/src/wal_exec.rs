//! WAL-flush helpers for write handlers. See ADR-001 for the drain-and-flush pattern.
//!
//! Every write handler records Cypher via `Conn::raw_query` / `Conn::cypher_query`,
//! then calls one of these helpers with `conn.drain_cyphers()` after the writes succeed.
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
    cyphers: Vec<String>,
    sink: &Arc<dyn TelemetrySink>,
) {
    if cyphers.is_empty() {
        return;
    }
    let mut guard = match wal.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("liminis-graph: wal_flush_chunk: lock poisoned: {e}");
            return;
        }
    };
    if let Some(ref mut writer) = *guard {
        let result = writer.with_chunk(|w| {
            for c in &cyphers {
                w.log_mutation(c, json!({}), "")?;
            }
            Ok(())
        });
        match result {
            Ok(_) => emit_rotation_if_any(writer, sink),
            Err(e) => eprintln!("liminis-graph: wal_flush_chunk: write failed (non-fatal): {e}"),
        }
    }
}

/// Flushes `cyphers` to WAL as individual ungrouped mutations (one `with_chunk` per cypher).
///
/// Use for delete handlers, corrections, and `handle_query_cypher`.
pub(crate) fn wal_flush_ungrouped(
    wal: &Arc<Mutex<Option<WalWriter>>>,
    cyphers: Vec<String>,
    sink: &Arc<dyn TelemetrySink>,
) {
    if cyphers.is_empty() {
        return;
    }
    let mut guard = match wal.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("liminis-graph: wal_flush_ungrouped: lock poisoned: {e}");
            return;
        }
    };
    if let Some(ref mut writer) = *guard {
        for c in &cyphers {
            let result = writer.with_chunk(|w| w.log_mutation(c, json!({}), ""));
            match result {
                Ok(_) => emit_rotation_if_any(writer, sink),
                Err(e) => {
                    eprintln!("liminis-graph: wal_flush_ungrouped: write failed (non-fatal): {e}")
                }
            }
        }
    }
}
