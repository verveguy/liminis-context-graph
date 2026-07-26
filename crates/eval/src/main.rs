//! `lcg-eval` — extraction-quality eval harness CLI entry point. See `README.md`'s
//! "Extraction-quality eval harness" section for usage and cost implications, and
//! `lcg_eval::cli::usage()` for the flag reference.

use std::sync::Arc;

use lcg_core::{CassetteWriter, Extractor, RecordingExtractor, TelemetrySink};

use lcg_eval::backend::{build_extractor, parse_backend_spec};
use lcg_eval::cli::{parse_args, usage, Args, CliMode};
use lcg_eval::corpus::{default_corpus_path, load_corpus, select_subset};
use lcg_eval::judge::{precision_recall_f1, AnthropicJudgeClient, JudgeClient, JudgeVerdict};
use lcg_eval::judge_cache::{cache_key, JudgeCache};
use lcg_eval::metrics::{edge_strict_prf1, entity_strict_prf1};
use lcg_eval::report::{percentiles, CandidateReport, Report, StructuredOutputReliability};
use lcg_eval::runner::{run_backend, BackendRunResult, CountingSink};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("lcg-eval: {e}");
            std::process::exit(2);
        }
    };
    let run_args = match cli {
        CliMode::Help => {
            println!("{}", usage());
            return;
        }
        CliMode::Run(a) => a,
    };

    if let Err(e) = run(run_args).await {
        eprintln!("lcg-eval: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Args) -> Result<(), String> {
    let corpus_path = cli
        .corpus
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_corpus_path);
    let all_chunks = load_corpus(&corpus_path)?;
    let limit = if cli.all {
        None
    } else {
        Some(cli.limit.unwrap_or(lcg_eval::cli::DEFAULT_LIMIT))
    };
    let chunks = select_subset(all_chunks, limit);
    eprintln!(
        "lcg-eval: loaded {} corpus chunks from {}",
        chunks.len(),
        corpus_path.display()
    );

    // cli::parse_args always resolves --reference to a configured backend name.
    let reference_name = cli
        .reference
        .clone()
        .expect("parse_args always sets reference");

    let mut run_results: Vec<BackendRunResult> = Vec::new();
    for b in &cli.backends {
        let kind = parse_backend_spec(&b.spec)?;
        let sink = Arc::new(CountingSink::new());
        let mut extractor = build_extractor(&kind, Arc::clone(&sink) as Arc<dyn TelemetrySink>)?;
        if let Some(cassette) = cli.record_cassette.iter().find(|c| c.backend == b.name) {
            let writer = Arc::new(CassetteWriter::open(&cassette.path).map_err(|e| e.to_string())?);
            extractor = Arc::new(RecordingExtractor::new(
                extractor,
                b.name.clone(),
                b.name.clone(),
                writer,
            ));
        }

        eprintln!(
            "lcg-eval: running backend '{}' ({}) over {} chunks",
            b.name,
            b.spec,
            chunks.len()
        );
        run_results
            .push(run_backend(&b.name, extractor as Arc<dyn Extractor>, sink, &chunks).await);
    }

    let reference_result = run_results
        .iter()
        .find(|r| r.backend_name == reference_name)
        .cloned()
        .ok_or_else(|| format!("--reference '{reference_name}' matched no run result"))?;

    let judge_client: Option<Arc<dyn JudgeClient>> =
        std::env::var("ANTHROPIC_API_KEY").ok().map(|key| {
            Arc::new(AnthropicJudgeClient::new(key, cli.judge_model.clone()))
                as Arc<dyn JudgeClient>
        });
    if judge_client.is_none() {
        eprintln!(
            "lcg-eval: ANTHROPIC_API_KEY not set — skipping LLM-as-judge scoring, reporting \
             strict-string F1 only"
        );
    }
    let judge_cache = JudgeCache::load(&cli.judge_cache)?;

    let mut candidates = Vec::new();
    for run_result in &run_results {
        candidates.push(
            score_candidate(
                run_result,
                &reference_result,
                judge_client.as_deref(),
                &judge_cache,
                &cli.judge_model,
            )
            .await?,
        );
    }

    let report = Report {
        corpus_size: chunks.len(),
        reference_backend: reference_name,
        candidates,
    };

    println!("{}", report.render_human_readable());
    if let Some(path) = &cli.output {
        std::fs::write(path, report.to_json())
            .map_err(|e| format!("failed to write --output {path}: {e}"))?;
        eprintln!("lcg-eval: wrote JSON report to {path}");
    }

    Ok(())
}

async fn score_candidate(
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
    let mut judged_edge_f1s = Vec::new();
    let mut judged_summary_f1s = Vec::new();
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
        let (_, _, f1) = judged_f1(
            judge,
            judge_cache,
            judge_model,
            "extract_nodes.extract_text",
            &ref_entities_val,
            &cand_entities_val,
        )
        .await?;
        judged_entity_f1s.push(f1);

        let ref_edges_val = serde_json::json!({"edges": ref_extraction.edges});
        let cand_edges_val = serde_json::json!({"edges": cand_extraction.edges});
        let (_, _, f1) = judged_f1(
            judge,
            judge_cache,
            judge_model,
            "extract_edges.default",
            &ref_edges_val,
            &cand_edges_val,
        )
        .await?;
        judged_edge_f1s.push(f1);

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
        let (_, _, f1) = judged_f1(
            judge,
            judge_cache,
            judge_model,
            "extract_nodes.extract_summaries_batch",
            &ref_summaries_val,
            &cand_summaries_val,
        )
        .await?;
        judged_summary_f1s.push(f1);
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
        strict_entity_f1: strict_entity.f1,
        strict_edge_f1: strict_edge.f1,
        judged_entity_f1: average(&judged_entity_f1s),
        judged_edge_f1: average(&judged_edge_f1s),
        judged_summary_f1: average(&judged_summary_f1s),
    })
}

/// Cache-first judge lookup (SC-003: a cache hit makes zero new judge calls).
async fn judged_f1(
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
            let v = judge.judge(prompt_name, reference, candidate).await?;
            judge_cache.insert(key, v.clone())?;
            v
        }
    };
    Ok(precision_recall_f1(&verdict))
}

fn average(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}
