//! LLM-as-judge scoring (FR-005/FR-008). The judge prompt and precision/recall/F1
//! derivation are ported verbatim from the prior Python harness
//! (`verveguy/liminis-framework@main:eval/extraction-quality/src/eval_extraction/judge.py`,
//! reproduced in the Research stage for issue #228) — not re-derived from intuition.
//!
//! The judge is intentionally *not* an `Extractor` implementation: it asks a free-form
//! semantic-equivalence question via a raw Anthropic Messages API call, independent of
//! whichever backends are under test (including when comparing two non-Anthropic
//! candidates against each other).

use std::sync::atomic::{AtomicUsize, Ordering};
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
///
/// `Deserialize` is hand-written rather than derived: see [`RawJudgeVerdict`].
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct JudgeVerdict {
    pub matched: Vec<(usize, usize)>,
    pub unmatched_reference: Vec<usize>,
    pub unmatched_candidate: Vec<usize>,
}

/// The verdict exactly as the judge writes it, before normalisation.
///
/// The prompt asks for `matched` to hold pairs of indices, but the judge routinely uses a
/// *sentinel* in one slot to say "this item matched nothing" — `[5, null]` and `[9, -1]`
/// were both observed during the #248 run — while also listing that index in
/// `unmatched_reference`. Against `Vec<(usize, usize)>` serde rejects the whole response
/// ("invalid type: null, expected usize"), so a verdict that is *complete and correct*
/// is discarded and the chunk loses that axis.
///
/// The sentinel encoding is redundant with the unmatched lists in every observed case, so
/// normalising costs nothing: drop the sentinel pairs, and union their real index into the
/// matching unmatched list in case the judge only reported it in one place. Dropping
/// matters for accuracy as well as parsing — a retained `[5, null]` would inflate
/// `matched.len()`, and precision and recall are both computed from it.
///
/// `matched` is deliberately REQUIRED while the two unmatched lists default.
///
/// `JudgeCache` stores matching and pairwise verdicts in one file discriminated by
/// `#[serde(untagged)]` (`judge_cache.rs`), which relies on the two shapes being disjoint.
/// Defaulting every field here would make *any* JSON object — including a
/// `{"winner", "rationale"}` pairwise verdict — parse as an all-empty `JudgeVerdict`, so
/// every cached pairwise verdict would silently fail to reload and be re-purchased on the
/// next run. Requiring `matched`, which `PairwiseVerdict` does not have, keeps them
/// disjoint while still tolerating a judge that omits an empty unmatched list.
#[derive(Deserialize)]
struct RawJudgeVerdict {
    matched: Vec<Vec<Option<i64>>>,
    #[serde(default)]
    unmatched_reference: Vec<i64>,
    #[serde(default)]
    unmatched_candidate: Vec<i64>,
}

/// A `matched`-pair slot holds a usable index only when present and non-negative.
/// `None` here means the judge wrote a sentinel (`null` or a negative number).
fn valid_index(slot: Option<i64>) -> Option<usize> {
    match slot {
        Some(i) if i >= 0 => Some(i as usize),
        _ => None,
    }
}

/// A bare entry in `unmatched_reference`/`unmatched_candidate`, which are plain lists and
/// never contain `null` slots.
///
/// A negative value here is **dropped**, not an error. That is a deliberate choice with a
/// real cost: before this type existed the field was `Vec<usize>` and a stray negative
/// failed the whole response loudly. Dropping trades a loud total loss of the axis for a
/// quiet partial one — the index vanishes and `ref_total`/`cand_total` shrink by one.
///
/// It is chosen because a negative index has no recoverable meaning (unlike a sentinel
/// inside `matched`, where the paired slot still names a real item), so failing the entire
/// verdict over one nonsense entry discards good data to no benefit. Pinned by a test so
/// the behaviour is a decision rather than an accident.
fn valid_bare_index(i: i64) -> Option<usize> {
    (i >= 0).then_some(i as usize)
}

fn push_unique(v: &mut Vec<usize>, x: usize) {
    if !v.contains(&x) {
        v.push(x);
    }
}

impl<'de> Deserialize<'de> for JudgeVerdict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawJudgeVerdict::deserialize(d)?;

        let mut unmatched_reference: Vec<usize> = Vec::new();
        for i in raw.unmatched_reference {
            if let Some(i) = valid_bare_index(i) {
                push_unique(&mut unmatched_reference, i);
            }
        }
        let mut unmatched_candidate: Vec<usize> = Vec::new();
        for i in raw.unmatched_candidate {
            if let Some(i) = valid_bare_index(i) {
                push_unique(&mut unmatched_candidate, i);
            }
        }

        let mut matched = Vec::with_capacity(raw.matched.len());
        for pair in raw.matched {
            // Arity is deliberately not assumed. `minItems`/`maxItems` are rejected by the
            // API's schema subset, so nothing upstream guarantees exactly two elements, and
            // a serde tuple would hard-fail the whole response on a 1- or 3-element array.
            let a = pair.first().copied().flatten();
            let b = pair.get(1).copied().flatten();
            match (valid_index(a), valid_index(b)) {
                (Some(r), Some(c)) => matched.push((r, c)),
                // A sentinel in one slot: the other slot names a genuinely unmatched item.
                (Some(r), None) => push_unique(&mut unmatched_reference, r),
                (None, Some(c)) => push_unique(&mut unmatched_candidate, c),
                (None, None) => {}
            }
        }

        // A real match is authoritative over any claim that the same index is unmatched,
        // however that claim arrived — a sentinel pair like `[[0,0],[0,null]]`, or the judge
        // simply listing 0 in `unmatched_reference` while also matching it. Both were left
        // standing before, so the index counted once via `matched.len()` and again via the
        // unmatched list, inflating the denominator in `precision_recall_f1` and
        // understating recall. That is precisely the inflation this type exists to prevent,
        // so it must be reconciled after `matched` is final rather than during collection.
        unmatched_reference.retain(|r| !matched.iter().any(|(mr, _)| mr == r));
        unmatched_candidate.retain(|c| !matched.iter().any(|(_, mc)| mc == c));

        Ok(JudgeVerdict {
            matched,
            unmatched_reference,
            unmatched_candidate,
        })
    }
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
    #[serde(alias = "a")]
    A,
    #[serde(alias = "b")]
    B,
    #[serde(rename = "tie", alias = "Tie", alias = "TIE")]
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
    /// Consecutive failures, reset by any success. See `CONSECUTIVE_FAILURE_LIMIT`.
    breaker: CircuitBreaker,
}

/// Response schema for a matching verdict, enforced by the API via
/// `output_config.format` rather than merely requested in prose.
///
/// The prompt already says "respond with ONLY this JSON (no preamble, no commentary)" and
/// the judge ignored it three separate ways during the #248 run: prose around the JSON, a
/// second corrected object after reconsidering, and `null`/`-1` sentinels inside `matched`.
///
/// What the schema does and does not buy, probed against the live API on 2026-07-28 rather
/// than assumed — the subset accepted here is narrower than JSON Schema:
///
/// | failure mode              | prevented? |
/// |---------------------------|------------|
/// | prose around the JSON     | yes        |
/// | a second corrected object | yes        |
/// | `null` sentinel           | yes, via `"type": "integer"` |
/// | `-1` sentinel             | **no** — `minimum` is rejected: "For 'integer' type, property 'minimum' is not supported" |
/// | inner array not a pair    | **no** — `minItems`/`maxItems` other than 0 or 1 are rejected |
///
/// So the sentinel normalisation in [`RawJudgeVerdict`] is load-bearing, not a courtesy
/// backstop: it is the only thing handling `-1`, and the only thing tolerating an inner
/// array whose length is not two. Do not delete it on the grounds that "the schema
/// guarantees the shape" — it guarantees less than it looks like it does.
fn matching_verdict_schema() -> Value {
    let index = json!({"type": "integer"});
    json!({
        "type": "object",
        "properties": {
            "matched": {
                "type": "array",
                "items": {"type": "array", "items": index}
            },
            "unmatched_reference": {"type": "array", "items": index},
            "unmatched_candidate": {"type": "array", "items": index},
        },
        "required": ["matched", "unmatched_reference", "unmatched_candidate"],
        "additionalProperties": false
    })
}

/// Response schema for a blind pairwise verdict (#269).
fn pairwise_verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "winner": {"type": "string", "enum": ["A", "B", "tie"]},
            "rationale": {"type": "string"},
        },
        "required": ["winner", "rationale"],
        "additionalProperties": false
    })
}

/// How many judge calls may fail back-to-back before the client stops making requests.
///
/// Judge failures are deliberately non-fatal per call, because one bad response should not
/// discard hours of scoring. That is right for transient faults and wrong for systemic
/// ones: an invalid model id, an exhausted credit balance, or a revoked key fails *every*
/// call, and without a breaker a ~6000-call scoring phase would grind through all of them
/// and report a clean-looking run with 6000 judge errors. A run this size will not
/// legitimately produce ten failures in a row.
pub const CONSECUTIVE_FAILURE_LIMIT: usize = 10;

/// The consecutive-failure breaker, split out from the HTTP path so its state transitions
/// are testable without a network round trip or a mock transport: every rule that matters
/// (closed below the limit, opens at it, any success resets) lives here and nowhere else.
#[derive(Debug)]
pub struct CircuitBreaker {
    consecutive: AtomicUsize,
    limit: usize,
}

impl CircuitBreaker {
    pub fn new(limit: usize) -> Self {
        Self {
            consecutive: AtomicUsize::new(0),
            limit,
        }
    }

    /// `Err` once the breaker is open — the caller must not issue the request.
    pub fn check(&self) -> Result<(), String> {
        let failures = self.consecutive.load(Ordering::Relaxed);
        if failures >= self.limit {
            return Err(format!(
                "judge circuit breaker open after {failures} consecutive failures — \
                 not issuing further requests. Fix the underlying cause and re-run; \
                 verdicts already cached will be re-served for free."
            ));
        }
        Ok(())
    }

    /// Resets on success rather than decaying: an isolated transient fault in a
    /// multi-thousand-call run must not accumulate toward the limit over hours.
    pub fn record<T, E>(&self, result: &Result<T, E>) {
        match result {
            Ok(_) => self.consecutive.store(0, Ordering::Relaxed),
            Err(_) => {
                self.consecutive.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn consecutive_failures(&self) -> usize {
        self.consecutive.load(Ordering::Relaxed)
    }
}

/// A stalled/unresponsive judge call would otherwise block the whole harness run
/// indefinitely (there's no other progress signal or retry budget above this client);
/// bound it generously since a large reference/candidate payload can legitimately take
/// a while to judge.
const JUDGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Cap on the raw-body fallback when the API's error JSON cannot be parsed. Bytes, not
/// chars — enough to identify the failure without dumping an HTML error page into stderr.
const ERROR_BODY_FALLBACK_BYTES: usize = 500;

impl AnthropicJudgeClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            client: reqwest::Client::builder()
                .timeout(JUDGE_REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client with a fixed timeout is infallible to build"),
            breaker: CircuitBreaker::new(CONSECUTIVE_FAILURE_LIMIT),
        }
    }
}

impl AnthropicJudgeClient {
    /// Shared request/retry/text-extraction path for both `judge()` and `judge_pairwise()`
    /// — the only difference between the two is the prompt text and the response shape
    /// parsed out of the returned text, so the HTTP + backoff scaffolding lives here once.
    async fn request_judge_text(&self, prompt: String, schema: Value) -> Result<String, String> {
        self.breaker.check()?;
        let result = self.request_judge_text_inner(prompt, Some(schema)).await;
        self.breaker.record(&result);
        result
    }

    /// `schema: None` issues the same call without `output_config.format`. That is the
    /// fallback for a grammar-compilation timeout, and the reason `extract_json_block` and
    /// the sentinel normalisation are still worth keeping: they are what parses the
    /// unconstrained response.
    fn request_judge_text_inner(
        &self,
        prompt: String,
        schema: Option<Value>,
    ) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let mut body = json!({
                "model": self.model,
                "max_tokens": 4096,
                "messages": [{"role": "user", "content": prompt.clone()}],
                // Explicit, not inherited from the model's default. Sonnet 4.6 runs
                // thinking-off when `thinking` is omitted, but Sonnet 5 runs adaptive thinking
                // by default — so `--judge-model claude-sonnet-5` would silently switch it on
                // across every call of a ~6000-call scoring phase, changing cost, latency and
                // verdicts at once, with nothing in the output saying so.
                //
                // Disabled rather than adaptive on purpose. Judging is index enumeration, which
                // is exactly where thinking measured *worse* on this corpus:
                // docs/history/extraction-eval-2026-04.md has qwen3.6-27b at 0.894 entity F1
                // without thinking and 0.772 with. A judge is a measuring instrument — cheap,
                // fast and repeatable beats deliberative.
                "thinking": {"type": "disabled"}
            });
            if let Some(schema) = &schema {
                body["output_config"] =
                    json!({"format": {"type": "json_schema", "schema": schema}});
            }

            // Retry 429/529 with exponential backoff, mirroring `AnthropicExtractor`
            // (`crates/core/src/extractor.rs`) so a rate-limited judge call doesn't fail
            // the whole harness run.
            let mut attempt = 0u32;
            let resp: Value = loop {
                let sent = self
                    .client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await;

                // Transport errors are retried on the same ladder as 429/529. They were not
                // retried at all before, so a single dropped connection propagated straight
                // out — which is precisely how the #248 scoring run died 99 minutes and 1379
                // pairwise verdicts in. Over ~6000 sequential calls a transient network fault
                // is close to certain; treating it as fatal made the run's success depend on
                // an unbroken hour of perfect connectivity.
                let http_resp = match sent {
                    Ok(r) => r,
                    Err(_) if attempt < 3 => {
                        sleep(Duration::from_secs(1u64 << attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(e) => return Err(format!("judge request failed after retries: {e}")),
                };

                let status = http_resp.status();
                if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < 3 {
                    let delay = Duration::from_secs(1u64 << attempt);
                    sleep(delay).await;
                    attempt += 1;
                    continue;
                }

                // The API puts the *reason* in the response body, and `error_for_status()`
                // discards it — a 400 for an invalid model, an oversized prompt, or an
                // exhausted credit balance are indistinguishable without it. During the #248
                // run that left every judge call reporting only "HTTP status client error
                // (400 Bad Request)", which is true and useless.
                if !status.is_success() {
                    let body = http_resp.text().await.unwrap_or_default();

                    // "Grammar compilation timed out" is the API failing to compile the
                    // constrained-output grammar, not a problem with the request's content — a
                    // failure mode that only exists because we ask for output_config.format.
                    // Retrying the same constrained request would time out again, so drop the
                    // constraint and let extract_json_block plus the sentinel normalisation
                    // handle the response, which is what they existed for before the schema.
                    // Losing the constraint on one call is strictly better than losing the
                    // call.
                    if body.contains("Grammar compilation") && schema.is_some() {
                        eprintln!(
                            "lcg-eval: judge grammar compilation timed out — retrying this call \
                         without the schema constraint"
                        );
                        return self.request_judge_text_inner(prompt, None).await;
                    }

                    let detail = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                        .unwrap_or_else(|| {
                            // Truncate by BYTES at a char boundary: `chars().take(500)` can
                            // yield ~2 KB of UTF-8 and contradict the documented cap.
                            let mut end = body.len().min(ERROR_BODY_FALLBACK_BYTES);
                            while end > 0 && !body.is_char_boundary(end) {
                                end -= 1;
                            }
                            body[..end].to_owned()
                        });
                    return Err(format!("judge request failed ({status}): {detail}"));
                }

                break http_resp
                    .json()
                    .await
                    .map_err(|e| format!("judge response not JSON: {e}"))?;
            };

            resp["content"][0]["text"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "judge response missing content[0].text".to_string())
        })
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

            let text = self
                .request_judge_text(prompt, matching_verdict_schema())
                .await?;
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

            let text = self
                .request_judge_text(prompt, pairwise_verdict_schema())
                .await?;
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
    // A fence narrows *where* to look; it never decides *what* to return. Returning fenced
    // content verbatim reintroduces the bug this function exists to fix whenever the judge
    // wraps its whole reply -- first verdict, reconsideration prose, corrected verdict --
    // in a single fence, which is at least as likely as the two-fence form actually
    // observed. So the object scan runs over the fenced content too.
    if let Some(fenced) = last_fenced_content(s) {
        if let Some(obj) = last_top_level_json_object(fenced) {
            return obj;
        }
    }
    last_top_level_json_object(s).unwrap_or_else(|| s.trim())
}

/// Content of the **last** fenced block in `s`, with an optional `json` language tag
/// stripped, or `None` when there is no complete fence.
///
/// Fences are collected as a flat list of ```` ``` ```` positions and the final pair is
/// taken, because openers and closers are indistinguishable by search: an earlier
/// implementation used `rfind("```")` to locate the *opening* fence, but for any
/// well-formed block that finds the *closing* marker, leaving nothing after it to match
/// and rendering the branch dead. (Caught in review on #271; it was harmless only because
/// the fallback scan recovers the right object anyway.)
fn last_fenced_content(s: &str) -> Option<&str> {
    let fences: Vec<usize> = s.match_indices("```").map(|(i, _)| i).collect();
    if fences.len() < 2 {
        return None;
    }
    let open = fences[fences.len() - 2];
    let close = fences[fences.len() - 1];
    let inner = &s[open + 3..close];
    Some(inner.strip_prefix("json").unwrap_or(inner).trim())
}

/// Returns the **last** top-level `{...}` span in `s` that parses as JSON, or `None`.
///
/// The judge is a reasoning model, and a reasoning model that spots its own mistake
/// answers twice: it emits a verdict, reconsiders in prose, then emits a corrected
/// verdict. The correction is the answer we want — it is the one the model stands
/// behind — so this scans forward and keeps the last object rather than the first.
///
/// The previous `s.find('{') ..= s.rfind('}')` span was first-brace-to-last-brace, which
/// on a self-corrected response covers *both* objects plus the prose between them.
/// `serde_json` parses the leading object and then reports "trailing characters", the
/// error propagates out of `score_candidate`, and a multi-hour scoring run dies on a
/// response that was not merely valid but *more* correct than the usual one. That killed
/// the #248 full-corpus run 17 calls into ~1340.
///
/// Advancing past each parsed object (rather than to the next `{`) is what keeps this
/// top-level: descending into a nested object would make the innermost trailing object
/// win, so `{"a": {"b": 1}}` would yield `{"b": 1}`.
fn last_top_level_json_object(s: &str) -> Option<&str> {
    let mut best = None;
    let mut idx = 0;
    while let Some(rel) = s[idx..].find('{') {
        let start = idx + rel;
        let mut stream =
            serde_json::Deserializer::from_str(&s[start..]).into_iter::<serde_json::Value>();
        match stream.next() {
            Some(Ok(_)) => {
                let consumed = stream.byte_offset();
                best = Some(&s[start..start + consumed]);
                idx = start + consumed;
            }
            _ => idx = start + 1,
        }
    }
    best
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

    /// The schema must stay inside the subset the API actually accepts. Both `minimum` and
    /// `minItems` were rejected when probed live, so re-adding either would 400 *every*
    /// judge call — a total run failure, not a degradation.
    #[test]
    fn matching_schema_stays_within_the_supported_subset() {
        let s = matching_verdict_schema();
        let rendered = s.to_string();
        assert!(
            !rendered.contains("minimum"),
            "API rejects 'minimum' on integer: every call would 400"
        );
        assert!(
            !rendered.contains("minItems") && !rendered.contains("maxItems"),
            "API rejects minItems/maxItems other than 0 or 1: every call would 400"
        );
        // `type: integer` is the one constraint that does hold, and it is what excludes the
        // `null` sentinel. `-1` is left to the normaliser.
        assert_eq!(
            s["properties"]["matched"]["items"]["items"]["type"],
            "integer"
        );
        for field in ["unmatched_reference", "unmatched_candidate"] {
            assert_eq!(s["properties"][field]["items"]["type"], "integer");
        }
        assert_eq!(s["additionalProperties"], false);
    }

    /// The pairwise schema's enum must match `PairwiseWinner`'s serde representation, or
    /// every constrained response would parse-fail on arrival.
    #[test]
    fn pairwise_schema_enum_matches_the_winner_type() {
        let s = pairwise_verdict_schema();
        let variants = s["properties"]["winner"]["enum"]
            .as_array()
            .unwrap()
            .clone();
        for v in variants {
            let as_json = format!(r#"{{"winner": {v}, "rationale": "x"}}"#);
            serde_json::from_str::<PairwiseVerdict>(&as_json).unwrap_or_else(|e| {
                panic!("schema allows {v} but PairwiseVerdict rejects it: {e}")
            });
        }
    }

    /// CodeRabbit's finding on #276: a sentinel pair whose real index ALSO appears in a
    /// genuine match. `[[0,0],[0,null]]` used to yield both `matched=[(0,0)]` and
    /// `unmatched_reference=[0]`, counting index 0 twice toward `ref_total` — the very
    /// inflation this type exists to prevent, arriving by a path the original fix missed.
    #[test]
    fn a_real_match_wins_over_a_sentinel_claiming_the_same_index() {
        let v: JudgeVerdict = serde_json::from_str(
            r#"{"matched": [[0,0],[0,null]], "unmatched_reference": [], "unmatched_candidate": []}"#,
        )
        .unwrap();
        assert_eq!(v.matched, vec![(0, 0)]);
        assert!(
            v.unmatched_reference.is_empty(),
            "0 is matched; it cannot also be unmatched"
        );
        let (p, r, _) = precision_recall_f1(&v);
        assert_eq!((p, r), (1.0, 1.0), "denominators must not be inflated");
    }

    /// The Claude reviewer's variant of the same gap: no sentinel involved — the judge
    /// simply lists an index as unmatched while also matching it. Same double count.
    #[test]
    fn a_real_match_wins_over_a_declared_unmatched_entry() {
        let v: JudgeVerdict = serde_json::from_str(
            r#"{"matched": [[5,3]], "unmatched_reference": [5], "unmatched_candidate": [3]}"#,
        )
        .unwrap();
        assert_eq!(v.matched, vec![(5, 3)]);
        assert!(v.unmatched_reference.is_empty());
        assert!(v.unmatched_candidate.is_empty());
        let (p, r, _) = precision_recall_f1(&v);
        assert_eq!((p, r), (1.0, 1.0));
    }

    /// Reconciliation must not over-reach: an index unmatched on one axis can legitimately
    /// be matched on the other, since the two lists index different item lists.
    #[test]
    fn reconciliation_is_per_axis_not_global() {
        let v: JudgeVerdict = serde_json::from_str(
            r#"{"matched": [[0,1]], "unmatched_reference": [1], "unmatched_candidate": [0]}"#,
        )
        .unwrap();
        assert_eq!(v.matched, vec![(0, 1)]);
        assert_eq!(
            v.unmatched_reference,
            vec![1],
            "reference 1 is genuinely unmatched"
        );
        assert_eq!(
            v.unmatched_candidate,
            vec![0],
            "candidate 0 is genuinely unmatched"
        );
    }

    /// Pins the documented decision that a bare negative in an unmatched list is dropped
    /// rather than failing the response. Before this type it errored loudly; dropping
    /// trades a loud total loss of the axis for a quiet partial one, which is a choice and
    /// should read as one.
    #[test]
    fn bare_negative_indices_in_unmatched_lists_are_dropped_not_fatal() {
        let v: JudgeVerdict = serde_json::from_str(
            r#"{"matched": [[0,0]], "unmatched_reference": [-1, 2], "unmatched_candidate": [-7]}"#,
        )
        .expect("a nonsense index must not discard the whole verdict");
        assert_eq!(v.matched, vec![(0, 0)]);
        assert_eq!(v.unmatched_reference, vec![2], "-1 dropped, 2 kept");
        assert!(v.unmatched_candidate.is_empty());
    }

    /// Duplicates within a declared list must not inflate the denominator either.
    #[test]
    fn duplicate_declared_unmatched_indices_are_collapsed() {
        let v: JudgeVerdict = serde_json::from_str(
            r#"{"matched": [], "unmatched_reference": [3, 3, 3], "unmatched_candidate": []}"#,
        )
        .unwrap();
        assert_eq!(v.unmatched_reference, vec![3]);
    }

    /// The schema cannot constrain inner-array arity, so a pair that is not a pair must
    /// degrade rather than fail the whole response — a serde tuple would hard-error here.
    #[test]
    fn judge_verdict_tolerates_non_pair_inner_arrays() {
        let s = r#"{"matched": [[0, 0], [1], [2, 2, 9], []],
                    "unmatched_reference": [], "unmatched_candidate": []}"#;
        let v: JudgeVerdict =
            serde_json::from_str(s).expect("odd arity must not fail the response");
        assert_eq!(v.matched, vec![(0, 0), (2, 2)], "extra elements ignored");
        assert_eq!(
            v.unmatched_reference,
            vec![1],
            "a lone index is an unmatched reference"
        );
    }

    /// Verbatim from the #248 run: a `null` sentinel in the candidate slot, with the index
    /// also correctly listed in `unmatched_reference`. The old derive rejected the whole
    /// response and the chunk lost its edge score.
    #[test]
    fn judge_verdict_drops_null_sentinel_pairs() {
        let s = r#"{
          "matched": [[0, 0], [1, 1], [5, null], [6, 9]],
          "unmatched_reference": [5],
          "unmatched_candidate": [5]
        }"#;
        let v: JudgeVerdict = serde_json::from_str(s).expect("null sentinels must parse");
        assert_eq!(v.matched, vec![(0, 0), (1, 1), (6, 9)]);
        assert_eq!(
            v.unmatched_reference,
            vec![5],
            "already listed — must not duplicate"
        );
        assert_eq!(v.unmatched_candidate, vec![5]);
    }

    /// The other observed sentinel: `-1` rather than `null`, three times in one response.
    #[test]
    fn judge_verdict_drops_negative_sentinel_pairs() {
        let s = r#"{
          "matched": [[0, 0], [9, -1], [10, -1], [11, -1]],
          "unmatched_reference": [9, 10, 11],
          "unmatched_candidate": []
        }"#;
        let v: JudgeVerdict = serde_json::from_str(s).expect("-1 sentinels must parse");
        assert_eq!(v.matched, vec![(0, 0)]);
        assert_eq!(v.unmatched_reference, vec![9, 10, 11]);
    }

    /// If the judge uses a sentinel but *forgets* the unmatched list, the index must still
    /// be accounted for — otherwise the item silently vanishes from both sides and recall
    /// is overstated.
    #[test]
    fn judge_verdict_recovers_a_sentinel_index_missing_from_the_unmatched_list() {
        let s = r#"{"matched": [[3, null], [null, 7]],
                    "unmatched_reference": [], "unmatched_candidate": []}"#;
        let v: JudgeVerdict = serde_json::from_str(s).unwrap();
        assert!(v.matched.is_empty());
        assert_eq!(v.unmatched_reference, vec![3]);
        assert_eq!(v.unmatched_candidate, vec![7]);
    }

    /// Sentinels must not be counted as matches: precision and recall are both derived
    /// from `matched.len()`, so retaining `[5, null]` would inflate both.
    #[test]
    fn sentinel_pairs_do_not_inflate_precision_and_recall() {
        let v: JudgeVerdict = serde_json::from_str(
            r#"{"matched": [[0,0],[1,null]], "unmatched_reference": [1], "unmatched_candidate": []}"#,
        )
        .unwrap();
        let (p, r, _) = precision_recall_f1(&v);
        assert_eq!(p, 1.0, "one real match, one candidate item");
        assert_eq!(r, 0.5, "one of two reference items matched");
    }

    /// `JudgeCache` discriminates matching from pairwise verdicts by `#[serde(untagged)]`,
    /// which needs the two shapes to stay disjoint. Making every field default would let a
    /// pairwise verdict parse as an all-empty `JudgeVerdict`, so cached pairwise verdicts
    /// would never reload and would be re-purchased on every run. Pinned here because the
    /// coupling lives in another module and is invisible from this one.
    #[test]
    fn judge_verdict_never_absorbs_a_pairwise_verdict() {
        let pairwise = r#"{"winner": "A", "rationale": "A is more complete"}"#;
        assert!(
            serde_json::from_str::<JudgeVerdict>(pairwise).is_err(),
            "a pairwise verdict must NOT deserialize as a JudgeVerdict"
        );
        assert!(
            serde_json::from_str::<JudgeVerdict>("{}").is_err(),
            "an empty object must not parse as an empty verdict"
        );
    }

    /// A well-formed verdict must round-trip unchanged through the hand-written impl.
    #[test]
    fn judge_verdict_without_sentinels_is_unchanged() {
        let s =
            r#"{"matched": [[0,1],[2,3]], "unmatched_reference": [4], "unmatched_candidate": [5]}"#;
        let v: JudgeVerdict = serde_json::from_str(s).unwrap();
        assert_eq!(v.matched, vec![(0, 1), (2, 3)]);
        assert_eq!(v.unmatched_reference, vec![4]);
        assert_eq!(v.unmatched_candidate, vec![5]);
        assert_eq!(
            serde_json::from_str::<JudgeVerdict>(&serde_json::to_string(&v).unwrap()).unwrap(),
            v,
            "Serialize/Deserialize must stay symmetric — the judge cache round-trips this"
        );
    }

    /// The exact response that killed the #248 full-corpus run: the judge answered,
    /// reconsidered in prose, and answered again with a correction. The old
    /// first-brace-to-last-brace span covered both objects plus the prose, so serde
    /// reported "trailing characters at line 7 column 1" and the run aborted.
    #[test]
    fn extract_json_block_takes_the_correction_when_the_judge_answers_twice() {
        let s = r#"{
  "matched": [[0, 0], [1, 1], [8, 1]],
  "unmatched_reference": [],
  "unmatched_candidate": [4]
}

Wait, let me reconsider. Reference index 1 is "birthplace of Alan Shepard" and
reference index 8 is also "Birthplace of Alan Shepard". Let me re-examine.

{
  "matched": [[0, 0], [1, 2], [8, 1]],
  "unmatched_reference": [],
  "unmatched_candidate": []
}"#;
        let verdict: JudgeVerdict = serde_json::from_str(extract_json_block(s))
            .expect("the corrected block must parse on its own");
        assert_eq!(verdict.matched, vec![(0, 0), (1, 2), (8, 1)]);
        assert!(
            verdict.unmatched_candidate.is_empty(),
            "must take the second verdict, not the first"
        );
    }

    /// Finding #1 from #271's review: the fenced variant of the observed failure. If the
    /// judge puts its whole reply -- both verdicts and the reconsideration prose -- inside
    /// ONE fence, returning fenced content verbatim fails to parse exactly as the unfenced
    /// case did. The scan must run over fenced content too.
    #[test]
    fn extract_json_block_handles_a_self_correction_inside_a_single_fence() {
        let s = "```json\n{\"matched\": [[0, 0]], \"unmatched_reference\": [], \
                 \"unmatched_candidate\": [4]}\n\nWait, that mismatches the birthplace. \
                 Correcting:\n\n{\"matched\": [[0, 1]], \"unmatched_reference\": [], \
                 \"unmatched_candidate\": []}\n```";
        let verdict: JudgeVerdict = serde_json::from_str(extract_json_block(s))
            .expect("must recover the corrected object from inside one fence");
        assert_eq!(verdict.matched, vec![(0, 1)]);
        assert!(verdict.unmatched_candidate.is_empty());
    }

    /// The bare-fence (no language tag) path, which review found was dead code: `rfind` on
    /// "```" locates the *closing* marker, so the old branch never fired.
    #[test]
    fn extract_json_block_reads_a_bare_fence() {
        let s = "here:\n```\n{\"matched\": [[1, 1]], \"unmatched_reference\": [], \
                 \"unmatched_candidate\": []}\n```";
        let verdict: JudgeVerdict =
            serde_json::from_str(extract_json_block(s)).expect("bare fences must parse");
        assert_eq!(verdict.matched, vec![(1, 1)]);
    }

    #[test]
    fn extract_json_block_prefers_the_last_fenced_block() {
        let s = "```json\n{\"matched\": [[0, 0]]}\n```\non reflection:\n```json\n{\"matched\": []}\n```";
        assert_eq!(extract_json_block(s), "{\"matched\": []}");
    }

    /// Advancing past each parsed object keeps the scan at the top level. Without that,
    /// the innermost trailing object would win and this would yield `{"b": 1}`.
    #[test]
    fn extract_json_block_does_not_descend_into_nested_objects() {
        let s = "{\"a\": {\"b\": 1}}";
        assert_eq!(extract_json_block(s), s);
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

    /// The breaker must short-circuit *before* the request is built, so this makes no
    /// network call despite the obviously invalid key — that is the whole point of it.
    #[tokio::test]
    async fn judge_circuit_breaker_stops_issuing_requests_after_repeated_failures() {
        let client = AnthropicJudgeClient::new("not-a-real-key".into(), "some-model".into());
        for _ in 0..CONSECUTIVE_FAILURE_LIMIT {
            client.breaker.record::<(), ()>(&Err(()));
        }

        let err = client
            .request_judge_text("anything".into(), matching_verdict_schema())
            .await
            .expect_err("breaker must be open");
        assert!(
            err.contains("circuit breaker open"),
            "expected a breaker message, got: {err}"
        );
    }

    #[test]
    fn circuit_breaker_starts_closed_and_stays_closed_below_the_limit() {
        let b = CircuitBreaker::new(3);
        assert!(b.check().is_ok(), "a fresh breaker must be closed");
        b.record::<(), ()>(&Err(()));
        b.record::<(), ()>(&Err(()));
        assert_eq!(b.consecutive_failures(), 2);
        assert!(b.check().is_ok(), "must stay closed one below the limit");
    }

    #[test]
    fn circuit_breaker_opens_exactly_at_the_limit() {
        let b = CircuitBreaker::new(3);
        for _ in 0..3 {
            b.record::<(), ()>(&Err(()));
        }
        let err = b.check().expect_err("must be open at the limit");
        assert!(err.contains("3 consecutive failures"), "got: {err}");
    }

    /// The property that makes a long run survivable: an isolated failure must not
    /// accumulate toward the limit over thousands of calls.
    #[test]
    fn circuit_breaker_resets_on_any_success() {
        let b = CircuitBreaker::new(3);
        b.record::<(), ()>(&Err(()));
        b.record::<(), ()>(&Err(()));
        b.record::<(), ()>(&Ok(()));
        assert_eq!(b.consecutive_failures(), 0);
        b.record::<(), ()>(&Err(()));
        b.record::<(), ()>(&Err(()));
        assert!(
            b.check().is_ok(),
            "two failures after a success must not trip a limit of 3"
        );
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
    fn pairwise_verdict_accepts_case_variant_winners() {
        // The prompt asks for "A" | "B" | "tie", but a judge model's reply casing isn't
        // guaranteed — a mismatch here aborts the whole pairwise pass via `?`, so these
        // aliases are cheap insurance against a plausible off-spec reply.
        for (raw, expected) in [
            ("a", PairwiseWinner::A),
            ("b", PairwiseWinner::B),
            ("Tie", PairwiseWinner::Tie),
            ("TIE", PairwiseWinner::Tie),
        ] {
            let json = format!(r#"{{"winner": "{raw}", "rationale": ""}}"#);
            let v: PairwiseVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v.winner, expected, "for winner value '{raw}'");
        }
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
