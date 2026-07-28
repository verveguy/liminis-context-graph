//! Reference-mode scoring loop (#273): drives one candidate's chunk-by-chunk comparison
//! against the reference backend's output, both by strict string match and (optionally) an
//! LLM-as-judge. Moved here verbatim from `main.rs` so it is reachable from tests without a
//! network call — see `pairwise.rs` for the sibling pairwise-mode scoring path, which made
//! the same move when it was added.

use crate::judge::{precision_recall_f1, JudgeClient, JudgeVerdict};
use crate::judge_cache::{cache_key, JudgeCache};
use crate::metrics::{edge_strict_prf1, entity_strict_prf1};
use crate::report::{
    percentiles, CandidateReport, StructuredOutputReliability, VocabularyComplianceReport,
};
use crate::runner::{BackendRunResult, VocabularyComplianceCounts};

pub async fn score_candidate(
    run_result: &BackendRunResult,
    reference_result: &BackendRunResult,
    judge_client: Option<&dyn JudgeClient>,
    judge_cache: &JudgeCache,
    judge_model: &str,
) -> Result<CandidateReport, String> {
    let latencies: Vec<u64> = run_result
        .chunk_results
        .iter()
        .map(|c| c.latency_ms)
        .collect();
    let errors = run_result
        .chunk_results
        .iter()
        .filter(|c| c.result.is_err())
        .count();
    let error_rate = if run_result.chunk_results.is_empty() {
        0.0
    } else {
        errors as f64 / run_result.chunk_results.len() as f64
    };

    let mut ref_entities_all = Vec::new();
    let mut cand_entities_all = Vec::new();
    let mut ref_edges_all = Vec::new();
    let mut cand_edges_all = Vec::new();
    let mut judged_entity_f1s = Vec::new();
    let mut judged_entity_precisions = Vec::new();
    let mut judged_entity_recalls = Vec::new();
    let mut judged_edge_f1s = Vec::new();
    let mut judged_edge_precisions = Vec::new();
    let mut judged_edge_recalls = Vec::new();
    let mut judged_summary_f1s = Vec::new();
    // Judge calls that failed after their retry ladder. Counted and reported rather than
    // propagated: extraction failures are already per-chunk, and a scoring phase is ~1340
    // sequential judge calls, so making one bad response fatal means a single hiccup
    // discards hours of work. It did exactly that to the #248 run.
    let mut judge_errors = 0usize;
    // Chunks where both reference and candidate extraction succeeded — the denominator
    // strict/judged F1 are actually computed over. Distinct from `chunks_run`, which
    // counts every chunk the harness attempted regardless of whether either side errored.
    let mut chunks_scored = 0usize;

    for (ref_chunk, cand_chunk) in reference_result
        .chunk_results
        .iter()
        .zip(run_result.chunk_results.iter())
    {
        let (Ok(ref_extraction), Ok(cand_extraction)) = (&ref_chunk.result, &cand_chunk.result)
        else {
            continue;
        };
        chunks_scored += 1;
        ref_entities_all.extend(ref_extraction.entities.iter().cloned());
        cand_entities_all.extend(cand_extraction.entities.iter().cloned());
        ref_edges_all.extend(ref_extraction.edges.iter().cloned());
        cand_edges_all.extend(cand_extraction.edges.iter().cloned());

        let Some(judge) = judge_client else { continue };

        let ref_entities_val = serde_json::json!({"extracted_entities": ref_extraction.entities});
        let cand_entities_val = serde_json::json!({"extracted_entities": cand_extraction.entities});
        match judged_f1(
            judge,
            judge_cache,
            judge_model,
            "extract_nodes.extract_text",
            &ref_entities_val,
            &cand_entities_val,
        )
        .await
        {
            Ok((p, r, f1)) => {
                judged_entity_precisions.push(p);
                judged_entity_recalls.push(r);
                judged_entity_f1s.push(f1);
            }
            Err(e) => {
                eprintln!("lcg-eval: judge error (entities): {e}");
                judge_errors += 1;
            }
        }

        let ref_edges_val = serde_json::json!({"edges": ref_extraction.edges});
        let cand_edges_val = serde_json::json!({"edges": cand_extraction.edges});
        match judged_f1(
            judge,
            judge_cache,
            judge_model,
            "extract_edges.default",
            &ref_edges_val,
            &cand_edges_val,
        )
        .await
        {
            Ok((p, r, f1)) => {
                judged_edge_precisions.push(p);
                judged_edge_recalls.push(r);
                judged_edge_f1s.push(f1);
            }
            Err(e) => {
                eprintln!("lcg-eval: judge error (edges): {e}");
                judge_errors += 1;
            }
        }

        let ref_summaries: Vec<&str> = ref_extraction
            .entities
            .iter()
            .map(|e| e.summary.as_str())
            .collect();
        let cand_summaries: Vec<&str> = cand_extraction
            .entities
            .iter()
            .map(|e| e.summary.as_str())
            .collect();
        let ref_summaries_val = serde_json::json!({"summaries": ref_summaries});
        let cand_summaries_val = serde_json::json!({"summaries": cand_summaries});
        match judged_f1(
            judge,
            judge_cache,
            judge_model,
            "extract_nodes.extract_summaries_batch",
            &ref_summaries_val,
            &cand_summaries_val,
        )
        .await
        {
            Ok((_, _, f1)) => judged_summary_f1s.push(f1),
            Err(e) => {
                eprintln!("lcg-eval: judge error (summaries): {e}");
                judge_errors += 1;
            }
        }
    }

    let strict_entity = entity_strict_prf1(&ref_entities_all, &cand_entities_all);
    let strict_edge = edge_strict_prf1(&ref_edges_all, &cand_edges_all);
    let structured_output = StructuredOutputReliability {
        clean: run_result.structured_output.clean,
        recovered: run_result.structured_output.recovered,
        malformed: run_result.structured_output.malformed,
        malformed_rate: run_result.structured_output.malformed_rate(),
    };

    Ok(CandidateReport {
        backend_name: run_result.backend_name.clone(),
        chunks_run: run_result.chunk_results.len(),
        chunks_scored,
        errors,
        error_rate,
        latency: percentiles(latencies),
        structured_output,
        vocabulary_compliance: run_result.vocabulary_compliance.map(vocab_report),
        strict_entity_f1: strict_entity.f1,
        strict_edge_f1: strict_edge.f1,
        judged_entity_f1: average(&judged_entity_f1s),
        judged_entity_precision: average(&judged_entity_precisions),
        judged_entity_recall: average(&judged_entity_recalls),
        judged_edge_f1: average(&judged_edge_f1s),
        judged_edge_precision: average(&judged_edge_precisions),
        judged_edge_recall: average(&judged_edge_recalls),
        judged_summary_f1: average(&judged_summary_f1s),
        judge_errors,
    })
}

/// Maps the harness-internal FR-007 tally to the report's serializable shape.
fn vocab_report(v: VocabularyComplianceCounts) -> VocabularyComplianceReport {
    VocabularyComplianceReport {
        entities_checked: v.entities_checked,
        entities_out_of_vocab: v.entities_out_of_vocab,
        entity_violation_rate: v.entity_violation_rate(),
        edges_checked: v.edges_checked,
        edges_out_of_vocab: v.edges_out_of_vocab,
        edge_violation_rate: v.edge_violation_rate(),
    }
}

/// Cache-first judge lookup (SC-003: a cache hit makes zero new judge calls).
pub async fn judged_f1(
    judge: &dyn JudgeClient,
    judge_cache: &JudgeCache,
    judge_model: &str,
    prompt_name: &str,
    reference: &serde_json::Value,
    candidate: &serde_json::Value,
) -> Result<(f64, f64, f64), String> {
    let key = cache_key(prompt_name, judge_model, reference, candidate);
    let verdict: JudgeVerdict = match judge_cache.get(&key) {
        Some(v) => v,
        None => {
            let v = judge
                .judge(prompt_name, reference, candidate)
                .await
                .map_err(|e| format!("judge call failed: {e}"))?;
            // Labelled distinctly from the call failure above because the two are different
            // failure classes and only one is transient. Now that judge errors are non-fatal,
            // a systemic cache problem (bad --judge-cache path, permissions, disk full) would
            // otherwise recur silently on every one of ~1340 calls, completing the run with a
            // huge judge_errors count while persisting nothing — burning exactly the spend
            // the non-fatal handling exists to protect.
            judge_cache
                .insert(key, v.clone())
                .map_err(|e| format!("verdict received but caching it failed: {e}"))?;
            v
        }
    };
    Ok(precision_recall_f1(&verdict))
}

pub fn average(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use lcg_core::{ExtractedEdge, ExtractedEntity, ExtractionResult};
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;
    use crate::judge::PairwiseVerdict;
    use crate::runner::ChunkResult;

    /// Scripted `JudgeClient` test double: fails per-call based on a caller-supplied
    /// predicate over `(prompt_name, zero-based call index for that prompt_name)`, so one
    /// double can express both "fails once, for one axis, on one chunk" and "fails on every
    /// call for a whole axis" (FR-003) without a family of ad-hoc structs. Tracks its own
    /// per-prompt-name call counter, matching this crate's convention of keeping test
    /// doubles local to the module that needs them (`StaticJudge` in `judge.rs`,
    /// `LexicographicJudge`/`CountingPairwiseJudge` in `pairwise.rs`).
    struct ScriptedJudge {
        fails: fn(&str, usize) -> bool,
        calls: Mutex<HashMap<String, usize>>,
    }

    impl ScriptedJudge {
        fn new(fails: fn(&str, usize) -> bool) -> Self {
            Self {
                fails,
                calls: Mutex::new(HashMap::new()),
            }
        }

        fn never_fails() -> Self {
            Self::new(|_, _| false)
        }
    }

    impl JudgeClient for ScriptedJudge {
        fn judge<'a>(
            &'a self,
            prompt_name: &'a str,
            _reference: &'a Value,
            _candidate: &'a Value,
        ) -> BoxFuture<'a, Result<JudgeVerdict, String>> {
            let call_index = {
                let mut calls = self.calls.lock().unwrap();
                let entry = calls.entry(prompt_name.to_string()).or_insert(0);
                let idx = *entry;
                *entry += 1;
                idx
            };
            let fail = (self.fails)(prompt_name, call_index);
            Box::pin(async move {
                if fail {
                    Err(format!(
                        "scripted failure on {prompt_name} call {call_index}"
                    ))
                } else {
                    // A single matched pair with nothing unmatched — precision/recall/F1 all
                    // 1.0, distinct from the both-empty default so a passing call is visibly
                    // "real" data in test assertions.
                    Ok(JudgeVerdict {
                        matched: vec![(0, 0)],
                        unmatched_reference: vec![],
                        unmatched_candidate: vec![],
                    })
                }
            })
        }

        fn judge_pairwise<'a>(
            &'a self,
            _prompt_name: &'a str,
            _chunk_text: &'a str,
            _slot_a: &'a Value,
            _slot_b: &'a Value,
        ) -> BoxFuture<'a, Result<PairwiseVerdict, String>> {
            unimplemented!("scoring::score_candidate never calls judge_pairwise")
        }
    }

    fn extraction_result(entity_name: &str, edge_fact: &str) -> ExtractionResult {
        ExtractionResult {
            entities: vec![ExtractedEntity {
                name: entity_name.to_string(),
                entity_type: "Thing".to_string(),
                summary: format!("{entity_name} summary"),
            }],
            edges: vec![ExtractedEdge {
                source_name: entity_name.to_string(),
                target_name: "other".to_string(),
                fact: edge_fact.to_string(),
                relation_type: None,
                valid_at: None,
                invalid_at: None,
            }],
        }
    }

    /// Builds a `BackendRunResult` whose chunks each carry distinct entity/edge content, so
    /// each chunk hashes to a distinct judge-cache key — a cache hit on one chunk must never
    /// silently mask a scripted failure meant for a different chunk.
    fn backend_run_result(name: &str, chunk_count: usize) -> BackendRunResult {
        let chunk_results = (0..chunk_count)
            .map(|i| ChunkResult {
                chunk_title: format!("chunk-{i}"),
                latency_ms: 10,
                result: Ok(extraction_result(
                    &format!("entity-{name}-{i}"),
                    &format!("fact-{name}-{i}"),
                )),
            })
            .collect();
        BackendRunResult {
            backend_name: name.to_string(),
            chunk_results,
            structured_output: Default::default(),
            vocabulary_compliance: None,
        }
    }

    fn fresh_cache() -> (TempDir, JudgeCache) {
        let dir = TempDir::new().unwrap();
        let cache = JudgeCache::load(dir.path().join("cache.jsonl")).unwrap();
        (dir, cache)
    }

    #[tokio::test]
    async fn entity_axis_failure_on_one_chunk_does_not_abort_and_other_axes_still_score() {
        // FR-003 scenario 1: fail only the very first `extract_nodes.extract_text` call
        // (chunk 0's entity axis); every other call (edge/summary axes on every chunk,
        // entity axis on later chunks) succeeds.
        let judge = ScriptedJudge::new(|prompt_name, call_index| {
            prompt_name == "extract_nodes.extract_text" && call_index == 0
        });
        let (_dir, cache) = fresh_cache();
        let reference = backend_run_result("reference", 2);
        let candidate = backend_run_result("candidate", 2);

        let report = score_candidate(&candidate, &reference, Some(&judge), &cache, "test-model")
            .await
            .expect("a single non-fatal judge error must not abort score_candidate");

        assert_eq!(report.judge_errors, 1);
        assert_eq!(report.chunks_scored, 2);
        // The failed chunk's entity data point is missing, but the other chunk's isn't —
        // the average is over one successful call, not zero.
        assert_eq!(report.judged_entity_f1, Some(1.0));
        // Edge/summary axes never failed on the entity-axis chunk — both scored calls land.
        assert_eq!(report.judged_edge_f1, Some(1.0));
        assert_eq!(report.judged_summary_f1, Some(1.0));
    }

    #[tokio::test]
    async fn axis_that_fails_every_call_reports_none_not_zero() {
        // FR-003 scenario 2: the edge axis fails unconditionally; entity/summary axes never
        // fail.
        let judge =
            ScriptedJudge::new(|prompt_name, _call_index| prompt_name == "extract_edges.default");
        let (_dir, cache) = fresh_cache();
        let reference = backend_run_result("reference", 3);
        let candidate = backend_run_result("candidate", 3);

        let report = score_candidate(&candidate, &reference, Some(&judge), &cache, "test-model")
            .await
            .unwrap();

        assert_eq!(
            report.judged_edge_f1, None,
            "an all-failures axis must report None, not 0.0"
        );
        assert_eq!(report.judged_edge_precision, None);
        assert_eq!(report.judged_edge_recall, None);
        assert_eq!(report.judge_errors, 3, "one failed edge call per chunk");
        assert_eq!(report.judged_entity_f1, Some(1.0));
        assert_eq!(report.judged_summary_f1, Some(1.0));
    }

    #[tokio::test]
    async fn precision_recall_f1_average_only_successful_calls() {
        // FR-003 scenario 3: entity axis fails on chunks 1 and 3 (0-based call indices 1 and
        // 3) out of 4, so the reported average must be computed over the 2 successful calls
        // only, not over 4 (which would silently treat failures as zero).
        let judge = ScriptedJudge::new(|prompt_name, call_index| {
            prompt_name == "extract_nodes.extract_text" && (call_index == 1 || call_index == 3)
        });
        let (_dir, cache) = fresh_cache();
        let reference = backend_run_result("reference", 4);
        let candidate = backend_run_result("candidate", 4);

        let report = score_candidate(&candidate, &reference, Some(&judge), &cache, "test-model")
            .await
            .unwrap();

        // Every successful ScriptedJudge call returns a perfect verdict (p=r=f1=1.0), so the
        // average over 2 successes is still 1.0 — the assertion that matters is judge_errors
        // and chunks_scored below, which prove the denominator excludes the failures.
        assert_eq!(report.judged_entity_f1, Some(1.0));
        assert_eq!(report.judge_errors, 2);
        assert_eq!(
            report.chunks_scored, 4,
            "chunks_scored counts extraction successes, independent of judge outcome"
        );
    }

    #[tokio::test]
    async fn chunk_with_failed_reference_extraction_is_excluded_from_scoring() {
        // Edge case: a chunk where the reference side's extraction errored must never reach
        // judging at all. A judge that fails unconditionally proves no call was attempted
        // for that chunk (chunks_scored excludes it and judge_errors stays 0).
        let judge = ScriptedJudge::new(|_, _| true);
        let (_dir, cache) = fresh_cache();
        let mut reference = backend_run_result("reference", 1);
        reference.chunk_results[0].result = Err("extraction failed".to_string());
        let candidate = backend_run_result("candidate", 1);

        let report = score_candidate(&candidate, &reference, Some(&judge), &cache, "test-model")
            .await
            .unwrap();

        assert_eq!(report.chunks_scored, 0);
        assert_eq!(report.judge_errors, 0);
        assert_eq!(report.judged_entity_f1, None);
    }

    #[tokio::test]
    async fn empty_candidate_list_reports_zeroed_denominators_not_a_panic() {
        // Edge case: zero chunks on both sides.
        let judge = ScriptedJudge::never_fails();
        let (_dir, cache) = fresh_cache();
        let reference = backend_run_result("reference", 0);
        let candidate = backend_run_result("candidate", 0);

        let report = score_candidate(&candidate, &reference, Some(&judge), &cache, "test-model")
            .await
            .unwrap();

        assert_eq!(report.chunks_scored, 0);
        assert_eq!(report.judge_errors, 0);
        assert_eq!(report.error_rate, 0.0);
        assert_eq!(report.judged_entity_f1, None);
        assert_eq!(report.judged_entity_precision, None);
        assert_eq!(report.judged_entity_recall, None);
        assert_eq!(report.judged_edge_f1, None);
        assert_eq!(report.judged_edge_precision, None);
        assert_eq!(report.judged_edge_recall, None);
        assert_eq!(report.judged_summary_f1, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn judge_call_failure_and_cache_write_failure_are_distinguishable() {
        // FR-004: `judged_f1` surfaces two different failure classes with different message
        // text. A judge-call failure comes from the scripted double returning `Err`
        // directly; a cache-write failure comes from a real filesystem condition (a
        // read-only cache directory) per Plan's decision to avoid introducing a new mocking
        // trait for `JudgeCache`.
        let failing_judge = ScriptedJudge::new(|_, _| true);
        let (_dir, healthy_cache) = fresh_cache();
        let call_err = judged_f1(
            &failing_judge,
            &healthy_cache,
            "test-model",
            "extract_nodes.extract_text",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(
            call_err.contains("judge call failed"),
            "expected a judge-call failure message, got: {call_err}"
        );

        let readonly_dir = TempDir::new().unwrap();
        let cache_path = readonly_dir.path().join("sub").join("cache.jsonl");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        let readonly_cache = JudgeCache::load(&cache_path).unwrap();
        set_readonly(cache_path.parent().unwrap());
        let never_fails = ScriptedJudge::never_fails();
        let cache_err = judged_f1(
            &never_fails,
            &readonly_cache,
            "test-model",
            "extract_nodes.extract_text",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        set_writable(cache_path.parent().unwrap());
        assert!(
            cache_err.contains("caching it failed"),
            "expected a cache-write failure message, got: {cache_err}"
        );
        assert_ne!(
            call_err, cache_err,
            "the two failure classes must be distinguishable in the surfaced message"
        );
    }

    #[cfg(unix)]
    fn set_readonly(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir, perms).unwrap();
    }

    #[cfg(unix)]
    fn set_writable(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir, perms).unwrap();
    }
}
