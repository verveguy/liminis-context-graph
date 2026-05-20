use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::{
    error::Error,
    telemetry::{cost_for_usage, now_ms, TelemetryEvent, TelemetrySink},
    types::ExtractionResult,
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

// ── Extractor trait ───────────────────────────────────────────────────────────

pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        episode_body: &'a str,
        group_id: &'a str,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>>;
}

// ── AnthropicExtractor ────────────────────────────────────────────────────────

/// Out-of-process entity/relationship extraction adapter (Principle V).
pub struct AnthropicExtractor {
    api_key: String,
    model: String,
    url: String,
    client: Client,
    sink: Arc<dyn TelemetrySink>,
}

impl AnthropicExtractor {
    /// Constructs from environment variables.
    ///
    /// - `ANTHROPIC_API_KEY` (required)
    /// - `GRAPHITI_EXTRACTION_LLM` (default `claude-haiku-4-5-20251001`)
    pub fn from_env(sink: Arc<dyn TelemetrySink>) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        // GRAPHITI_EXTRACTION_LLM may be "primary:fallback" format (consumed by LlmRouter).
        // AnthropicExtractor::from_env only needs the primary token.
        let model = std::env::var("GRAPHITI_EXTRACTION_LLM")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string())
            .split(':')
            .next()
            .unwrap_or("claude-haiku-4-5-20251001")
            .to_string();
        Self { api_key, model, url: ANTHROPIC_API_URL.to_string(), client: Client::new(), sink }
    }

    pub fn with_model(model: String, api_key: String, sink: Arc<dyn TelemetrySink>) -> Self {
        Self { api_key, model, url: ANTHROPIC_API_URL.to_string(), client: Client::new(), sink }
    }

    /// Constructs with a custom API URL — useful for pointing at an unreachable address in tests.
    pub fn with_url(model: String, api_key: String, url: String, sink: Arc<dyn TelemetrySink>) -> Self {
        Self { api_key, model, url, client: Client::new(), sink }
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    fn is_sonnet(&self) -> bool {
        self.model.to_lowercase().contains("sonnet")
    }

    async fn do_extract(&self, episode_body: &str, _group_id: &str) -> Result<ExtractionResult, Error> {
        let system_text = "You are a knowledge graph extraction assistant. \
            Extract named entities and relationships from the given text. \
            Return ONLY valid JSON matching this schema exactly:\n\
            {\"entities\":[{\"name\":\"string\",\"entity_type\":\"string\",\"summary\":\"string\"}],\
            \"edges\":[{\"source_name\":\"string\",\"target_name\":\"string\",\"fact\":\"string\"}]}";

        let system_value: Value = if self.is_sonnet() {
            json!([{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}])
        } else {
            json!(system_text)
        };

        let body = json!({
            "model": &self.model,
            "max_tokens": 1024,
            "system": system_value,
            "messages": [
                {
                    "role": "user",
                    "content": format!("Extract entities and relationships from:\n\n{episode_body}")
                }
            ]
        });

        let mut attempt = 0u32;
        loop {
            let mut req = self
                .client
                .post(&self.url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01");

            if self.is_sonnet() {
                req = req.header("anthropic-beta", "prompt-caching-2024-07-31");
            }

            let http_resp = req.json(&body).send().await?;
            let status = http_resp.status();

            if (status == 429 || status == 529) && attempt < 3 {
                let delay = Duration::from_secs(1u64 << attempt);
                sleep(delay).await;
                attempt += 1;
                continue;
            }

            let resp: Value = http_resp.error_for_status()?.json().await?;
            self.emit_token_usage(&resp);

            let content = resp["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|block| block["text"].as_str())
                .ok_or_else(|| Error::Ipc("extraction response missing content text".to_string()))?;

            let json_str = extract_json_block(content);
            let result: ExtractionResult = serde_json::from_str(json_str)?;
            return Ok(result);
        }
    }

    fn emit_token_usage(&self, resp: &Value) {
        let usage = &resp["usage"];
        if !usage.is_object() {
            return;
        }
        let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
        let cache_read_tokens = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let cache_creation_tokens = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let estimated_cost_usd = cost_for_usage(
            &self.model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        );
        self.sink.emit(TelemetryEvent::TokenUsage {
            ts_ms: now_ms(),
            role: "extraction".to_string(),
            model: self.model.clone(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            estimated_cost_usd,
        });
    }
}

impl Extractor for AnthropicExtractor {
    fn extract<'a>(
        &'a self,
        episode_body: &'a str,
        group_id: &'a str,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(self.do_extract(episode_body, group_id))
    }
}

// ── MockExtractor ─────────────────────────────────────────────────────────────

/// Zero-latency extractor for tests and benches. Returns a fixed 2-entity, 1-edge result.
pub struct MockExtractor;

impl Extractor for MockExtractor {
    fn extract<'a>(
        &'a self,
        _episode_body: &'a str,
        _group_id: &'a str,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        use crate::types::{ExtractedEdge, ExtractedEntity};
        Box::pin(async {
            Ok(ExtractionResult {
                entities: vec![
                    ExtractedEntity {
                        name: "Alice".to_string(),
                        entity_type: "Person".to_string(),
                        summary: "A person named Alice".to_string(),
                    },
                    ExtractedEntity {
                        name: "Acme Corp".to_string(),
                        entity_type: "Organization".to_string(),
                        summary: "A company called Acme Corp".to_string(),
                    },
                ],
                edges: vec![ExtractedEdge {
                    source_name: "Alice".to_string(),
                    target_name: "Acme Corp".to_string(),
                    fact: "Alice works at Acme Corp".to_string(),
                }],
            })
        })
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::NoopSink;

    // T015: Sonnet model uses prompt-caching path; non-Sonnet does not.
    #[test]
    fn sonnet_model_detected_for_prompt_cache() {
        let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
        let sonnet = AnthropicExtractor::with_model(
            "claude-sonnet-4-5-20251115".to_string(),
            "key".to_string(),
            Arc::clone(&sink),
        );
        let haiku = AnthropicExtractor::with_model(
            "claude-haiku-4-5-20251001".to_string(),
            "key".to_string(),
            Arc::clone(&sink),
        );
        assert!(sonnet.is_sonnet(), "sonnet model name should trigger prompt-cache path");
        assert!(!haiku.is_sonnet(), "haiku model name should not trigger prompt-cache path");
    }
}

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
        return &s[start..=end];
    }
    s.trim()
}
