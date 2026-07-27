//! The harness's output artifact (FR-006/FR-007): per-candidate F1 for nodes/edges/
//! summaries, latency percentiles, error rate, and structured-output reliability as a
//! first-class metric — never folded into F1.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct LatencyPercentiles {
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
}

/// Nearest-rank percentile over the given latencies. Empty input reports all-zero rather
/// than panicking, so a backend that failed on every chunk still produces a valid report.
pub fn percentiles(mut latencies_ms: Vec<u64>) -> LatencyPercentiles {
    if latencies_ms.is_empty() {
        return LatencyPercentiles {
            p50_ms: 0,
            p95_ms: 0,
            p99_ms: 0,
        };
    }
    latencies_ms.sort_unstable();
    let pick = |p: f64| {
        let idx = ((latencies_ms.len() as f64 - 1.0) * p).round() as usize;
        latencies_ms[idx.min(latencies_ms.len() - 1)]
    };
    LatencyPercentiles {
        p50_ms: pick(0.50),
        p95_ms: pick(0.95),
        p99_ms: pick(0.99),
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct StructuredOutputReliability {
    pub clean: u64,
    pub recovered: u64,
    pub malformed: u64,
    pub malformed_rate: f64,
}

/// FR-007: how often a `Strict`-mode candidate emitted an entity or relation type outside
/// the ontology's declared vocabulary — a distinct failure mode from JSON-syntax validity
/// (`StructuredOutputReliability`), never folded into it (SC-003).
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct VocabularyComplianceReport {
    pub entities_checked: u64,
    pub entities_out_of_vocab: u64,
    pub entity_violation_rate: f64,
    pub edges_checked: u64,
    pub edges_out_of_vocab: u64,
    pub edge_violation_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateReport {
    pub backend_name: String,
    pub chunks_run: usize,
    /// Chunks where both the reference and this candidate's extraction succeeded — the
    /// actual denominator `strict_*_f1`/`judged_*_f1` are computed over. Can be smaller
    /// than `chunks_run` when either side errored on some chunks, so two candidates'
    /// F1 scores aren't necessarily computed over the same number of samples.
    pub chunks_scored: usize,
    pub errors: usize,
    pub error_rate: f64,
    pub latency: LatencyPercentiles,
    pub structured_output: StructuredOutputReliability,
    /// `Some` only for a `Strict`-mode run (FR-007); `None` for freeform/`Open` runs.
    pub vocabulary_compliance: Option<VocabularyComplianceReport>,
    pub strict_entity_f1: f64,
    pub strict_edge_f1: f64,
    pub judged_entity_f1: Option<f64>,
    /// Precision and recall are the diagnostic half of the judged score and F1 hides
    /// them: a model extracting a third more items than the reference is penalised
    /// identically to one missing a third, but the two mean opposite things. Both were
    /// already computed by `precision_recall_f1` and discarded before reaching here.
    pub judged_entity_precision: Option<f64>,
    pub judged_entity_recall: Option<f64>,
    pub judged_edge_f1: Option<f64>,
    pub judged_edge_precision: Option<f64>,
    pub judged_edge_recall: Option<f64>,
    pub judged_summary_f1: Option<f64>,
    /// Judge calls that failed after exhausting their retries. Non-fatal (see
    /// `score_candidate`), so a nonzero value means some chunks are missing from the
    /// judged averages — read it as a caveat on those numbers, not as a run failure.
    pub judge_errors: usize,
}

/// FR-007/FR-010: one backend pair's aggregated result on one axis (entities/edges/
/// summary — never folded together, matching `CandidateReport`'s convention). `wins`/
/// `losses` are from `backend_a`'s perspective: `wins` is how often `backend_a` won,
/// `losses` is how often `backend_b` won. A win rate is never reported without its
/// order-inconsistency rate alongside it (FR-007) — both live on this same struct.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PairwiseReportEntry {
    pub backend_a: String,
    pub backend_b: String,
    pub axis: String,
    pub wins: usize,
    pub losses: usize,
    pub ties: usize,
    /// Excludes ties from the denominator — see `pairwise::AxisTally::win_rate_a`'s doc for
    /// why (an unbiased judge's tie rate would otherwise depress a genuinely 50/50
    /// noise-floor pair below the SC-001 calibration band for no bias-related reason).
    pub win_rate: f64,
    pub order_inconsistency_rate: f64,
    pub chunks_compared: usize,
    /// FR-010: chunks present on only one side of the pair — never silently folded into
    /// `losses`.
    pub chunks_skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub corpus_size: usize,
    pub reference_backend: String,
    /// FR-003: which regime produced this report — "freeform", "open", or "strict" — so
    /// freeform and ontology-constrained reports are never confused when compared or
    /// archived side by side.
    pub ontology_mode: String,
    pub candidates: Vec<CandidateReport>,
    /// `None` when `--judge-mode` is omitted or set to `reference` (SC-004: this keeps
    /// `to_json()`'s output byte-identical to the pre-#269 shape — `skip_serializing_if`
    /// means the key is entirely absent, not present-but-null, whenever pairwise mode
    /// wasn't requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairwise: Option<Vec<PairwiseReportEntry>>,
}

impl Report {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn render_human_readable(&self) -> String {
        let mut out = format!(
            "Extraction-quality eval report — corpus size: {}, reference backend: {}, \
             ontology mode: {}\n\n",
            self.corpus_size, self.reference_backend, self.ontology_mode
        );
        // `None` judged F1 can mean either "no judge client configured" (no
        // ANTHROPIC_API_KEY) or "zero chunk pairs succeeded on both sides to score" —
        // report `chunks_scored` alongside rather than guessing which one applied.
        let fmt_opt = |v: Option<f64>| {
            v.map(|x| format!("{x:.3}"))
                .unwrap_or_else(|| "n/a".to_string())
        };
        for c in &self.candidates {
            out.push_str(&format!(
                "== {} ==\n  chunks run: {}  chunks scored: {}  errors: {} ({:.1}%)\n  \
                 latency p50/p95/p99 (ms): {}/{}/{}\n  \
                 structured output: clean={} recovered={} malformed={} (malformed rate {:.1}%)\n",
                c.backend_name,
                c.chunks_run,
                c.chunks_scored,
                c.errors,
                c.error_rate * 100.0,
                c.latency.p50_ms,
                c.latency.p95_ms,
                c.latency.p99_ms,
                c.structured_output.clean,
                c.structured_output.recovered,
                c.structured_output.malformed,
                c.structured_output.malformed_rate * 100.0,
            ));
            // Vocabulary compliance (FR-007) is a distinct metric from structured-output
            // reliability above — rendered as its own line, only when applicable (Strict).
            if let Some(v) = &c.vocabulary_compliance {
                out.push_str(&format!(
                    "  vocabulary compliance: entities {}/{} out-of-vocab ({:.1}%)  \
                     edges {}/{} out-of-vocab ({:.1}%)\n",
                    v.entities_out_of_vocab,
                    v.entities_checked,
                    v.entity_violation_rate * 100.0,
                    v.edges_out_of_vocab,
                    v.edges_checked,
                    v.edge_violation_rate * 100.0,
                ));
            }
            out.push_str(&format!(
                "  strict F1 — entities: {:.3}  edges: {:.3}\n  \
                 judged F1 — entities: {}  edges: {}  summaries: {}\n  \
                 judged entities — precision: {}  recall: {}\n  \
                 judged edges    — precision: {}  recall: {}\n",
                c.strict_entity_f1,
                c.strict_edge_f1,
                fmt_opt(c.judged_entity_f1),
                fmt_opt(c.judged_edge_f1),
                fmt_opt(c.judged_summary_f1),
                fmt_opt(c.judged_entity_precision),
                fmt_opt(c.judged_entity_recall),
                fmt_opt(c.judged_edge_precision),
                fmt_opt(c.judged_edge_recall),
            ));
            if c.judge_errors > 0 {
                out.push_str(&format!(
                    "  judge errors: {} (those chunks are absent from the judged averages)\n",
                    c.judge_errors
                ));
            }
            out.push('\n');
        }

        // FR-007: rendered only when pairwise mode actually ran — omitted entirely (not
        // an empty section header) when `--judge-mode` is `reference` or omitted, so
        // reference-mode output stays unchanged (SC-004).
        if let Some(pairwise) = &self.pairwise {
            if !pairwise.is_empty() {
                out.push_str("== pairwise judging ==\n");
                for e in pairwise {
                    out.push_str(&format!(
                        "  {} vs {} [{}]: wins {} losses {} ties {}  win rate {:.1}%  \
                         order-inconsistency rate {:.1}%  chunks compared {}  skipped {}\n",
                        e.backend_a,
                        e.backend_b,
                        e.axis,
                        e.wins,
                        e.losses,
                        e.ties,
                        e.win_rate * 100.0,
                        e.order_inconsistency_rate * 100.0,
                        e.chunks_compared,
                        e.chunks_skipped,
                    ));
                }
                out.push('\n');
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_empty_is_all_zero() {
        assert_eq!(
            percentiles(vec![]),
            LatencyPercentiles {
                p50_ms: 0,
                p95_ms: 0,
                p99_ms: 0
            }
        );
    }

    #[test]
    fn percentiles_single_value() {
        let p = percentiles(vec![42]);
        assert_eq!(p.p50_ms, 42);
        assert_eq!(p.p95_ms, 42);
        assert_eq!(p.p99_ms, 42);
    }

    #[test]
    fn percentiles_sorted_input_independent_of_order() {
        let a = percentiles(vec![1, 2, 3, 4, 5]);
        let b = percentiles(vec![5, 3, 1, 4, 2]);
        assert_eq!(a, b);
    }

    #[test]
    fn percentiles_p50_is_median_ish() {
        let p = percentiles((1..=100).collect());
        assert!(p.p50_ms >= 49 && p.p50_ms <= 51);
        assert!(p.p95_ms >= 94 && p.p95_ms <= 96);
        assert!(p.p99_ms >= 98 && p.p99_ms <= 100);
    }

    fn sample_report() -> Report {
        Report {
            corpus_size: 2,
            reference_backend: "baseline".to_string(),
            ontology_mode: "freeform".to_string(),
            candidates: vec![CandidateReport {
                backend_name: "baseline".to_string(),
                chunks_run: 2,
                chunks_scored: 2,
                errors: 0,
                error_rate: 0.0,
                latency: LatencyPercentiles {
                    p50_ms: 10,
                    p95_ms: 20,
                    p99_ms: 30,
                },
                structured_output: StructuredOutputReliability {
                    clean: 2,
                    recovered: 0,
                    malformed: 0,
                    malformed_rate: 0.0,
                },
                vocabulary_compliance: None,
                strict_entity_f1: 0.771,
                strict_edge_f1: 0.771,
                judged_entity_f1: Some(0.978),
                judged_entity_precision: Some(0.981),
                judged_entity_recall: Some(0.975),
                judged_edge_f1: Some(0.978),
                judged_edge_precision: Some(0.981),
                judged_edge_recall: Some(0.975),
                judged_summary_f1: None,
                judge_errors: 0,
            }],
            pairwise: None,
        }
    }

    fn sample_strict_report() -> Report {
        let mut report = sample_report();
        report.ontology_mode = "strict".to_string();
        report.candidates[0].vocabulary_compliance = Some(VocabularyComplianceReport {
            entities_checked: 10,
            entities_out_of_vocab: 2,
            entity_violation_rate: 0.2,
            edges_checked: 5,
            edges_out_of_vocab: 1,
            edge_violation_rate: 0.2,
        });
        report
    }

    fn sample_pairwise_report() -> Report {
        let mut report = sample_report();
        report.pairwise = Some(vec![PairwiseReportEntry {
            backend_a: "baseline".to_string(),
            backend_b: "candidate".to_string(),
            axis: "entities".to_string(),
            wins: 6,
            losses: 5,
            ties: 9,
            win_rate: 0.545,
            order_inconsistency_rate: 0.05,
            chunks_compared: 20,
            chunks_skipped: 2,
        }]);
        report
    }

    /// SC-004: with `--judge-mode` omitted (the `pairwise` field always `None`), the JSON
    /// report must be byte-identical to the pre-#269 shape. This golden string was captured
    /// from `sample_report().to_json()` *before* the `pairwise` field was added to `Report`,
    /// so any accidental key addition/reordering/nulling here is caught immediately.
    #[test]
    fn json_report_is_byte_identical_to_pre_pairwise_golden_sc004() {
        let golden = "{\n  \"corpus_size\": 2,\n  \"reference_backend\": \"baseline\",\n  \"ontology_mode\": \"freeform\",\n  \"candidates\": [\n    {\n      \"backend_name\": \"baseline\",\n      \"chunks_run\": 2,\n      \"chunks_scored\": 2,\n      \"errors\": 0,\n      \"error_rate\": 0.0,\n      \"latency\": {\n        \"p50_ms\": 10,\n        \"p95_ms\": 20,\n        \"p99_ms\": 30\n      },\n      \"structured_output\": {\n        \"clean\": 2,\n        \"recovered\": 0,\n        \"malformed\": 0,\n        \"malformed_rate\": 0.0\n      },\n      \"vocabulary_compliance\": null,\n      \"strict_entity_f1\": 0.771,\n      \"strict_edge_f1\": 0.771,\n      \"judged_entity_f1\": 0.978,\n      \"judged_edge_f1\": 0.978,\n      \"judged_summary_f1\": null\n    }\n  ]\n}";
        assert_eq!(sample_report().to_json(), golden);
    }

    #[test]
    fn json_report_round_trips_through_serde_json() {
        let report = sample_report();
        let json = report.to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["corpus_size"], 2);
        assert_eq!(value["candidates"][0]["backend_name"], "baseline");
        assert_eq!(value["candidates"][0]["structured_output"]["malformed"], 0);
    }

    #[test]
    fn human_readable_report_mentions_key_metrics() {
        let out = sample_report().render_human_readable();
        assert!(out.contains("baseline"));
        assert!(out.contains("0.771"));
        assert!(out.contains("0.978"));
        assert!(out.contains("n/a"));
        assert!(out.contains("chunks scored"));
    }

    #[test]
    fn json_report_records_ontology_mode() {
        let report = sample_strict_report();
        let json = report.to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["ontology_mode"], "strict");
        assert_eq!(
            value["candidates"][0]["vocabulary_compliance"]["entities_out_of_vocab"],
            2
        );
    }

    #[test]
    fn freeform_report_has_no_vocabulary_compliance_field_value() {
        let report = sample_report();
        let json = report.to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["ontology_mode"], "freeform");
        assert!(value["candidates"][0]["vocabulary_compliance"].is_null());
    }

    #[test]
    fn human_readable_report_renders_ontology_mode_and_vocab_compliance_distinctly() {
        let out = sample_strict_report().render_human_readable();
        assert!(out.contains("ontology mode: strict"));
        assert!(out.contains("vocabulary compliance"));
        assert!(out.contains("2/10"));
        assert!(out.contains("1/5"));
        // Vocabulary compliance must be a visibly separate line from structured output —
        // not merged into it (SC-003).
        let structured_line_idx = out.find("structured output:").unwrap();
        let vocab_line_idx = out.find("vocabulary compliance:").unwrap();
        assert!(vocab_line_idx > structured_line_idx);
        let structured_line_end =
            out[structured_line_idx..].find('\n').unwrap() + structured_line_idx;
        assert!(
            vocab_line_idx >= structured_line_end,
            "vocabulary compliance must be on its own line, not appended to structured output's"
        );
    }

    #[test]
    fn human_readable_report_omits_vocab_compliance_line_when_not_applicable() {
        let out = sample_report().render_human_readable();
        assert!(!out.contains("vocabulary compliance"));
    }

    // ── pairwise report section (FR-007, SC-004) ───────────────────────────────────

    #[test]
    fn pairwise_field_absent_from_json_when_none() {
        let json = sample_report().to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            value.get("pairwise").is_none(),
            "the pairwise key must be entirely absent (not present-but-null) when \
             --judge-mode is omitted, per SC-004"
        );
    }

    #[test]
    fn pairwise_field_present_and_populated_in_json_when_some() {
        let json = sample_pairwise_report().to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["pairwise"][0]["backend_a"], "baseline");
        assert_eq!(value["pairwise"][0]["backend_b"], "candidate");
        assert_eq!(value["pairwise"][0]["axis"], "entities");
        assert_eq!(value["pairwise"][0]["wins"], 6);
        assert_eq!(value["pairwise"][0]["losses"], 5);
        assert_eq!(value["pairwise"][0]["ties"], 9);
        assert_eq!(value["pairwise"][0]["chunks_compared"], 20);
        assert_eq!(value["pairwise"][0]["chunks_skipped"], 2);
    }

    #[test]
    fn human_readable_report_omits_pairwise_section_when_none() {
        let out = sample_report().render_human_readable();
        assert!(!out.contains("pairwise judging"));
    }

    #[test]
    fn human_readable_report_renders_pairwise_section_with_inconsistency_rate() {
        let out = sample_pairwise_report().render_human_readable();
        assert!(out.contains("pairwise judging"));
        assert!(out.contains("baseline vs candidate"));
        assert!(out.contains("[entities]"));
        // FR-007: a win rate is never reported without its order-inconsistency rate.
        assert!(out.contains("win rate"));
        assert!(out.contains("order-inconsistency rate"));
        assert!(out.contains("chunks compared"));
        assert!(out.contains("skipped"));
    }
}
