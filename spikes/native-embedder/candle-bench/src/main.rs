mod embedder;

use anyhow::Result;
use clap::Parser;
use common::{
    corpus::{bench_sentences, PARITY_SENTENCES},
    stats::{cosine_similarity, BenchStats, ParityResult},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "candle-bench",
    about = "BGE-base-en-v1.5 embedding spike — candle backend"
)]
struct Args {
    /// Directory containing model.safetensors, config.json, tokenizer.json
    #[arg(long)]
    model_dir: PathBuf,

    /// Number of warmup iterations before timing
    #[arg(long, default_value_t = 10)]
    warmup: usize,

    /// Number of timed single-input iterations
    #[arg(long, default_value_t = 200)]
    iters: usize,

    /// Path to reference_embeddings.json for cosine parity check
    #[arg(long)]
    parity_json: Option<PathBuf>,

    /// Write structured JSON results here (stdout if omitted)
    #[arg(long)]
    output_json: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct ReferenceEmbeddings {
    sentences: Vec<String>,
    embeddings: Vec<Vec<f32>>,
}

#[derive(Serialize)]
struct Output {
    library: &'static str,
    platform: String,
    model_dir: String,
    cold_start_ms: f64,
    warmup_iters: usize,
    bench: BenchStats,
    parity: Option<ParityResult>,
}

fn main() -> Result<()> {
    let process_start = Instant::now();
    let args = Args::parse();

    eprintln!("Loading model from {} ...", args.model_dir.display());
    let embedder = embedder::BgeEmbedder::load(&args.model_dir)?;

    // First embed call marks end of cold start
    let _ = embedder.embed("warmup")?;
    let cold_start_ms = process_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("Cold start: {:.0} ms", cold_start_ms);

    let bench_sents = bench_sentences();

    // Warmup
    eprintln!("Warmup ({} iters) ...", args.warmup);
    for i in 0..args.warmup {
        let _ = embedder.embed(bench_sents[i % bench_sents.len()])?;
    }

    // Timed single-input iterations
    eprintln!("Benchmarking ({} iters) ...", args.iters);
    let mut latencies_ms = Vec::with_capacity(args.iters);
    for i in 0..args.iters {
        let t0 = Instant::now();
        let _ = embedder.embed(bench_sents[i % bench_sents.len()])?;
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // Batch throughput: embed all 200 bench sentences sequentially, 3 trials
    eprintln!(
        "Batch throughput (3 trials × {} sentences) ...",
        bench_sents.len()
    );
    let mut batch_total_ms = f64::MAX;
    for _ in 0..3 {
        let t0 = Instant::now();
        for s in &bench_sents {
            let _ = embedder.embed(s)?;
        }
        batch_total_ms = batch_total_ms.min(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let bench_stats = BenchStats::compute(&latencies_ms, batch_total_ms, bench_sents.len());

    // Parity check
    let parity = if let Some(parity_path) = &args.parity_json {
        eprintln!("Parity check vs {} ...", parity_path.display());
        let f = std::fs::File::open(parity_path)?;
        let refs: ReferenceEmbeddings = serde_json::from_reader(f)?;

        if refs.embeddings.len() != PARITY_SENTENCES.len() {
            anyhow::bail!(
                "reference JSON has {} embeddings, expected {}",
                refs.embeddings.len(),
                PARITY_SENTENCES.len()
            );
        }
        let mut sims = Vec::with_capacity(PARITY_SENTENCES.len());
        for (i, sentence) in PARITY_SENTENCES.iter().enumerate() {
            let emb = embedder.embed(sentence)?;
            let sim = cosine_similarity(&emb, &refs.embeddings[i]);
            sims.push(sim);
        }
        Some(ParityResult::compute(&sims, 0.999))
    } else {
        None
    };

    let output = Output {
        library: "candle",
        platform: platform_string(),
        model_dir: args.model_dir.display().to_string(),
        cold_start_ms,
        warmup_iters: args.warmup,
        bench: bench_stats,
        parity,
    };

    let json = serde_json::to_string_pretty(&output)?;
    if let Some(out_path) = &args.output_json {
        std::fs::write(out_path, &json)?;
        eprintln!("Results written to {}", out_path.display());
    } else {
        println!("{}", json);
    }

    eprintln!(
        "\np50={:.1}ms p95={:.1}ms batch={:.0}sent/s",
        output.bench.p50_ms, output.bench.p95_ms, output.bench.batch_throughput_per_sec
    );

    Ok(())
}

fn platform_string() -> String {
    format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
}
