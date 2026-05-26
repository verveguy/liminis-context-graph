use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

// ── Event enum ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEvent {
    IpcCall {
        ts_ms: u64,
        method: String,
        request_id: Value,
        duration_ms: u64,
        success: bool,
    },
    TokenUsage {
        ts_ms: u64,
        role: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        estimated_cost_usd: Option<f64>,
    },
    LlmFallback {
        ts_ms: u64,
        role: String,
        primary_model: String,
        fallback_model: String,
        error_reason: String,
    },
    WalAppend {
        ts_ms: u64,
        duration_us: u64,
        bytes: usize,
    },
    WalReplayComplete {
        ts_ms: u64,
        episodes_replayed: u64,
        duration_ms: u64,
        throughput_eps: f64,
    },
    ServiceState {
        ts_ms: u64,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<serde_json::Value>,
    },
    ExtractionTruncated {
        ts_ms: u64,
        model: String,
        chunk_len_bytes: usize,
        initial_max_tokens: u32,
        retry_succeeded: bool,
    },
    WalRotated {
        ts_ms: u64,
        from_file_seq: u32,
        to_file_seq: u32,
        closed_bytes: u64,
        closed_events: u64,
    },
    WorkspaceMigration {
        ts_ms: u64,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<serde_json::Value>,
    },
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Sink trait ───────────────────────────────────────────────────────────────

pub trait TelemetrySink: Send + Sync {
    fn emit(&self, event: TelemetryEvent);
}

// ── NoopSink ─────────────────────────────────────────────────────────────────

pub struct NoopSink;

impl TelemetrySink for NoopSink {
    fn emit(&self, _event: TelemetryEvent) {}
}

// ── CaptureSink (for tests) ──────────────────────────────────────────────────

pub struct CaptureSink {
    events: Mutex<Vec<TelemetryEvent>>,
}

impl CaptureSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<TelemetryEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl Default for CaptureSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySink for CaptureSink {
    fn emit(&self, event: TelemetryEvent) {
        self.events.lock().unwrap().push(event);
    }
}

// ── Pricing / cost calculation ───────────────────────────────────────────────

const COMPILED_PRICING: &str = include_str!("../../assets/llm_pricing.json");

fn load_pricing() -> &'static Value {
    static PRICING: OnceLock<Value> = OnceLock::new();
    PRICING.get_or_init(|| {
        if let Ok(path) = std::env::var("LIMINIS_LLM_COST_TABLE_PATH") {
            match std::fs::read_to_string(&path).and_then(|s| {
                serde_json::from_str::<Value>(&s)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            }) {
                Ok(v) => return v,
                Err(e) => eprintln!(
                    "liminis-graph: LIMINIS_LLM_COST_TABLE_PATH={path} unreadable or invalid JSON, \
                     using built-in pricing: {e}"
                ),
            }
        }
        serde_json::from_str(COMPILED_PRICING).unwrap_or(Value::Object(Default::default()))
    })
}

/// Returns estimated cost in USD, or `None` if the model is not in the pricing table.
pub fn cost_for_usage(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> Option<f64> {
    let table = load_pricing();
    let entry = table.get(model)?;
    let input_rate = entry["input_per_mtok"].as_f64()?;
    let output_rate = entry["output_per_mtok"].as_f64()?;
    let cache_read_rate = entry["cache_read_per_mtok"].as_f64()?;
    let cache_creation_rate = entry["cache_creation_per_mtok"].as_f64()?;

    let cost = (input_tokens as f64 * input_rate
        + output_tokens as f64 * output_rate
        + cache_read_tokens as f64 * cache_read_rate
        + cache_creation_tokens as f64 * cache_creation_rate)
        / 1_000_000.0;

    Some(cost)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_sink_stores_events() {
        let sink = CaptureSink::new();
        sink.emit(TelemetryEvent::IpcCall {
            ts_ms: 0,
            method: "knowledge_find_entities".to_string(),
            request_id: Value::Number(1.into()),
            duration_ms: 10,
            success: true,
        });
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            TelemetryEvent::IpcCall { success: true, .. }
        ));
    }

    #[test]
    fn cost_for_usage_known_model() {
        let cost = cost_for_usage("claude-haiku-4-5-20251001", 1_000_000, 0, 0, 0);
        assert!(cost.is_some());
        assert!((cost.unwrap() - 0.80).abs() < 1e-9);
    }

    #[test]
    fn cost_for_usage_cache_tokens() {
        // 1M cache-read tokens at $0.08/MTok
        let cost = cost_for_usage("claude-haiku-4-5-20251001", 0, 0, 1_000_000, 0);
        assert!(cost.is_some());
        assert!((cost.unwrap() - 0.08).abs() < 1e-9);
    }

    #[test]
    fn cost_for_usage_unknown_model() {
        let cost = cost_for_usage("unknown-model-xyz", 1000, 100, 0, 0);
        assert!(cost.is_none());
    }

    #[test]
    fn noop_sink_does_not_panic() {
        let sink = NoopSink;
        sink.emit(TelemetryEvent::WalAppend {
            ts_ms: 0,
            duration_us: 1,
            bytes: 512,
        });
    }
}
