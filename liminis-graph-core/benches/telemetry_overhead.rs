// HOT-path overhead benchmark for telemetry emission.
// Measures overhead of NoopSink and channel-send (simulating StderrSink) per emit call.
// Constitution requirement: < 10 µs per event on the hot path.

use criterion::{criterion_group, criterion_main, Criterion};
use liminis_graph_core::telemetry::{CaptureSink, NoopSink, TelemetryEvent, TelemetrySink};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;

fn make_ipc_event() -> TelemetryEvent {
    TelemetryEvent::IpcCall {
        ts_ms: 1716100000000,
        method: "knowledge_find_entities".to_string(),
        request_id: Value::Number(1.into()),
        duration_ms: 42,
        success: true,
    }
}

fn bench_noop_sink(c: &mut Criterion) {
    let sink = NoopSink;
    c.bench_function("noop_sink_emit", |b| {
        b.iter(|| {
            sink.emit(make_ipc_event());
        });
    });
}

fn bench_channel_send(c: &mut Criterion) {
    // Simulate StderrSink channel-send without draining (measures the send side only).
    let (tx, _rx) = unbounded_channel::<TelemetryEvent>();
    c.bench_function("channel_send_emit", |b| {
        b.iter(|| {
            let _ = tx.send(make_ipc_event());
        });
    });
}

fn bench_capture_sink(c: &mut Criterion) {
    let sink = Arc::new(CaptureSink::new());
    c.bench_function("capture_sink_emit", |b| {
        b.iter(|| {
            sink.emit(make_ipc_event());
        });
    });
}

criterion_group!(benches, bench_noop_sink, bench_channel_send, bench_capture_sink);
criterion_main!(benches);
