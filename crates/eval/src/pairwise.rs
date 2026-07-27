//! Blind pairwise judging (#269, FR-002..FR-011): scores two backends' extractions
//! against each other, per chunk and per axis, without either side being a privileged
//! reference. Operates entirely on `BackendRunResult`s already produced by
//! `runner::run_backend` — makes zero extraction calls of its own (FR-009).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use lcg_core::ExtractionResult;

use crate::corpus::CorpusChunk;
use crate::judge::{JudgeClient, PairwiseVerdict, PairwiseWinner};
use crate::judge_cache::{cache_key, JudgeCache};
use crate::runner::BackendRunResult;

/// SC-001: two independent samples of the same model, judged pairwise, must land within
/// this band on every axis — the pairwise analogue of the noise floor. A win rate outside
/// it means the judge has a position bias, not that either side is genuinely different.
pub const CALIBRATION_BAND_LOW: f64 = 0.45;
pub const CALIBRATION_BAND_HIGH: f64 = 0.55;

/// SC-002: above this order-inconsistency rate, the judge flips its verdict on more than
/// 1 in 5 chunks when the slot order is reversed — the aggregate win rate is then not
/// distinguishable from position-bias noise and the pairwise result is not to be trusted.
pub const ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD: f64 = 0.20;

/// FR-005: the three axes a pairwise verdict is judged on, kept distinct rather than
/// collapsed into one pair-level figure (matches `CandidateReport`'s existing convention
/// of never folding entity/edge/summary F1 together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairwiseAxis {
    Entities,
    Edges,
    Summary,
}

impl PairwiseAxis {
    pub const ALL: [PairwiseAxis; 3] = [Self::Entities, Self::Edges, Self::Summary];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Entities => "entities",
            Self::Edges => "edges",
            Self::Summary => "summary",
        }
    }

    /// Disjoint from the reference-mode `judge()` prompt-name family
    /// (`"extract_nodes..."` / `"extract_edges..."`) so pairwise and matching-mode cache
    /// entries can never collide (FR-006).
    pub fn prompt_name(&self) -> &'static str {
        match self {
            Self::Entities => "pairwise.extract_nodes.extract_text",
            Self::Edges => "pairwise.extract_edges.default",
            Self::Summary => "pairwise.extract_nodes.extract_summaries_batch",
        }
    }

    /// The judge-facing payload for this axis, mirroring the shape the reference-mode
    /// judge prompt already uses for the same axis (`main.rs::score_candidate`).
    pub fn payload(&self, extraction: &ExtractionResult) -> Value {
        match self {
            Self::Entities => json!({ "extracted_entities": extraction.entities }),
            Self::Edges => json!({ "edges": extraction.edges }),
            Self::Summary => {
                let summaries: Vec<&str> = extraction
                    .entities
                    .iter()
                    .map(|e| e.summary.as_str())
                    .collect();
                json!({ "summaries": summaries })
            }
        }
    }
}

/// FR-003: deterministic, reproducible slot assignment — derived from a hash of the chunk
/// key plus the two backend names, never wall-clock or RNG. SHA-256 (not
/// `std::collections::hash_map::DefaultHasher`, whose output isn't guaranteed stable
/// across Rust/std versions) is already a harness dependency via `judge_cache::cache_key`.
pub fn chunk_pair_seed(chunk_key: &str, backend_a: &str, backend_b: &str) -> u64 {
    let input = format!("{chunk_key}|{backend_a}|{backend_b}");
    let digest = Sha256::digest(input.as_bytes());
    u64::from_be_bytes(
        digest[0..8]
            .try_into()
            .expect("sha256 digest is at least 8 bytes"),
    )
}

/// Which of the two named backends occupies "slot A" in the deterministic primary
/// ordering for this chunk. The seed only fixes which physical backend a given call's
/// slot A refers to — FR-004's required second (flipped) order is always judged too,
/// regardless of which ordering this returns as primary.
fn primary_order<'a>(
    chunk_key: &str,
    backend_a: &'a str,
    backend_b: &'a str,
) -> (&'a str, &'a str) {
    if chunk_pair_seed(chunk_key, backend_a, backend_b).is_multiple_of(2) {
        (backend_a, backend_b)
    } else {
        (backend_b, backend_a)
    }
}

/// FR-007/FR-010: the aggregated per-pair, per-axis tally — wins for each named side,
/// ties, the order-inconsistency count, and how many chunks were actually compared versus
/// skipped (present on only one side, per FR-010).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AxisTally {
    pub wins_a: usize,
    pub wins_b: usize,
    pub ties: usize,
    pub inconsistent: usize,
    pub chunks_compared: usize,
    pub chunks_skipped: usize,
}

impl AxisTally {
    /// FR-007's "win rate" excludes ties from the denominator: an unbiased judge's
    /// baseline tie rate would otherwise push a genuinely 50/50 noise-floor pair below the
    /// SC-001 calibration band for no reason related to position bias. `0.5` (no signal
    /// either way) when there are no decisive (non-tied) comparisons at all.
    pub fn win_rate_a(&self) -> f64 {
        let decisive = self.wins_a + self.wins_b;
        if decisive == 0 {
            0.5
        } else {
            self.wins_a as f64 / decisive as f64
        }
    }

    pub fn order_inconsistency_rate(&self) -> f64 {
        if self.chunks_compared == 0 {
            0.0
        } else {
            self.inconsistent as f64 / self.chunks_compared as f64
        }
    }
}

/// Cache-first pairwise judge lookup (SC-005: a cache hit makes zero new judge calls).
/// The chunk text is embedded symmetrically in both cache-key operands (Plan's Key
/// Decision) — `cache_key`'s existing order-sensitivity on its two operands already makes
/// the two slot orders yield distinct keys, so `judge_cache::cache_key` needs no changes.
async fn judged_pairwise(
    judge: &dyn JudgeClient,
    cache: &JudgeCache,
    judge_model: &str,
    prompt_name: &str,
    chunk_text: &str,
    slot_a: &Value,
    slot_b: &Value,
) -> Result<PairwiseVerdict, String> {
    let key_operand_a = json!({ "chunk": chunk_text, "slot": slot_a });
    let key_operand_b = json!({ "chunk": chunk_text, "slot": slot_b });
    let key = cache_key(prompt_name, judge_model, &key_operand_a, &key_operand_b);
    if let Some(v) = cache.get_pairwise(&key) {
        return Ok(v);
    }
    let v = judge
        .judge_pairwise(prompt_name, chunk_text, slot_a, slot_b)
        .await?;
    cache.insert_pairwise(key, v.clone())?;
    Ok(v)
}

/// Scores one backend pair on one axis across every chunk (FR-004/FR-010). Each chunk
/// present on both sides is judged in both slot orders; agreeing verdicts count as a win
/// for the agreed side, disagreeing verdicts count as a tie and increment the
/// order-inconsistency counter. A chunk missing (an `Err` result, e.g. a cassette miss) on
/// either side is tallied as skipped, never as a loss.
pub async fn score_pair_axis(
    judge: &dyn JudgeClient,
    cache: &JudgeCache,
    judge_model: &str,
    axis: PairwiseAxis,
    chunks: &[CorpusChunk],
    run_a: &BackendRunResult,
    run_b: &BackendRunResult,
) -> Result<AxisTally, String> {
    let mut tally = AxisTally::default();

    for (i, chunk) in chunks.iter().enumerate() {
        let (Some(chunk_a), Some(chunk_b)) =
            (run_a.chunk_results.get(i), run_b.chunk_results.get(i))
        else {
            tally.chunks_skipped += 1;
            continue;
        };
        let (Ok(extraction_a), Ok(extraction_b)) = (&chunk_a.result, &chunk_b.result) else {
            tally.chunks_skipped += 1;
            continue;
        };
        tally.chunks_compared += 1;

        let (first_name, second_name) =
            primary_order(&chunk.title, &run_a.backend_name, &run_b.backend_name);
        let payload_for = |name: &str| -> Value {
            if name == run_a.backend_name {
                axis.payload(extraction_a)
            } else {
                axis.payload(extraction_b)
            }
        };
        let payload_first = payload_for(first_name);
        let payload_second = payload_for(second_name);

        let primary_verdict = judged_pairwise(
            judge,
            cache,
            judge_model,
            axis.prompt_name(),
            &chunk.prose,
            &payload_first,
            &payload_second,
        )
        .await?;
        let flipped_verdict = judged_pairwise(
            judge,
            cache,
            judge_model,
            axis.prompt_name(),
            &chunk.prose,
            &payload_second,
            &payload_first,
        )
        .await?;

        let physical_primary = match primary_verdict.winner {
            PairwiseWinner::A => Some(first_name),
            PairwiseWinner::B => Some(second_name),
            PairwiseWinner::Tie => None,
        };
        let physical_flipped = match flipped_verdict.winner {
            PairwiseWinner::A => Some(second_name),
            PairwiseWinner::B => Some(first_name),
            PairwiseWinner::Tie => None,
        };

        if physical_primary == physical_flipped {
            match physical_primary {
                Some(name) if name == run_a.backend_name => tally.wins_a += 1,
                Some(name) if name == run_b.backend_name => tally.wins_b += 1,
                _ => tally.ties += 1,
            }
        } else {
            tally.ties += 1;
            tally.inconsistent += 1;
        }
    }

    Ok(tally)
}

/// One backend pair's tally on one axis — the unit `score_all_pairs` produces per
/// (pair, axis) combination, ready for `report.rs` to translate into a `PairwiseReportEntry`.
#[derive(Debug, Clone, PartialEq)]
pub struct PairwiseAxisResult {
    pub backend_a: String,
    pub backend_b: String,
    pub axis: PairwiseAxis,
    pub tally: AxisTally,
}

/// FR-008: covers every unordered backend pair present in `run_results` — including
/// reference-vs-candidate, which is not an incidental extra but User Story 2's mandatory
/// calibration control whenever the run configures it. `run_results` order is preserved
/// (insertion order from `--backend`), so this is a plain nested-index iteration rather
/// than a dedicated `BackendPair` type — there is no behavior a struct would add here.
pub async fn score_all_pairs(
    chunks: &[CorpusChunk],
    run_results: &[BackendRunResult],
    judge: &dyn JudgeClient,
    cache: &JudgeCache,
    judge_model: &str,
) -> Result<Vec<PairwiseAxisResult>, String> {
    let mut results = Vec::new();
    for i in 0..run_results.len() {
        for j in (i + 1)..run_results.len() {
            for axis in PairwiseAxis::ALL {
                let tally = score_pair_axis(
                    judge,
                    cache,
                    judge_model,
                    axis,
                    chunks,
                    &run_results[i],
                    &run_results[j],
                )
                .await?;
                results.push(PairwiseAxisResult {
                    backend_a: run_results[i].backend_name.clone(),
                    backend_b: run_results[j].backend_name.clone(),
                    axis,
                    tally,
                });
            }
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{ChunkResult, StructuredOutputCounts};
    use futures::future::BoxFuture;
    use lcg_core::{ExtractedEntity, ExtractionResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn chunk(title: &str) -> CorpusChunk {
        CorpusChunk {
            title: title.to_string(),
            revision_id: 1,
            prose: format!("prose for {title}"),
        }
    }

    fn ok_chunk_result(title: &str, entity_name: &str) -> ChunkResult {
        ChunkResult {
            chunk_title: title.to_string(),
            latency_ms: 0,
            result: Ok(ExtractionResult {
                entities: vec![ExtractedEntity {
                    name: entity_name.to_string(),
                    entity_type: "Thing".to_string(),
                    summary: "a thing".to_string(),
                }],
                edges: vec![],
            }),
        }
    }

    fn err_chunk_result(title: &str) -> ChunkResult {
        ChunkResult {
            chunk_title: title.to_string(),
            latency_ms: 0,
            result: Err("cassette miss".to_string()),
        }
    }

    /// An `Ok` extraction with no entities/edges at all — a chunk where the backend
    /// extracted nothing, not one that failed to run (see `err_chunk_result` for the
    /// latter). Distinct scenarios: an empty extraction is still compared (FR-005's "both
    /// being empty" is a defined tie case, per `PAIRWISE_JUDGE_PROMPT`), whereas an `Err`
    /// chunk is skipped entirely (FR-010).
    fn empty_chunk_result(title: &str) -> ChunkResult {
        ChunkResult {
            chunk_title: title.to_string(),
            latency_ms: 0,
            result: Ok(ExtractionResult {
                entities: vec![],
                edges: vec![],
            }),
        }
    }

    fn run(name: &str, chunk_results: Vec<ChunkResult>) -> BackendRunResult {
        BackendRunResult {
            backend_name: name.to_string(),
            chunk_results,
            structured_output: StructuredOutputCounts::default(),
            vocabulary_compliance: None,
        }
    }

    // ── chunk_pair_seed: FR-003 determinism ────────────────────────────────────────

    #[test]
    fn seed_is_deterministic_across_repeated_calls() {
        let a = chunk_pair_seed("chunk-1", "x", "y");
        let b = chunk_pair_seed("chunk-1", "x", "y");
        assert_eq!(a, b);
    }

    #[test]
    fn seed_changes_with_chunk_key() {
        let a = chunk_pair_seed("chunk-1", "x", "y");
        let b = chunk_pair_seed("chunk-2", "x", "y");
        assert_ne!(a, b);
    }

    #[test]
    fn seed_changes_with_backend_names() {
        let a = chunk_pair_seed("chunk-1", "x", "y");
        let b = chunk_pair_seed("chunk-1", "x", "z");
        assert_ne!(a, b);
    }

    // ── score_pair_axis: FR-004/FR-010 aggregation ─────────────────────────────────

    /// A judge whose verdict depends only on payload *content* (lexicographic string
    /// comparison), never on which physical backend happens to occupy slot A/B for a given
    /// call — so it is naturally consistent across both slot orders (an agreeing judge).
    struct LexicographicJudge;

    impl JudgeClient for LexicographicJudge {
        fn judge<'a>(
            &'a self,
            _prompt_name: &'a str,
            _reference: &'a Value,
            _candidate: &'a Value,
        ) -> BoxFuture<'a, Result<crate::judge::JudgeVerdict, String>> {
            unreachable!("pairwise tests never call judge()")
        }

        fn judge_pairwise<'a>(
            &'a self,
            _prompt_name: &'a str,
            _chunk_text: &'a str,
            slot_a: &'a Value,
            slot_b: &'a Value,
        ) -> BoxFuture<'a, Result<PairwiseVerdict, String>> {
            let a_str = slot_a.to_string();
            let b_str = slot_b.to_string();
            let winner = match a_str.cmp(&b_str) {
                std::cmp::Ordering::Greater => PairwiseWinner::A,
                std::cmp::Ordering::Less => PairwiseWinner::B,
                std::cmp::Ordering::Equal => PairwiseWinner::Tie,
            };
            Box::pin(async move {
                Ok(PairwiseVerdict {
                    winner,
                    rationale: String::new(),
                })
            })
        }
    }

    /// A judge exhibiting pure position bias: always favors whichever physical backend is
    /// in slot A, regardless of content. Used to exercise FR-004's order-inconsistency
    /// counter, which must fire whenever the two slot orders disagree.
    struct AlwaysSlotAJudge;

    impl JudgeClient for AlwaysSlotAJudge {
        fn judge<'a>(
            &'a self,
            _prompt_name: &'a str,
            _reference: &'a Value,
            _candidate: &'a Value,
        ) -> BoxFuture<'a, Result<crate::judge::JudgeVerdict, String>> {
            unreachable!("pairwise tests never call judge()")
        }

        fn judge_pairwise<'a>(
            &'a self,
            _prompt_name: &'a str,
            _chunk_text: &'a str,
            _slot_a: &'a Value,
            _slot_b: &'a Value,
        ) -> BoxFuture<'a, Result<PairwiseVerdict, String>> {
            Box::pin(async move {
                Ok(PairwiseVerdict {
                    winner: PairwiseWinner::A,
                    rationale: String::new(),
                })
            })
        }
    }

    fn empty_cache() -> (TempDir, JudgeCache) {
        let dir = TempDir::new().unwrap();
        let cache = JudgeCache::load(dir.path().join("cache.jsonl")).unwrap();
        (dir, cache)
    }

    #[tokio::test]
    async fn agreeing_verdicts_produce_a_win_for_the_agreed_side() {
        let chunks = vec![chunk("A")];
        // "zzz" always sorts lexicographically greater than "aaa" in the serialized
        // payload, so LexicographicJudge always picks backend "y" here regardless of
        // which physical backend the deterministic seed puts in slot A.
        let run_x = run("x", vec![ok_chunk_result("A", "aaa")]);
        let run_y = run("y", vec![ok_chunk_result("A", "zzz")]);
        let (_dir, cache) = empty_cache();

        let tally = score_pair_axis(
            &LexicographicJudge,
            &cache,
            "judge-model",
            PairwiseAxis::Entities,
            &chunks,
            &run_x,
            &run_y,
        )
        .await
        .unwrap();

        assert_eq!(tally.chunks_compared, 1);
        assert_eq!(tally.chunks_skipped, 0);
        assert_eq!(tally.wins_b, 1, "backend y's payload always sorts greater");
        assert_eq!(tally.wins_a, 0);
        assert_eq!(tally.ties, 0);
        assert_eq!(tally.inconsistent, 0);
    }

    #[tokio::test]
    async fn disagreeing_verdicts_produce_a_tie_and_increment_inconsistency() {
        let chunks = vec![chunk("A")];
        let run_x = run("x", vec![ok_chunk_result("A", "e1")]);
        let run_y = run("y", vec![ok_chunk_result("A", "e2")]);
        let (_dir, cache) = empty_cache();

        let tally = score_pair_axis(
            &AlwaysSlotAJudge,
            &cache,
            "judge-model",
            PairwiseAxis::Entities,
            &chunks,
            &run_x,
            &run_y,
        )
        .await
        .unwrap();

        assert_eq!(tally.chunks_compared, 1);
        assert_eq!(tally.wins_a, 0);
        assert_eq!(tally.wins_b, 0);
        assert_eq!(tally.ties, 1);
        assert_eq!(
            tally.inconsistent, 1,
            "a judge that always favors slot A must flip its physical answer when order flips"
        );
    }

    #[tokio::test]
    async fn either_side_err_chunk_is_skipped_not_scored_as_a_loss() {
        let chunks = vec![chunk("A"), chunk("B")];
        let run_x = run("x", vec![ok_chunk_result("A", "e1"), err_chunk_result("B")]);
        let run_y = run(
            "y",
            vec![ok_chunk_result("A", "e1"), ok_chunk_result("B", "e2")],
        );
        let (_dir, cache) = empty_cache();

        let tally = score_pair_axis(
            &LexicographicJudge,
            &cache,
            "judge-model",
            PairwiseAxis::Entities,
            &chunks,
            &run_x,
            &run_y,
        )
        .await
        .unwrap();

        assert_eq!(tally.chunks_compared, 1, "only chunk A had both sides Ok");
        assert_eq!(
            tally.chunks_skipped, 1,
            "chunk B must be tallied as skipped"
        );
        assert_eq!(
            tally.wins_a + tally.wins_b + tally.ties,
            1,
            "the skipped chunk must never contribute a win/loss/tie"
        );
    }

    /// Edge Case (spec): "One or both extractions empty for a chunk → a defined verdict per
    /// axis, not a panic." A judge that always ties (mirroring `PAIRWISE_JUDGE_PROMPT`'s
    /// explicit "both being empty ... it is a tie" instruction) must be exercised over
    /// empty-on-both-sides, empty-on-one-side, and non-empty chunks without panicking, with
    /// every chunk counted as compared (not skipped — an empty extraction is a valid `Ok`
    /// result, distinct from a cassette miss).
    #[tokio::test]
    async fn empty_extractions_produce_a_defined_verdict_not_a_panic() {
        let chunks = vec![
            chunk("both-empty"),
            chunk("one-empty"),
            chunk("neither-empty"),
        ];
        let run_x = run(
            "x",
            vec![
                empty_chunk_result("both-empty"),
                empty_chunk_result("one-empty"),
                ok_chunk_result("neither-empty", "e1"),
            ],
        );
        let run_y = run(
            "y",
            vec![
                empty_chunk_result("both-empty"),
                ok_chunk_result("one-empty", "e2"),
                ok_chunk_result("neither-empty", "e2"),
            ],
        );
        let (_dir, cache) = empty_cache();

        for axis in PairwiseAxis::ALL {
            let tally = score_pair_axis(
                &AlwaysSlotAJudge,
                &cache,
                "judge-model",
                axis,
                &chunks,
                &run_x,
                &run_y,
            )
            .await
            .unwrap();

            assert_eq!(
                tally.chunks_compared, 3,
                "empty extractions are still Ok results, not skips, for axis {axis:?}"
            );
            assert_eq!(tally.chunks_skipped, 0);
        }
    }

    #[tokio::test]
    async fn symmetric_content_aware_judge_lands_within_calibration_band() {
        // User Story 2 shape: a content-aware (order-invariant) judge comparing two
        // backends whose payloads alternate which one "wins" chunk-by-chunk must produce a
        // win rate near 50%, the way two independent samples of the same model should.
        let mut chunks = Vec::new();
        let mut run_x_results = Vec::new();
        let mut run_y_results = Vec::new();
        for i in 0..20 {
            let title = format!("chunk-{i}");
            chunks.push(chunk(&title));
            if i % 2 == 0 {
                run_x_results.push(ok_chunk_result(&title, "zzz"));
                run_y_results.push(ok_chunk_result(&title, "aaa"));
            } else {
                run_x_results.push(ok_chunk_result(&title, "aaa"));
                run_y_results.push(ok_chunk_result(&title, "zzz"));
            }
        }
        let run_x = run("x", run_x_results);
        let run_y = run("y", run_y_results);
        let (_dir, cache) = empty_cache();

        let tally = score_pair_axis(
            &LexicographicJudge,
            &cache,
            "judge-model",
            PairwiseAxis::Entities,
            &chunks,
            &run_x,
            &run_y,
        )
        .await
        .unwrap();

        assert_eq!(tally.chunks_compared, 20);
        assert_eq!(tally.inconsistent, 0);
        let rate = tally.win_rate_a();
        assert!(
            (CALIBRATION_BAND_LOW..=CALIBRATION_BAND_HIGH).contains(&rate),
            "expected win rate within the SC-001 calibration band, got {rate}"
        );
    }

    /// A `JudgeClient` that counts every `judge_pairwise` call it actually serves — used
    /// to verify SC-005 (a repeat run against a pre-populated cache makes zero new calls).
    struct CountingPairwiseJudge {
        calls: AtomicUsize,
    }

    impl JudgeClient for CountingPairwiseJudge {
        fn judge<'a>(
            &'a self,
            _prompt_name: &'a str,
            _reference: &'a Value,
            _candidate: &'a Value,
        ) -> BoxFuture<'a, Result<crate::judge::JudgeVerdict, String>> {
            unreachable!("pairwise tests never call judge()")
        }

        fn judge_pairwise<'a>(
            &'a self,
            _prompt_name: &'a str,
            _chunk_text: &'a str,
            _slot_a: &'a Value,
            _slot_b: &'a Value,
        ) -> BoxFuture<'a, Result<PairwiseVerdict, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(PairwiseVerdict {
                    winner: PairwiseWinner::Tie,
                    rationale: String::new(),
                })
            })
        }
    }

    #[tokio::test]
    async fn repeat_run_against_populated_cache_makes_zero_new_judge_calls() {
        let chunks = vec![chunk("A")];
        let run_x = run("x", vec![ok_chunk_result("A", "e1")]);
        let run_y = run("y", vec![ok_chunk_result("A", "e2")]);
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("cache.jsonl");
        let judge = CountingPairwiseJudge {
            calls: AtomicUsize::new(0),
        };

        {
            let cache = JudgeCache::load(&cache_path).unwrap();
            score_pair_axis(
                &judge,
                &cache,
                "judge-model",
                PairwiseAxis::Entities,
                &chunks,
                &run_x,
                &run_y,
            )
            .await
            .unwrap();
        }
        // One chunk, both slot orders judged (FR-004) => exactly 2 calls on a cold cache.
        assert_eq!(judge.calls.load(Ordering::SeqCst), 2);

        {
            let cache = JudgeCache::load(&cache_path).unwrap();
            score_pair_axis(
                &judge,
                &cache,
                "judge-model",
                PairwiseAxis::Entities,
                &chunks,
                &run_x,
                &run_y,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            2,
            "a repeat run against the same cache must make zero new judge calls (SC-005)"
        );
    }

    #[tokio::test]
    async fn score_all_pairs_covers_every_unordered_pair_including_reference() {
        let chunks = vec![chunk("A")];
        let run_ref = run("reference", vec![ok_chunk_result("A", "e1")]);
        let run_cand = run("candidate", vec![ok_chunk_result("A", "e2")]);
        let run_third = run("third", vec![ok_chunk_result("A", "e3")]);
        let (_dir, cache) = empty_cache();

        let results = score_all_pairs(
            &chunks,
            &[run_ref, run_cand, run_third],
            &LexicographicJudge,
            &cache,
            "judge-model",
        )
        .await
        .unwrap();

        // C(3,2) = 3 pairs, each judged on all 3 axes (FR-008).
        assert_eq!(results.len(), 3 * PairwiseAxis::ALL.len());
        let pairs: std::collections::HashSet<(String, String)> = results
            .iter()
            .map(|r| (r.backend_a.clone(), r.backend_b.clone()))
            .collect();
        assert!(pairs.contains(&("reference".to_string(), "candidate".to_string())));
        assert!(pairs.contains(&("reference".to_string(), "third".to_string())));
        assert!(pairs.contains(&("candidate".to_string(), "third".to_string())));
    }

    #[test]
    fn win_rate_defaults_to_half_with_no_decisive_comparisons() {
        let tally = AxisTally {
            ties: 5,
            ..Default::default()
        };
        assert_eq!(tally.win_rate_a(), 0.5);
    }

    #[test]
    fn order_inconsistency_rate_is_zero_with_no_compared_chunks() {
        let tally = AxisTally::default();
        assert_eq!(tally.order_inconsistency_rate(), 0.0);
    }
}
