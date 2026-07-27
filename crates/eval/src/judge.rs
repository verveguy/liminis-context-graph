//! LLM-as-judge scoring (FR-005/FR-008). The judge prompt and precision/recall/F1
//! derivation are ported verbatim from the prior Python harness
//! (`verveguy/liminis-framework@main:eval/extraction-quality/src/eval_extraction/judge.py`,
//! reproduced in the Research stage for issue #228) — not re-derived from intuition.
//!
//! The judge is intentionally *not* an `Extractor` implementation: it asks a free-form
//! semantic-equivalence question via a raw Anthropic Messages API call, independent of
//! whichever backends are under test (including when comparing two non-Anthropic
//! candidates against each other).

use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::sleep;

/// Verbatim from the ported Python source, `JUDGE_PROMPT` in `judge.py` — the `{prompt_name}`/
/// `{ref}`/`{cand}` placeholders are substituted via `str::replace` (not `format!`, so the
/// literal `{`/`}` in the JSON example below are not doubled the way Python's `.format()`
/// required).
pub const JUDGE_PROMPT: &str = "You are evaluating whether two structured extractions express the same information.

PROMPT TYPE: {prompt_name}

REFERENCE EXTRACTION:
{ref}

CANDIDATE EXTRACTION:
{cand}

For each ITEM in the reference and candidate item lists, determine semantic equivalence.

Two items are equivalent if they convey the same information, even with different wording:
- Entities: same real-world entity, allowing name variations (articles, abbreviations, capitalisation, \"the X\" vs \"X\"). Allow minor entity_type differences if the underlying entity is clearly the same.
- Edges/relationships: same source entity, same target entity (allow name variations as above), AND the relation type expresses the same semantic meaning. Match \"won\" with \"won_award\", \"authored\" with \"wrote\", \"located_in\" with \"is_located_in\", \"is_capital_of\" with \"capital_of\", \"discussed_in_agenda_with\" with \"is_agenda_item_alongside\". Reverse-direction relations match if semantically symmetric (e.g. \"X awarded_to Y\" matches \"Y won X\"). Do NOT match relations with different semantic meaning (e.g. \"located_in\" with \"founded_in\", \"authored\" with \"edited\").
- Summaries: convey the same key information about the same subject — accept paraphrases.

ITEM LISTS (use the appropriate field):
- extract_nodes.extract_text → use response[\"extracted_entities\"] (list)
- extract_edges.* → use response[\"edges\"] (list)
- extract_nodes.extract_summaries_batch → use response[\"summaries\"] (list)

Indices are 0-based positions within that list.

Respond with ONLY this JSON (no preamble, no commentary):
{
  \"matched\": [[ref_idx, cand_idx], ...],
  \"unmatched_reference\": [ref_idx, ...],
  \"unmatched_candidate\": [cand_idx, ...]
}
";

/// The judge's per-comparison verdict — matches the JSON shape `JUDGE_PROMPT` asks for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct JudgeVerdict {
    pub matched: Vec<(usize, usize)>,
    pub unmatched_reference: Vec<usize>,
    pub unmatched_candidate: Vec<usize>,
}

/// Precision/recall/F1 derivation ported verbatim: each defaults to 1.0 (precision/recall)
/// or 0.0 (F1) on an empty denominator, exactly as the Python source does.
pub fn precision_recall_f1(v: &JudgeVerdict) -> (f64, f64, f64) {
    let matched = v.matched.len() as f64;
    let cand_total = matched + v.unmatched_candidate.len() as f64;
    let ref_total = matched + v.unmatched_reference.len() as f64;
    let precision = if cand_total == 0.0 {
        1.0
    } else {
        matched / cand_total
    };
    let recall = if ref_total == 0.0 {
        1.0
    } else {
        matched / ref_total
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    (precision, recall, f1)
}

/// Blind pairwise judge prompt (FR-002/#269): unlike `JUDGE_PROMPT`, this one includes the
/// source chunk text — deciding *which* of two extractions better captures a source is a
/// different question from deciding whether two items are semantically equivalent, and
/// answering it requires reading the source. The two extractions are presented as
/// unlabelled `EXTRACTION A`/`EXTRACTION B` (FR-003) with no backend/model/provider
/// identifier reachable by the judge.
pub const PAIRWISE_JUDGE_PROMPT: &str = "You are comparing two structured extractions of the same source text, to judge which one better captures its content. Both extractions come from extraction systems under evaluation — neither is a trusted reference, and you are blind to which system produced which. Judge only what is in front of you.

PROMPT TYPE: {prompt_name}

SOURCE CHUNK:
{chunk}

EXTRACTION A:
{slot_a}

EXTRACTION B:
{slot_b}

Judge which extraction better captures the SOURCE CHUNK's content:
- Entities: which list better identifies the real-world entities discussed in the source, with accurate types, and without inventing entities not present in the source.
- Edges/relationships: which list better captures the relationships actually stated or clearly implied in the source, with correct source/target entities and relation meaning, without inventing relationships.
- Summaries: which set of summaries more accurately and completely reflects what the source says about each entity, without fabrication.

ITEM LISTS (use the appropriate field, matching PROMPT TYPE):
- extract_nodes.extract_text → response[\"extracted_entities\"] (list)
- extract_edges.* → response[\"edges\"] (list)
- extract_nodes.extract_summaries_batch → response[\"summaries\"] (list)

If one extraction is clearly more complete and accurate relative to the source, it wins. If both are roughly equivalent in quality (including both being empty, or both equally flawed), it is a tie. Do not prefer an extraction merely for being longer.

Respond with ONLY this JSON (no preamble, no commentary):
{
  \"winner\": \"A\" | \"B\" | \"tie\",
  \"rationale\": \"<one or two sentence justification>\"
}
";

/// FR-005: the pairwise judge's per-axis, per-chunk verdict — winner in `{A, B, tie}` plus
/// a brief rationale. One `judge_pairwise` call covers exactly one axis (mirroring how
/// reference-mode issues three separate `judge()` calls, one per axis), so this type never
/// tries to carry all three axes at once.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PairwiseWinner {
    A,
    B,
    #[serde(rename = "tie")]
    Tie,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairwiseVerdict {
    pub winner: PairwiseWinner,
    pub rationale: String,
}

/// A client capable of issuing one judge comparison. Trait-ified (rather than a bare
/// function) so tests can substitute a canned/mocked judge instead of hitting the live
/// Anthropic API — judge calls cost real money (Research: ~$5 over ~700 calls for the
/// prior harness's full matrix).
pub trait JudgeClient: Send + Sync {
    fn judge<'a>(
        &'a self,
        prompt_name: &'a str,
        reference: &'a Value,
        candidate: &'a Value,
    ) -> BoxFuture<'a, Result<JudgeVerdict, String>>;

    /// FR-002: blind pairwise comparison. `chunk_text` is the source episode body;
    /// `slot_a`/`slot_b` are the two extractions to compare for one axis, presented
    /// unlabelled per FR-003.
    fn judge_pairwise<'a>(
        &'a self,
        prompt_name: &'a str,
        chunk_text: &'a str,
        slot_a: &'a Value,
        slot_b: &'a Value,
    ) -> BoxFuture<'a, Result<PairwiseVerdict, String>>;
}

/// Minimal direct Anthropic Messages API client, independent of any `Extractor` backend
/// under test (Research Constraint 3: the judge isn't an `Extractor` in the ported source
/// either — it's a raw `messages.create` call).
pub struct AnthropicJudgeClient {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

/// A stalled/unresponsive judge call would otherwise block the whole harness run
/// indefinitely (there's no other progress signal or retry budget above this client);
/// bound it generously since a large reference/candidate payload can legitimately take
/// a while to judge.
const JUDGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

impl AnthropicJudgeClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            client: reqwest::Client::builder()
                .timeout(JUDGE_REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client with a fixed timeout is infallible to build"),
        }
    }
}

impl AnthropicJudgeClient {
    /// Shared request/retry/text-extraction path for both `judge()` and `judge_pairwise()`
    /// — the only difference between the two is the prompt text and the response shape
    /// parsed out of the returned text, so the HTTP + backoff scaffolding lives here once.
    async fn request_judge_text(&self, prompt: String) -> Result<String, String> {
        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": prompt}]
        });

        // Retry 429/529 with exponential backoff, mirroring `AnthropicExtractor`
        // (`crates/core/src/extractor.rs`) so a rate-limited judge call doesn't fail
        // the whole harness run.
        let mut attempt = 0u32;
        let resp: Value = loop {
            let http_resp = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("judge request failed: {e}"))?;

            let status = http_resp.status();
            if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < 3 {
                let delay = Duration::from_secs(1u64 << attempt);
                sleep(delay).await;
                attempt += 1;
                continue;
            }

            break http_resp
                .error_for_status()
                .map_err(|e| format!("judge request failed: {e}"))?
                .json()
                .await
                .map_err(|e| format!("judge response not JSON: {e}"))?;
        };

        resp["content"][0]["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "judge response missing content[0].text".to_string())
    }
}

impl JudgeClient for AnthropicJudgeClient {
    fn judge<'a>(
        &'a self,
        prompt_name: &'a str,
        reference: &'a Value,
        candidate: &'a Value,
    ) -> BoxFuture<'a, Result<JudgeVerdict, String>> {
        Box::pin(async move {
            let prompt = JUDGE_PROMPT
                .replace("{prompt_name}", prompt_name)
                .replace(
                    "{ref}",
                    &serde_json::to_string_pretty(reference).unwrap_or_default(),
                )
                .replace(
                    "{cand}",
                    &serde_json::to_string_pretty(candidate).unwrap_or_default(),
                );

            let text = self.request_judge_text(prompt).await?;
            let json_str = extract_json_block(&text);
            serde_json::from_str::<JudgeVerdict>(json_str)
                .map_err(|e| format!("judge response not valid JSON ({e}): {text}"))
        })
    }

    fn judge_pairwise<'a>(
        &'a self,
        prompt_name: &'a str,
        chunk_text: &'a str,
        slot_a: &'a Value,
        slot_b: &'a Value,
    ) -> BoxFuture<'a, Result<PairwiseVerdict, String>> {
        Box::pin(async move {
            let prompt = PAIRWISE_JUDGE_PROMPT
                .replace("{prompt_name}", prompt_name)
                .replace("{chunk}", chunk_text)
                .replace(
                    "{slot_a}",
                    &serde_json::to_string_pretty(slot_a).unwrap_or_default(),
                )
                .replace(
                    "{slot_b}",
                    &serde_json::to_string_pretty(slot_b).unwrap_or_default(),
                );

            let text = self.request_judge_text(prompt).await?;
            let json_str = extract_json_block(&text);
            serde_json::from_str::<PairwiseVerdict>(json_str)
                .map_err(|e| format!("pairwise judge response not valid JSON ({e}): {text}"))
        })
    }
}

/// Local copy of `lcg_core::extractor`'s private defensive-parse helper — small enough
/// (and specific enough to the judge's own response shape) that duplicating it here beats
/// exporting a private helper out of `extractor.rs` for one caller (Plan's key decision).
fn extract_json_block(s: &str) -> &str {
    if let Some(start) = s.find("```json") {
        let after = &s[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        // A malformed/refusal response can have a stray '}' before the first '{' (e.g. no
        // real JSON present at all), which would make `start > end` and panic on the slice
        // below. Fall through to returning the trimmed whole string in that case.
        if start <= end {
            return &s[start..=end];
        }
    }
    s.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_prompt_contains_expected_placeholders() {
        assert!(JUDGE_PROMPT.contains("{prompt_name}"));
        assert!(JUDGE_PROMPT.contains("{ref}"));
        assert!(JUDGE_PROMPT.contains("{cand}"));
        assert!(JUDGE_PROMPT.contains("\"matched\""));
        assert!(JUDGE_PROMPT.contains("\"unmatched_reference\""));
        assert!(JUDGE_PROMPT.contains("\"unmatched_candidate\""));
    }

    #[test]
    fn prf1_perfect_match() {
        let v = JudgeVerdict {
            matched: vec![(0, 0), (1, 1)],
            unmatched_reference: vec![],
            unmatched_candidate: vec![],
        };
        let (p, r, f1) = precision_recall_f1(&v);
        assert_eq!(p, 1.0);
        assert_eq!(r, 1.0);
        assert_eq!(f1, 1.0);
    }

    #[test]
    fn prf1_both_empty_defaults_to_perfect() {
        let v = JudgeVerdict::default();
        let (p, r, f1) = precision_recall_f1(&v);
        assert_eq!(p, 1.0);
        assert_eq!(r, 1.0);
        assert_eq!(f1, 1.0);
    }

    #[test]
    fn prf1_no_matches_is_zero_f1() {
        let v = JudgeVerdict {
            matched: vec![],
            unmatched_reference: vec![0, 1],
            unmatched_candidate: vec![0, 1],
        };
        let (p, r, f1) = precision_recall_f1(&v);
        assert_eq!(p, 0.0);
        assert_eq!(r, 0.0);
        assert_eq!(f1, 0.0);
    }

    #[test]
    fn prf1_partial_match() {
        // 1 matched, 1 extra candidate, 1 missed reference.
        let v = JudgeVerdict {
            matched: vec![(0, 0)],
            unmatched_reference: vec![1],
            unmatched_candidate: vec![1],
        };
        let (p, r, f1) = precision_recall_f1(&v);
        assert!((p - 0.5).abs() < 1e-9);
        assert!((r - 0.5).abs() < 1e-9);
        assert!((f1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn extract_json_block_strips_fences() {
        let s = "```json\n{\"matched\": []}\n```";
        assert_eq!(extract_json_block(s), "{\"matched\": []}");
    }

    #[test]
    fn extract_json_block_finds_braces_without_fences() {
        let s = "sure, here you go: {\"matched\": []} thanks";
        assert_eq!(extract_json_block(s), "{\"matched\": []}");
    }

    #[test]
    fn extract_json_block_does_not_panic_on_inverted_braces() {
        // A '}' before any '{' with no fences (e.g. a refusal or truncated response with
        // a stray brace) must not panic on the `s[start..=end]` slice — it should fall
        // through to returning the trimmed whole string instead.
        let s = "oops } no real json here {";
        assert_eq!(extract_json_block(s), s);
    }

    /// Canned judge client for tests — never makes a network call. Carries both a matching
    /// verdict and a pairwise verdict so a single double can stand in for either method the
    /// trait requires.
    struct StaticJudge {
        verdict: JudgeVerdict,
        pairwise_verdict: PairwiseVerdict,
    }

    impl StaticJudge {
        fn matching(verdict: JudgeVerdict) -> Self {
            Self {
                verdict,
                pairwise_verdict: PairwiseVerdict {
                    winner: PairwiseWinner::Tie,
                    rationale: String::new(),
                },
            }
        }

        fn pairwise(pairwise_verdict: PairwiseVerdict) -> Self {
            Self {
                verdict: JudgeVerdict::default(),
                pairwise_verdict,
            }
        }
    }

    impl JudgeClient for StaticJudge {
        fn judge<'a>(
            &'a self,
            _prompt_name: &'a str,
            _reference: &'a Value,
            _candidate: &'a Value,
        ) -> BoxFuture<'a, Result<JudgeVerdict, String>> {
            let v = self.verdict.clone();
            Box::pin(async move { Ok(v) })
        }

        fn judge_pairwise<'a>(
            &'a self,
            _prompt_name: &'a str,
            _chunk_text: &'a str,
            _slot_a: &'a Value,
            _slot_b: &'a Value,
        ) -> BoxFuture<'a, Result<PairwiseVerdict, String>> {
            let v = self.pairwise_verdict.clone();
            Box::pin(async move { Ok(v) })
        }
    }

    #[tokio::test]
    async fn judge_client_trait_is_mockable() {
        let judge = StaticJudge::matching(JudgeVerdict {
            matched: vec![(0, 0)],
            unmatched_reference: vec![],
            unmatched_candidate: vec![],
        });
        let verdict = judge
            .judge("extract_nodes.extract_text", &json!({}), &json!({}))
            .await
            .unwrap();
        assert_eq!(verdict.matched.len(), 1);
    }

    #[test]
    fn pairwise_judge_prompt_contains_expected_placeholders() {
        assert!(PAIRWISE_JUDGE_PROMPT.contains("{prompt_name}"));
        assert!(PAIRWISE_JUDGE_PROMPT.contains("{chunk}"));
        assert!(PAIRWISE_JUDGE_PROMPT.contains("{slot_a}"));
        assert!(PAIRWISE_JUDGE_PROMPT.contains("{slot_b}"));
        assert!(PAIRWISE_JUDGE_PROMPT.contains("\"winner\""));
        assert!(PAIRWISE_JUDGE_PROMPT.contains("\"rationale\""));
        // FR-003: no backend/model/provider identifier reachable by the judge — the prompt
        // template itself must not name a concrete backend.
        assert!(!PAIRWISE_JUDGE_PROMPT.to_lowercase().contains("anthropic"));
    }

    #[test]
    fn pairwise_verdict_parses_valid_json() {
        let v: PairwiseVerdict =
            serde_json::from_str(r#"{"winner": "A", "rationale": "A is more complete"}"#).unwrap();
        assert_eq!(v.winner, PairwiseWinner::A);
        assert_eq!(v.rationale, "A is more complete");
    }

    #[test]
    fn pairwise_verdict_accepts_lowercase_tie() {
        let v: PairwiseVerdict =
            serde_json::from_str(r#"{"winner": "tie", "rationale": "equivalent"}"#).unwrap();
        assert_eq!(v.winner, PairwiseWinner::Tie);
    }

    #[test]
    fn pairwise_verdict_rejects_malformed_json() {
        let err = serde_json::from_str::<PairwiseVerdict>("not json at all").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[tokio::test]
    async fn judge_pairwise_trait_is_mockable() {
        let judge = StaticJudge::pairwise(PairwiseVerdict {
            winner: PairwiseWinner::B,
            rationale: "B captures more".to_string(),
        });
        let verdict = judge
            .judge_pairwise(
                "pairwise.extract_nodes.extract_text",
                "some source chunk text",
                &json!({"extracted_entities": []}),
                &json!({"extracted_entities": []}),
            )
            .await
            .unwrap();
        assert_eq!(verdict.winner, PairwiseWinner::B);
    }
}
