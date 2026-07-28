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

