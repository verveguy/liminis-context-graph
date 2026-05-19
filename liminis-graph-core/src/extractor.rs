use crate::{error::Error, types::ExtractionResult};
use reqwest::Client;
use serde_json::{json, Value};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Out-of-process entity/relationship extraction adapter (AD-5, Principle V).
pub struct Extractor {
    api_key: String,
    model: String,
    client: Client,
}

impl Extractor {
    /// Constructs from environment variables.
    ///
    /// - `ANTHROPIC_API_KEY` (required)
    /// - `GRAPHITI_EXTRACTION_LLM` (default `claude-haiku-4-5-20251001`)
    pub fn from_env() -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let model = std::env::var("GRAPHITI_EXTRACTION_LLM")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        Self {
            api_key,
            model,
            client: Client::new(),
        }
    }

    /// Extracts entities and relationships from `episode_body`.
    ///
    /// Uses a structured JSON output prompt; the system prompt is placed in the
    /// `system` field for prompt-cache eligibility (FR-015).
    pub async fn extract(
        &self,
        episode_body: &str,
        _group_id: &str,
    ) -> Result<ExtractionResult, Error> {
        let system = "You are a knowledge graph extraction assistant. \
            Extract named entities and relationships from the given text. \
            Return ONLY valid JSON matching this schema exactly:\n\
            {\"entities\":[{\"name\":\"string\",\"entity_type\":\"string\",\"summary\":\"string\"}],\
            \"edges\":[{\"source_name\":\"string\",\"target_name\":\"string\",\"fact\":\"string\"}]}";

        let body = json!({
            "model": &self.model,
            "max_tokens": 1024,
            "system": system,
            "messages": [
                {
                    "role": "user",
                    "content": format!("Extract entities and relationships from:\n\n{episode_body}")
                }
            ]
        });

        let resp: Value = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // Parse the assistant message content
        let content = resp["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| block["text"].as_str())
            .ok_or_else(|| Error::Ipc("extraction response missing content text".to_string()))?;

        // Extract JSON from the content (may be wrapped in markdown code fences)
        let json_str = extract_json_block(content);
        let result: ExtractionResult = serde_json::from_str(json_str)?;
        Ok(result)
    }
}

fn extract_json_block(s: &str) -> &str {
    // Strip ```json ... ``` fences if present
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
    // Find first '{' and last '}' as fallback
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        return &s[start..=end];
    }
    s.trim()
}
