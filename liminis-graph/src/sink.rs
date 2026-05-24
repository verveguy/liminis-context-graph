use std::sync::Arc;

use liminis_graph_core::telemetry::{TelemetryEvent, TelemetrySink};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;

pub struct StderrSink {
    tx: UnboundedSender<TelemetryEvent>,
}

impl StderrSink {
    /// Constructs `StderrSink`, spawns a background drain task, and returns
    /// `(Arc<Self>, JoinHandle<()>)`. The caller must await the handle after dropping
    /// all `Arc<StderrSink>` clones to ensure the final telemetry event flushes before exit.
    pub fn start() -> (Arc<Self>, JoinHandle<()>) {
        let (tx, mut rx) = unbounded_channel::<TelemetryEvent>();
        let handle = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Ok(json) = serde_json::to_string(&event) {
                    eprintln!("{json}");
                }
            }
        });
        (Arc::new(Self { tx }), handle)
    }
}

impl TelemetrySink for StderrSink {
    fn emit(&self, event: TelemetryEvent) {
        // Unbounded channel: non-blocking, never drops on backpressure (queue grows),
        // only drops the event when the receiver is gone (shutdown in progress).
        let _ = self.tx.send(event);
    }
}
