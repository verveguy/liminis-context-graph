use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub batch_throughput_per_sec: f64,
    pub n_iters: usize,
}

impl BenchStats {
    pub fn compute(latencies_ms: &[f64], batch_total_ms: f64, batch_size: usize) -> Self {
        assert!(!latencies_ms.is_empty(), "latencies_ms must not be empty");
        let mut sorted = latencies_ms.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = sorted.len();
        BenchStats {
            p50_ms: percentile(&sorted, 50.0),
            p95_ms: percentile(&sorted, 95.0),
            p99_ms: percentile(&sorted, 99.0),
            min_ms: sorted[0],
            max_ms: sorted[n - 1],
            mean_ms: sorted.iter().sum::<f64>() / n as f64,
            batch_throughput_per_sec: batch_size as f64 * 1000.0 / batch_total_ms,
            n_iters: n,
        }
    }
}

pub fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    let n = sorted_ms.len();
    if n == 0 {
        return 0.0;
    }
    let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
    sorted_ms[idx.min(n - 1)]
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm_a * norm_b + 1e-10)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ParityResult {
    pub min_cosine: f32,
    pub max_cosine: f32,
    pub mean_cosine: f32,
    pub n_below_threshold: usize,
    pub threshold: f32,
    pub passed: bool,
}

impl ParityResult {
    pub fn compute(similarities: &[f32], threshold: f32) -> Self {
        let n = similarities.len();
        let min_cosine = similarities.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_cosine = similarities.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let mean_cosine = similarities.iter().sum::<f32>() / n as f32;
        let n_below_threshold = similarities.iter().filter(|&&s| s < threshold).count();
        ParityResult {
            min_cosine,
            max_cosine,
            mean_cosine,
            n_below_threshold,
            threshold,
            passed: n_below_threshold == 0,
        }
    }
}
