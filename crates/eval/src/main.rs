//! `lcg-eval` — extraction-quality eval harness CLI entry point. See `README.md`'s
//! "Extraction-quality eval harness" section for usage and cost implications, and
//! `lcg_eval::cli::usage()` for the flag reference.

use std::sync::Arc;

use lcg_core::ontology::load_ontology_from_path;
use lcg_core::{
    CassetteWriter, ExtractionFailureSink, ExtractionFailureWriter, Extractor, Ontology,
    RecordingExtractor, TeeSink, TelemetrySink, DEFAULT_MAX_BYTES_PER_FILE,
};

use lcg_eval::backend::{
    build_extractor, parse_backend_spec, provider_label, resolved_model, BackendKind,
};
use lcg_eval::cli::{parse_args, usage, Args, CliMode, JudgeMode};
use lcg_eval::corpus::{default_corpus_path, load_corpus, select_subset};
use lcg_eval::judge::{AnthropicJudgeClient, JudgeClient};
use lcg_eval::judge_cache::JudgeCache;
use lcg_eval::pairwise::{
    score_all_pairs, CALIBRATION_BAND_HIGH, CALIBRATION_BAND_LOW,
    ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD,
};
use lcg_eval::plan::{hash_file, render as render_plan, resolve as resolve_plan};
use lcg_eval::report::{
    validate_recorded_cassette, validate_recorded_cassettes_distinct, PairwiseReportEntry, Report,
};
use lcg_eval::runner::{run_backend, BackendRunResult, CountingSink};
use lcg_eval::scoring::score_candidate;

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

    // FR-001/User Story 2: `--dry-run` and the real run both resolve through `plan::resolve`
    // so a preview can never drift from what a real run would actually do (ADR-0052).
    let resolved = resolve_plan(&cli, chunks.len());
    if cli.dry_run {
        println!("{}", render_plan(&resolved));
        return Ok(());
    }
    if !resolved.guard_violations.is_empty() {
        let violations = resolved
            .guard_violations
            .iter()
            .map(|v| format!("  - {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "refusing to run — {} guard violation(s) would invalidate the measurement:\n{}",
            resolved.guard_violations.len(),
            violations
        ));
    }
    // FR-005: a coverage shortfall shrinks the sample but never aborts (see plan.rs's module
    // doc) — noted here so it's visible even outside --dry-run.
    for b in &resolved.backends {
        if let Some(shortfall) = b.coverage_shortfall {
            eprintln!(
                "lcg-eval: NOTE backend '{}' cassette covers {shortfall} fewer chunk(s) than \
                 the requested scope ({})",
                b.name, resolved.scope_label
            );
        }
    }

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
        let cassette = cli.record_cassette.iter().find(|c| c.backend == b.name);

        // #306 FR-001: the failure-record sidecar is installed alongside `CassetteWriter`
        // (this backend is being recorded), fed from the *same* telemetry sink passed into
        // the leaf extractor — `TelemetryEvent::ExtractionFailure` is emitted from inside
        // `AnthropicExtractor`/`OaiExtractor` itself, a layer `RecordingExtractor` (which only
        // ever sees the whole `extract()` call's success value) cannot observe.
        let telemetry_sink: Arc<dyn TelemetrySink> = match cassette {
            Some(cassette) => {
                let failure_writer =
                    ExtractionFailureWriter::open(&cassette.path, DEFAULT_MAX_BYTES_PER_FILE)
                        .map_err(|e| e.to_string())?;
                Arc::new(TeeSink::new(vec![
                    Arc::clone(&sink) as Arc<dyn TelemetrySink>,
                    Arc::new(ExtractionFailureSink::new(failure_writer)) as Arc<dyn TelemetrySink>,
                ]))
            }
            None => Arc::clone(&sink) as Arc<dyn TelemetrySink>,
        };
        let mut extractor = build_extractor(&kind, telemetry_sink)?;
        if let Some(cassette) = cassette {
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

    // FR-006: verify each recorded cassette's record count matches this report's own
    // accounting before printing or writing anything — a truncated capture must never
    // produce a plausible-looking report artifact. Counted via `count_records`, not
    // `load_records`: the latter hard-rejects duplicate keys, and `classify_entities`/
    // `classify_relations` cassette keys hash only extracted content, not chunk identity —
    // two distinct chunks that legitimately extract the same (often empty) content collide,
    // and that must not discard an already-paid-for, otherwise-valid report (handarbeit-
    // pruefer review finding on PR #280). `load_records` still enforces uniqueness where it
    // actually matters: at replay time.
    let mut recorded_hashes = Vec::with_capacity(cli.record_cassette.len());
    for cassette in &cli.record_cassette {
        let candidate = report
            .candidates
            .iter()
            .find(|c| c.backend_name == cassette.backend)
            .ok_or_else(|| {
                format!(
                    "--record-cassette backend '{}' matched no candidate in the report",
                    cassette.backend
                )
            })?;
        let actual = lcg_core::cassette::count_records(&cassette.path)
            .map_err(|e| format!("--record-cassette {}: {e}", cassette.path))?;
        validate_recorded_cassette(candidate, actual)?;
        if let Err(e) = lcg_core::cassette::load_records(&cassette.path) {
            eprintln!(
                "lcg-eval: NOTE backend '{}' cassette {} will not replay safely as recorded \
                 ({e}) — this run's report is unaffected, but fix the cassette before using \
                 it as a cassette: backend",
                cassette.backend, cassette.path
            );
        }
        let hash = hash_file(&cassette.path).map_err(|e| {
            format!(
                "--record-cassette {}: cannot hash cassette for the identity check: {e}",
                cassette.path
            )
        })?;
        recorded_hashes.push((cassette.backend.clone(), hash));
    }
    // FR-004 (post-run, full coverage): hash every cassette this run touched — both
    // pre-existing `cassette:` replay backends and freshly `--record-cassette`d live ones —
    // and check pairwise distinctness across the whole set. `plan::resolve`'s pre-flight
    // guard already rejects two pre-existing replay cassettes sharing content before any
    // outbound call, and a pair recorded during THIS run doesn't exist to hash until it
    // completes; neither of those alone catches a live backend's fresh output coming out
    // byte-identical to a replay backend already in play in the same run — exactly
    // `04-full-run.sh`'s baseline-replayed/candidate-recorded shape (handarbeit-pruefer
    // review finding on PR #280).
    let mut all_cassette_hashes = recorded_hashes;
    for b in &cli.backends {
        if let Ok(BackendKind::Cassette { path }) = parse_backend_spec(&b.spec) {
            let hash = hash_file(&path).map_err(|e| {
                format!(
                    "backend '{}': cannot hash cassette '{path}' for the identity check: {e}",
                    b.name
                )
            })?;
            all_cassette_hashes.push((b.name.clone(), hash));
        }
    }
    validate_recorded_cassettes_distinct(&all_cassette_hashes)?;

    println!("{}", report.render_human_readable());
    if let Some(path) = &cli.output {
        std::fs::write(path, report.to_json())
            .map_err(|e| format!("failed to write --output {path}: {e}"))?;
        eprintln!("lcg-eval: wrote JSON report to {path}");
    }

    Ok(())
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
        judge_errors: r.tally.judge_errors,
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
