//! `lcg-eval` — extraction-quality eval harness CLI entry point. See `README.md`'s
//! "Extraction-quality eval harness" section for usage and cost implications, and
//! `lcg_eval::cli::usage()` for the flag reference.

use std::sync::Arc;

use lcg_core::ontology::load_ontology_from_path;
use lcg_core::{CassetteWriter, Extractor, Ontology, RecordingExtractor, TelemetrySink};

use lcg_eval::backend::{build_extractor, parse_backend_spec, provider_label, resolved_model};
use lcg_eval::cli::{parse_args, usage, Args, CliMode, JudgeMode};
use lcg_eval::corpus::{default_corpus_path, load_corpus, select_subset};
use lcg_eval::judge::{precision_recall_f1, AnthropicJudgeClient, JudgeClient, JudgeVerdict};
use lcg_eval::judge_cache::{cache_key, JudgeCache};
use lcg_eval::metrics::{edge_strict_prf1, entity_strict_prf1};
use lcg_eval::pairwise::{
    score_all_pairs, CALIBRATION_BAND_HIGH, CALIBRATION_BAND_LOW,
    ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD,
};
use lcg_eval::report::{
    percentiles, CandidateReport, PairwiseReportEntry, Report, StructuredOutputReliability,
    VocabularyComplianceReport,
};
use lcg_eval::runner::{run_backend, BackendRunResult, CountingSink, VocabularyComplianceCounts};

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
        CliMode::Run(a) => *a,
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

    // FR-001: load the ontology once (if given) and thread it through every backend's
    // extraction calls. FR-002: a missing/malformed/empty ontology file is a loud usage
    // error here, never a silent fallback to freeform.
    let ontology: Option<Ontology> = match &cli.ontology {
        Some(path) => Some(
            load_ontology_from_path(std::path::Path::new(path), cli.ontology_mode.clone())
                .map_err(|e| format!("--ontology {path}: {e}"))?,
        ),
        None => None,
    };
    // FR-003: record which regime produced this report.
    let ontology_mode_label = match &ontology {
        Some(o) => o.mode.to_string(),
        None => "freeform".to_string(),
    };

    let mut run_results: Vec<BackendRunResult> = Vec::new();
    for b in &cli.backends {
        let kind = parse_backend_spec(&b.spec)?;
        let sink = Arc::new(CountingSink::new());
        let mut extractor = build_extractor(&kind, Arc::clone(&sink) as Arc<dyn TelemetrySink>)?;
        if let Some(cassette) = cli.record_cassette.iter().find(|c| c.backend == b.name) {
            let writer = Arc::new(CassetteWriter::open(&cassette.path).map_err(|e| e.to_string())?);
            extractor = Arc::new(RecordingExtractor::new(
                extractor,
                provider_label(&kind),
                resolved_model(&kind),
                writer,
            ));
        }

        eprintln!(
            "lcg-eval: running backend '{}' ({}) over {} chunks",
            b.name,
            b.spec,
            chunks.len()
        );
        run_results.push(
            run_backend(
                &b.name,
                extractor as Arc<dyn Extractor>,
                sink,
                &chunks,
                ontology.as_ref(),
            )
            .await,
        );
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

    // Reference-mode judge calls are skipped entirely (not run-and-ignored) when the user
    // asked for pairwise only — `--judge-mode both` is how to get both passes' judge spend.
    let reference_judge_client: Option<&dyn JudgeClient> = if cli.judge_mode == JudgeMode::Pairwise
    {
        None
    } else {
        judge_client.as_deref()
    };

    let mut candidates = Vec::new();
    for run_result in &run_results {
        candidates.push(
            score_candidate(
                run_result,
                &reference_result,
                reference_judge_client,
                &judge_cache,
                &cli.judge_model,
            )
            .await?,
        );
    }

    // FR-008/FR-009: pairwise mode covers every configured backend pair, reusing the same
    // `run_results` already produced above — zero additional extraction calls. `None` (not
    // an empty `Some(vec![])`) both when `--judge-mode` never asked for it (SC-004: the key
    // is then absent from serialization) and when it did ask but no judge client is
    // available (nothing was actually scored).
    let pairwise = if cli.judge_mode == JudgeMode::Reference {
        None
    } else {
        match judge_client.as_deref() {
            Some(judge) => {
                let axis_results =
                    score_all_pairs(&chunks, &run_results, judge, &judge_cache, &cli.judge_model)
                        .await?;
                let entries: Vec<PairwiseReportEntry> =
                    axis_results.into_iter().map(to_report_entry).collect();
                warn_on_calibration_and_inconsistency(&entries);
                Some(entries)
            }
            None => {
                eprintln!(
                    "lcg-eval: ANTHROPIC_API_KEY not set — skipping pairwise judging \
                     (--judge-mode {})",
                    judge_mode_label(cli.judge_mode)
                );
                None
            }
        }
    };

    let report = Report {
        corpus_size: chunks.len(),
        reference_backend: reference_name,
        ontology_mode: ontology_mode_label,
        candidates,
        pairwise,
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

fn average(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn judge_mode_label(mode: JudgeMode) -> &'static str {
    match mode {
        JudgeMode::Reference => "reference",
        JudgeMode::Pairwise => "pairwise",
        JudgeMode::Both => "both",
    }
}

/// Translates one `pairwise::score_all_pairs` result into the report's serializable shape
/// (FR-007). `wins`/`losses` are from `backend_a`'s perspective.
fn to_report_entry(r: lcg_eval::pairwise::PairwiseAxisResult) -> PairwiseReportEntry {
    PairwiseReportEntry {
        backend_a: r.backend_a,
        backend_b: r.backend_b,
        axis: r.axis.label().to_string(),
        wins: r.tally.wins_a,
        losses: r.tally.wins_b,
        ties: r.tally.ties,
        win_rate: r.tally.win_rate_a(),
        order_inconsistency_rate: r.tally.order_inconsistency_rate(),
        chunks_compared: r.tally.chunks_compared,
        chunks_skipped: r.tally.chunks_skipped,
    }
}

/// SC-001/SC-002: loud stderr warnings — never block the run, the report artifact itself
/// stays pure data — when a pair/axis's win rate falls outside the calibration band or its
/// order-inconsistency rate exceeds the untrusted threshold.
///
/// The calibration-band check (SC-001) fires for every pair, not just the designated
/// calibration control: which pair *is* the calibration control (reference vs. its own
/// independent sample) is operator knowledge, not something derivable from `Args` in
/// general — the most common pattern (two independently-recorded `cassette:path=` files of
/// the same model, e.g. the #248 runbook's `baseline`/`candidate`) has no shared spec string
/// to detect it by, so a same-spec-only heuristic would silently fail to warn on exactly the
/// pair SC-001 cares about. The wording below therefore doesn't assert bias outright for a
/// non-calibration pair — a win rate outside 45-55% between two genuinely different backends
/// is the expected, desired signal (the whole point of pairwise judging), not a defect.
fn warn_on_calibration_and_inconsistency(entries: &[PairwiseReportEntry]) {
    for e in entries {
        if !(CALIBRATION_BAND_LOW..=CALIBRATION_BAND_HIGH).contains(&e.win_rate) {
            eprintln!(
                "lcg-eval: NOTE — {} vs {} [{}]: win rate {:.1}% falls outside the {:.0}-{:.0}% \
                 calibration band (SC-001). If this is your reference-vs-its-own-independent-\
                 sample calibration pair, this deviation likely indicates judge position bias — \
                 investigate before trusting any pairwise result from this run. If this is a \
                 genuine candidate-vs-candidate comparison, a win rate outside the band is the \
                 expected, desired signal, not evidence of bias.",
                e.backend_a,
                e.backend_b,
                e.axis,
                e.win_rate * 100.0,
                CALIBRATION_BAND_LOW * 100.0,
                CALIBRATION_BAND_HIGH * 100.0,
            );
        }
        if e.order_inconsistency_rate > ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD {
            eprintln!(
                "lcg-eval: WARNING {} vs {} [{}]: order-inconsistency rate {:.1}% exceeds the \
                 {:.0}% untrusted threshold (SC-002) — this pairwise result is not to be \
                 trusted",
                e.backend_a,
                e.backend_b,
                e.axis,
                e.order_inconsistency_rate * 100.0,
                ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD * 100.0,
            );
        }
    }
}
