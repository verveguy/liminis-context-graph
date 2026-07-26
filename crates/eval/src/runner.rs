//! Runs a configured backend's `Extractor::extract` over every selected corpus chunk,
//! capturing per-chunk latency and errors (Edge Case: an outright request failure is
//! counted in the error-rate metric, never silently dropped) plus a per-backend tally of
//! `StructuredOutputParse` telemetry (clean/recovered/malformed — FR-007).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lcg_core::{
    ExtractOptions, ExtractionResult, Extractor, SourceType, TelemetryEvent, TelemetrySink,
};

use crate::corpus::CorpusChunk;

/// A fixed, deterministic reference time for every extraction call in a harness run —
/// the eval measures extraction quality, not time-relative-fact resolution, so a
/// constant avoids introducing wall-clock nondeterminism into judge/strict scoring.
pub const REFERENCE_TIME: &str = "2026-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StructuredOutputCounts {
    pub clean: u64,
    pub recovered: u64,
    pub malformed: u64,
}

impl StructuredOutputCounts {
    pub fn total(&self) -> u64 {
        self.clean + self.recovered + self.malformed
    }

    pub fn malformed_rate(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            self.malformed as f64 / self.total() as f64
        }
    }
}

/// A `TelemetrySink` that tallies `StructuredOutputParse` events and discards everything
/// else — the harness's seam for FR-007's structured-output-reliability metric.
pub struct CountingSink {
    counts: Mutex<StructuredOutputCounts>,
}

impl CountingSink {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(StructuredOutputCounts::default()),
        }
    }

    pub fn snapshot(&self) -> StructuredOutputCounts {
        *self.counts.lock().unwrap()
    }
}

impl Default for CountingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySink for CountingSink {
    fn emit(&self, event: TelemetryEvent) {
        if let TelemetryEvent::StructuredOutputParse { outcome, .. } = event {
            let mut counts = self.counts.lock().unwrap();
            match outcome.as_str() {
                "clean" => counts.clean += 1,
                "recovered" => counts.recovered += 1,
                "malformed" => counts.malformed += 1,
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub chunk_title: String,
    pub latency_ms: u64,
    /// `Err` carries the stringified error (Edge Case: never silently dropped — every
    /// backend request failure surfaces here for the error-rate metric).
    pub result: Result<ExtractionResult, String>,
}

#[derive(Debug, Clone)]
pub struct BackendRunResult {
    pub backend_name: String,
    pub chunk_results: Vec<ChunkResult>,
    pub structured_output: StructuredOutputCounts,
}

/// Runs `extractor` over every chunk in `chunks`, sequentially (a corpus run is already
/// bounded by the default 50-chunk subset; sequential execution keeps rate-limit/latency
/// measurement simple and avoids surprising a locally-hosted single-worker backend with
/// concurrent load).
pub async fn run_backend(
    name: &str,
    extractor: Arc<dyn Extractor>,
    sink: Arc<CountingSink>,
    chunks: &[CorpusChunk],
) -> BackendRunResult {
    let mut chunk_results = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let opts = ExtractOptions {
            episode_body: &chunk.prose,
            group_id: "eval",
            source_type: SourceType::Text,
            custom_instructions: None,
            reference_time: REFERENCE_TIME,
            ontology: None,
        };
        let start = Instant::now();
        let result = extractor.extract(opts).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        chunk_results.push(ChunkResult {
            chunk_title: chunk.title.clone(),
            latency_ms,
            result: result.map_err(|e| e.to_string()),
        });
    }
    BackendRunResult {
        backend_name: name.to_string(),
        chunk_results,
        structured_output: sink.snapshot(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcg_core::{ConfigurableExtractor, ExtractedEntity};

    fn chunk(title: &str, prose: &str) -> CorpusChunk {
        CorpusChunk {
            title: title.to_string(),
            revision_id: 1,
            prose: prose.to_string(),
        }
    }

    fn extraction(entity_count: usize) -> ExtractionResult {
        ExtractionResult {
            entities: (0..entity_count)
                .map(|i| ExtractedEntity {
                    name: format!("Entity{i}"),
                    entity_type: "Thing".to_string(),
                    summary: "a thing".to_string(),
                })
                .collect(),
            edges: vec![],
        }
    }

    #[tokio::test]
    async fn captures_latency_and_results_per_chunk() {
        let extractor: Arc<dyn Extractor> = Arc::new(ConfigurableExtractor::new(vec![
            extraction(1),
            extraction(2),
        ]));
        let sink = Arc::new(CountingSink::new());
        let chunks = vec![chunk("A", "prose a"), chunk("B", "prose b")];

        let result = run_backend("mock", extractor, sink, &chunks).await;

        assert_eq!(result.backend_name, "mock");
        assert_eq!(result.chunk_results.len(), 2);
        assert_eq!(result.chunk_results[0].chunk_title, "A");
        assert_eq!(
            result.chunk_results[0]
                .result
                .as_ref()
                .unwrap()
                .entities
                .len(),
            1
        );
        assert_eq!(
            result.chunk_results[1]
                .result
                .as_ref()
                .unwrap()
                .entities
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn errors_are_counted_not_dropped() {
        struct FailingExtractor;
        impl Extractor for FailingExtractor {
            fn extract<'a>(
                &'a self,
                _opts: ExtractOptions<'a>,
            ) -> futures::future::BoxFuture<'a, Result<ExtractionResult, lcg_core::Error>>
            {
                Box::pin(async { Err(lcg_core::Error::Ipc("boom".to_string())) })
            }
            fn classify_entities<'a>(
                &'a self,
                entities: &'a [(&'a str, &'a str)],
                _allowed_types: Option<&'a [String]>,
            ) -> futures::future::BoxFuture<'a, Result<Vec<String>, lcg_core::Error>> {
                let count = entities.len();
                Box::pin(async move { Ok(vec![String::new(); count]) })
            }
            fn classify_relations<'a>(
                &'a self,
                edges: &'a [(&'a str, &'a str)],
                _allowed_types: &'a [(String, Option<String>)],
            ) -> futures::future::BoxFuture<'a, Result<Vec<String>, lcg_core::Error>> {
                let count = edges.len();
                Box::pin(async move { Ok(vec![String::new(); count]) })
            }
        }

        let extractor: Arc<dyn Extractor> = Arc::new(FailingExtractor);
        let sink = Arc::new(CountingSink::new());
        let chunks = vec![chunk("A", "prose a")];

        let result = run_backend("failing", extractor, sink, &chunks).await;
        assert_eq!(result.chunk_results.len(), 1);
        let err = result.chunk_results[0].result.as_ref().unwrap_err();
        assert!(err.contains("boom"));
    }

    #[test]
    fn counting_sink_tallies_by_outcome() {
        let sink = CountingSink::new();
        for outcome in ["clean", "clean", "recovered", "malformed"] {
            sink.emit(TelemetryEvent::StructuredOutputParse {
                ts_ms: 0,
                model: "test".to_string(),
                call_type: "entities".to_string(),
                outcome: outcome.to_string(),
            });
        }
        let snapshot = sink.snapshot();
        assert_eq!(snapshot.clean, 2);
        assert_eq!(snapshot.recovered, 1);
        assert_eq!(snapshot.malformed, 1);
        assert_eq!(snapshot.total(), 4);
        assert!((snapshot.malformed_rate() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn counting_sink_ignores_other_events() {
        let sink = CountingSink::new();
        sink.emit(TelemetryEvent::WalAppend {
            ts_ms: 0,
            duration_us: 1,
            bytes: 1,
        });
        assert_eq!(sink.snapshot().total(), 0);
    }

    #[test]
    fn malformed_rate_is_zero_with_no_events() {
        let counts = StructuredOutputCounts::default();
        assert_eq!(counts.malformed_rate(), 0.0);
    }
}
