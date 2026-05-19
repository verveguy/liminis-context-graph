use std::sync::Arc;

use liminis_graph_core::telemetry::{TelemetryEvent, TelemetrySink};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub struct StderrSink {
    tx: UnboundedSender<TelemetryEvent>,
}

impl StderrSink {
    /// Constructs `StderrSink`, spawns a background drain task, and returns `Arc<Self>`.
    pub fn start() -> Arc<Self> {
        let (tx, mut rx) = unbounded_channel::<TelemetryEvent>();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Ok(json) = serde_json::to_string(&event) {
                    eprintln!("{json}");
                }
            }
        });
        Arc::new(Self { tx })
    }
}

impl TelemetrySink for StderrSink {
    fn emit(&self, event: TelemetryEvent) {
        // Non-blocking send; drop if receiver is gone (shutdown in progress).
        let _ = self.tx.send(event);
    }
}
