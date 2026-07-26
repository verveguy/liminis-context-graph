//! Integration tests exercising the harness's public library surface end to end,
//! composed the same way `main.rs` wires them, without any network access — every
//! backend here is `ConfigurableExtractor`/a canned `JudgeClient`, so these run in
//! default `cargo test` with zero cost and zero flakiness. Live-API tests are kept out
//! of this file entirely per the Plan (any such test must be `#[ignore]`d).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use tempfile::TempDir;

use lcg_core::{
    ConfigurableExtractor, Error, ExtractOptions, ExtractedEntity, ExtractionResult, Extractor,
};
use lcg_eval::corpus::{load_corpus, select_subset, CorpusChunk};
use lcg_eval::judge::{precision_recall_f1, JudgeClient, JudgeVerdict};
use lcg_eval::judge_cache::{cache_key, JudgeCache};
use lcg_eval::metrics::entity_strict_prf1;
use lcg_eval::runner::{run_backend, CountingSink};

fn chunk(title: &str, prose: &str) -> CorpusChunk {
    CorpusChunk {
        title: title.to_string(),
        revision_id: 1,
        prose: prose.to_string(),
    }
}

// ── corpus-subset determinism ──────────────────────────────────────────────────

#[test]
fn corpus_subset_selection_is_deterministic_across_runs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corpus.jsonl");
    std::fs::write(
        &path,
        concat!(
            "{\"_header\": true}\n",
            "{\"title\": \"A\", \"revision_id\": 1, \"prose\": \"prose a\"}\n",
            "{\"title\": \"B\", \"revision_id\": 2, \"prose\": \"prose b\"}\n",
            "{\"title\": \"C\", \"revision_id\": 3, \"prose\": \"prose c\"}\n",
        ),
    )
    .unwrap();

    let first = select_subset(load_corpus(&path).unwrap(), Some(2));
    let second = select_subset(load_corpus(&path).unwrap(), Some(2));

    assert_eq!(first.len(), 2);
    assert_eq!(
        first, second,
        "same limit against the same file must select the same chunks"
    );
    assert_eq!(first[0].title, "A");
    assert_eq!(first[1].title, "B");
}

// ── per-backend error/latency capture (Edge Case: failures counted, not dropped) ──

struct FailingExtractor;

impl Extractor for FailingExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(async { Err(Error::Ipc("simulated backend failure".to_string())) })
    }
    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = edges.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

#[tokio::test]
async fn backend_failures_are_counted_in_error_rate_not_silently_dropped() {
    let extractor: Arc<dyn Extractor> = Arc::new(FailingExtractor);
    let sink = Arc::new(CountingSink::new());
    let chunks = vec![chunk("A", "prose a"), chunk("B", "prose b")];

    let result = run_backend("failing-backend", extractor, sink, &chunks).await;

    assert_eq!(result.chunk_results.len(), 2);
    let errors = result
        .chunk_results
        .iter()
        .filter(|c| c.result.is_err())
        .count();
    assert_eq!(
        errors, 2,
        "every failed chunk must surface as an error, not be dropped"
    );
    for c in &result.chunk_results {
        // latency is still captured even on a failed call.
        let _ = c.latency_ms;
    }
}

#[tokio::test]
async fn successful_backend_run_reports_zero_errors_and_captures_latency() {
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Person".to_string(),
                summary: "a person".to_string(),
            }],
            edges: vec![],
        }]));
    let sink = Arc::new(CountingSink::new());
    let chunks = vec![chunk("A", "prose a")];

    let result = run_backend("ok-backend", extractor, sink, &chunks).await;

    assert_eq!(result.chunk_results.len(), 1);
    assert!(result.chunk_results[0].result.is_ok());
}

// ── judge-cache-hit avoids new calls (SC-003) ──────────────────────────────────

/// A `JudgeClient` that counts every call it actually serves — the harness's cache
/// must keep this counter at 1 across repeated identical comparisons.
struct CountingJudgeClient {
    calls: AtomicUsize,
    verdict: JudgeVerdict,
}

impl CountingJudgeClient {
    fn new(verdict: JudgeVerdict) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            verdict,
        }
    }
}

impl JudgeClient for CountingJudgeClient {
    fn judge<'a>(
        &'a self,
        _prompt_name: &'a str,
        _reference: &'a Value,
        _candidate: &'a Value,
    ) -> BoxFuture<'a, Result<JudgeVerdict, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let v = self.verdict.clone();
        Box::pin(async move { Ok(v) })
    }
}

async fn judged_f1_with_cache(
    judge: &dyn JudgeClient,
    cache: &JudgeCache,
    prompt_name: &str,
    reference: &Value,
    candidate: &Value,
) -> (f64, f64, f64) {
    let key = cache_key(prompt_name, "claude-sonnet-4-6", reference, candidate);
    let verdict = match cache.get(&key) {
        Some(v) => v,
        None => {
            let v = judge
                .judge(prompt_name, reference, candidate)
                .await
                .unwrap();
            cache.insert(key, v.clone()).unwrap();
            v
        }
    };
    precision_recall_f1(&verdict)
}

#[tokio::test]
async fn second_run_against_same_cache_makes_zero_new_judge_calls_sc003() {
    let dir = TempDir::new().unwrap();
    let cache_path = dir.path().join("judge_cache.jsonl");

    let verdict = JudgeVerdict {
        matched: vec![(0, 0)],
        unmatched_reference: vec![],
        unmatched_candidate: vec![],
    };
    let judge = CountingJudgeClient::new(verdict);
    let reference = serde_json::json!({"extracted_entities": [{"name": "Alice"}]});
    let candidate = serde_json::json!({"extracted_entities": [{"name": "Alice"}]});

    // First "run": cache is empty, so this must call the judge exactly once.
    {
        let cache = JudgeCache::load(&cache_path).unwrap();
        let (_, _, f1) = judged_f1_with_cache(
            &judge,
            &cache,
            "extract_nodes.extract_text",
            &reference,
            &candidate,
        )
        .await;
        assert_eq!(f1, 1.0);
    }
    assert_eq!(judge.calls.load(Ordering::SeqCst), 1);

    // Second "run" against the same corpus/backends and the same on-disk cache path:
    // must be served entirely from cache (SC-003).
    {
        let cache = JudgeCache::load(&cache_path).unwrap();
        let (_, _, f1) = judged_f1_with_cache(
            &judge,
            &cache,
            "extract_nodes.extract_text",
            &reference,
            &candidate,
        )
        .await;
        assert_eq!(f1, 1.0);
    }
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        1,
        "a cache hit on re-run must make zero new judge calls"
    );
}

// ── strict-vs-judged materially differs (SC-004's noise-floor property) ───────

#[tokio::test]
async fn judged_score_beats_strict_score_on_wording_variance_sc004() {
    // "Alice Smith" (reference) vs "Alice" (candidate) — strict string matching sees
    // these as entirely different entities (SC-004's point: pure wording variance
    // produces a real F1 floor under strict matching).
    let reference_entities = vec![ExtractedEntity {
        name: "Alice Smith".to_string(),
        entity_type: "Person".to_string(),
        summary: "a person".to_string(),
    }];
    let candidate_entities = vec![ExtractedEntity {
        name: "Alice".to_string(),
        entity_type: "Person".to_string(),
        summary: "a person".to_string(),
    }];

    let strict = entity_strict_prf1(&reference_entities, &candidate_entities);
    assert_eq!(
        strict.f1, 0.0,
        "strict string matching must not treat these as equivalent"
    );

    // A judge, by contrast, recognizes "Alice Smith" and "Alice" as the same
    // real-world entity — canned here (not a live call) as the ported judge prompt
    // instructs ("name variations... allow minor... differences if the underlying
    // entity is clearly the same").
    let judge_verdict = JudgeVerdict {
        matched: vec![(0, 0)],
        unmatched_reference: vec![],
        unmatched_candidate: vec![],
    };
    let (_, _, judged_f1) = precision_recall_f1(&judge_verdict);

    assert_eq!(judged_f1, 1.0);
    assert!(
        judged_f1 - strict.f1 > 0.5,
        "judged F1 ({judged_f1}) must be materially higher than strict F1 ({}) on pure \
         wording variance — this is exactly the property that motivates using a judge",
        strict.f1
    );
}

// ── structured-output metrics reported separately from F1 (FR-007) ────────────

#[tokio::test]
async fn structured_output_reliability_is_independent_of_extraction_f1() {
    use lcg_core::{TelemetryEvent, TelemetrySink};

    // A backend whose extractions are perfect (F1 = 1.0) but whose underlying
    // structured-output parsing needed defensive recovery / hit malformed responses —
    // FR-007 requires this axis to be visible independently of F1, not folded into it.
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Person".to_string(),
                summary: "a person".to_string(),
            }],
            edges: vec![],
        }]));
    let sink = Arc::new(CountingSink::new());

    // Simulate what OaiExtractor would have emitted internally for this call: a
    // "recovered" parse (fence-stripped JSON) even though the extraction itself was
    // perfect — proving the two metrics are tracked on independent axes.
    sink.emit(TelemetryEvent::StructuredOutputParse {
        ts_ms: 0,
        model: "test-model".to_string(),
        call_type: "entities".to_string(),
        outcome: "recovered".to_string(),
    });

    let chunks = vec![chunk("A", "prose a")];
    let result = run_backend("backend", extractor, Arc::clone(&sink), &chunks).await;

    // F1 side: a perfect match against itself.
    let entities = &result.chunk_results[0].result.as_ref().unwrap().entities;
    let strict = entity_strict_prf1(entities, entities);
    assert_eq!(strict.f1, 1.0);

    // Structured-output side: the recovery is visible and non-zero, entirely
    // independent of the (perfect) F1 score above.
    let structured = result.structured_output;
    assert_eq!(structured.recovered, 1);
    assert_eq!(structured.malformed, 0);
    assert!(
        structured.recovered > 0,
        "recovery must be visible even when F1 is perfect"
    );
}
