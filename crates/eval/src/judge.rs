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

            let text = resp["content"][0]["text"]
                .as_str()
                .ok_or_else(|| "judge response missing content[0].text".to_string())?;
            let json_str = extract_json_block(text);
            serde_json::from_str::<JudgeVerdict>(json_str)
                .map_err(|e| format!("judge response not valid JSON ({e}): {text}"))
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

    /// Canned judge client for tests — never makes a network call.
    struct StaticJudge(JudgeVerdict);

    impl JudgeClient for StaticJudge {
        fn judge<'a>(
            &'a self,
            _prompt_name: &'a str,
            _reference: &'a Value,
            _candidate: &'a Value,
        ) -> BoxFuture<'a, Result<JudgeVerdict, String>> {
            let v = self.0.clone();
            Box::pin(async move { Ok(v) })
        }
    }

    #[tokio::test]
    async fn judge_client_trait_is_mockable() {
        let judge = StaticJudge(JudgeVerdict {
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
}
