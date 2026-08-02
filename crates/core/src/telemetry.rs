use std::sync::{Arc, Mutex, OnceLock};
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
        mutations_replayed: u64,
        unrecognised_lines: u64,
        failed_lines: u64,
        unparseable_lines: u64,
        legacy_skipped_lines: u64,
        duration_ms: u64,
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
        /// Identifies which chunk produced this truncation (#306 FR-004) — `chunk.title` in
        /// the eval harness, the episode `name` in production. `None` when the caller's
        /// `ExtractOptions.chunk_key` was itself `None`.
        chunk_key: Option<String>,
    },
    /// Emitted by `OaiExtractor` for every entity/edge extraction response, distinguishing
    /// whether the structured-output JSON was well-formed as-is (`"clean"`), required
    /// `extract_json_block`'s fence/prefix stripping to parse (`"recovered"`), was not valid
    /// JSON at all (`"malformed"`), or parsed as valid JSON but failed schema/field validation
    /// on a genuinely required field (`"schema_invalid"`, #314 FR-003). `call_type` is
    /// `"entities"` or `"edges"`.
    StructuredOutputParse {
        ts_ms: u64,
        model: String,
        call_type: String,
        outcome: String,
    },
    /// The heavier, complete-body sibling of `ExtractionTruncated`/`StructuredOutputParse`
    /// (#306 FR-001/FR-002) — emitted at all three extraction-call failure sites (HTTP error,
    /// budget-exhaustion-after-retry, and parse/malformed error) from inside
    /// `AnthropicExtractor`/`OaiExtractor`. Consumed only by the sidecar-writing
    /// `extraction_failures::ExtractionFailureSink`; `StructuredOutputParse`/
    /// `ExtractionTruncated` stay lightweight, counting-only events for `CountingSink`.
    ExtractionFailure {
        ts_ms: u64,
        model: String,
        /// `"entities"` or `"edges"`.
        call_type: String,
        chunk_key: Option<String>,
        /// `"http_error"`, `"truncation"`, `"malformed"`, or `"schema_invalid"` (#314 FR-003:
        /// valid JSON that failed schema/field validation, as opposed to `"malformed"`, which is
        /// content that never parsed as JSON at all).
        classification: String,
        /// The complete raw response body — never truncated to a prefix (FR-002). A UTF-8
        /// decoding failure is stored lossily rather than dropping the record.
        raw_body: String,
        finish_reason: Option<String>,
        completion_tokens: Option<u64>,
        max_tokens: u32,
        /// #307 FR-007: the count of entities already extracted for this chunk before the edge
        /// call failed, so an edge-exhaustion `Err` (which discards those entities from the
        /// caller's return value) still leaves them recoverable for forensics. `Some(count)` at
        /// every `call_type: "edges"` failure site (entities always succeed before edges run);
        /// `None` at every `call_type: "entities"` site, where there is nothing to report yet.
        #[serde(default)]
        entities_extracted: Option<usize>,
    },
    /// Emitted by `OaiExtractor::do_extract_entities` on a successful parse (#314 FR-004) —
    /// the Anthropic path can never produce this, since its `tool_use` schema's `"required":
    /// ["name", "entity_type", "summary"]` structurally prevents an empty summary from reaching
    /// this point. Surfaces the missing-summary rate as its own signal, separate from the
    /// pass/fail classification in `StructuredOutputParse`/`ExtractionFailure`: an empty summary
    /// is a degraded entity, not a failed extraction.
    EntitiesMissingSummary {
        ts_ms: u64,
        model: String,
        chunk_key: Option<String>,
        entities_extracted: usize,
        /// Count of entities in this chunk whose `summary` is empty — absent, explicit `null`,
        /// or explicit `""` in the source JSON are all indistinguishable after parsing (#314
        /// Edge Cases) and all count here.
        missing_summary: usize,
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
    /// Emitted at each phase of autonomous WAL-corruption self-recovery.
    /// Valid `phase` values: `"corruption_detected"`, `"checkpoint_drop_complete"`,
    /// `"cursor_derived"`, `"replay_complete"`, `"index_build_complete"`,
    /// `"recovery_complete"`, `"fallback_triggered"`.
    WalAutoRecovery {
        ts_ms: u64,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        from_seq: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cursor_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mutations_replayed: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fallback_reason: Option<String>,
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

// ── TeeSink ──────────────────────────────────────────────────────────────────

/// Fans out every event to each sink in `sinks`, in order. Lets a `RecordingExtractor`
/// construction site install both the pre-existing telemetry sink and a new
/// `extraction_failures::ExtractionFailureSink` without either needing to know about the
/// other (#306 FR-001).
pub struct TeeSink {
    sinks: Vec<Arc<dyn TelemetrySink>>,
}

impl TeeSink {
    pub fn new(sinks: Vec<Arc<dyn TelemetrySink>>) -> Self {
        Self { sinks }
    }
}

impl TelemetrySink for TeeSink {
    fn emit(&self, event: TelemetryEvent) {
        // Clone for every sink but the last, which takes the original — avoids one needless
        // clone of a potentially-large `ExtractionFailure` payload per `emit` call.
        if let Some((last, rest)) = self.sinks.split_last() {
            for sink in rest {
                sink.emit(event.clone());
            }
            last.emit(event);
        }
    }
}

// ── Pricing / cost calculation ───────────────────────────────────────────────

const COMPILED_PRICING: &str = include_str!("../../../assets/llm_pricing.json");

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
                    "liminis-context-graph: LIMINIS_LLM_COST_TABLE_PATH={path} unreadable or invalid JSON, \
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

    #[test]
    fn tee_sink_forwards_to_every_sink() {
        let a = Arc::new(CaptureSink::new());
        let b = Arc::new(CaptureSink::new());
        let tee = TeeSink::new(vec![
            Arc::clone(&a) as Arc<dyn TelemetrySink>,
            Arc::clone(&b) as Arc<dyn TelemetrySink>,
        ]);
        tee.emit(TelemetryEvent::WalAppend {
            ts_ms: 0,
            duration_us: 1,
            bytes: 512,
        });
        assert_eq!(a.events().len(), 1);
        assert_eq!(b.events().len(), 1);
    }

    #[test]
    fn tee_sink_with_no_sinks_does_not_panic() {
        let tee = TeeSink::new(vec![]);
        tee.emit(TelemetryEvent::WalAppend {
            ts_ms: 0,
            duration_us: 1,
            bytes: 512,
        });
    }
}
