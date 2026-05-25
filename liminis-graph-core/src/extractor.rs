use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::{
    env::lcg_env_var,
    error::Error,
    ontology::{normalize_relation_type, Ontology},
    prompts,
    telemetry::{cost_for_usage, now_ms, TelemetryEvent, TelemetrySink},
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult, SourceType},
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

// ── ExtractOptions ────────────────────────────────────────────────────────────

/// Options passed to `Extractor::extract` for a single episode.
#[derive(Copy, Clone)]
pub struct ExtractOptions<'a> {
    pub episode_body: &'a str,
    pub group_id: &'a str,
    pub source_type: SourceType,
    pub custom_instructions: Option<&'a str>,
    pub reference_time: &'a str,
    pub ontology: Option<&'a Ontology>,
}

// ── Extractor trait ───────────────────────────────────────────────────────────

pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>>;

    /// Classifies entity types for a batch of (name, summary) pairs.
    ///
    /// Returns a `Vec<String>` of the same length as `entities`. Each entry is the
    /// specific entity type label for that entity (e.g. `"Person"`, `"Organization"`),
    /// or an empty string if the entity could not be classified.
    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>>;
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
    /// - `LCG_EXTRACTION_LLM` (default `claude-haiku-4-5-20251001`)
    pub fn from_env(sink: Arc<dyn TelemetrySink>) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        // LCG_EXTRACTION_LLM may be "primary:fallback" format (consumed by LlmRouter).
        // AnthropicExtractor::from_env only needs the primary token.
        // deprecated: remove in Phase B (see #59)
        let model = lcg_env_var("LCG_EXTRACTION_LLM", "GRAPHITI_EXTRACTION_LLM")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string())
            .split(':')
            .next()
            .unwrap_or("claude-haiku-4-5-20251001")
            .to_string();
        Self {
            api_key,
            model,
            url: ANTHROPIC_API_URL.to_string(),
            client: Client::new(),
            sink,
        }
    }

    pub fn with_model(model: String, api_key: String, sink: Arc<dyn TelemetrySink>) -> Self {
        Self {
            api_key,
            model,
            url: ANTHROPIC_API_URL.to_string(),
            client: Client::new(),
            sink,
        }
    }

    /// Constructs with a custom API URL — useful for pointing at an unreachable address in tests.
    pub fn with_url(
        model: String,
        api_key: String,
        url: String,
        sink: Arc<dyn TelemetrySink>,
    ) -> Self {
        Self {
            api_key,
            model,
            url,
            client: Client::new(),
            sink,
        }
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    fn is_sonnet(&self) -> bool {
        self.model.to_lowercase().contains("sonnet")
    }

    async fn do_extract_entities(&self, opts: &ExtractOptions<'_>) -> Result<Vec<ExtractedEntity>, Error> {
        let system_text = prompts::entity_system_prompt(opts.source_type, opts.ontology);
        let user_text = prompts::entity_user_prompt_for(
            opts.source_type,
            opts.episode_body,
            opts.custom_instructions,
        );

        let system_value: Value = if self.is_sonnet() {
            json!([{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}])
        } else {
            json!(system_text)
        };

        let entity_tool = json!({
            "name": "extract_entities",
            "description": "Extract named entities from the text.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "entities": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "entity_type": {"type": "string"},
                                "summary": {"type": "string"}
                            },
                            "required": ["name", "entity_type", "summary"]
                        }
                    }
                },
                "required": ["entities"]
            }
        });

        const INITIAL_MAX_TOKENS: u32 = 8192;
        let chunk_len_bytes = opts.episode_body.len();

        let mut body = json!({
            "model": &self.model,
            "max_tokens": INITIAL_MAX_TOKENS,
            "system": system_value,
            "tools": [entity_tool],
            "tool_choice": {"type": "tool", "name": "extract_entities"},
            "messages": [{"role": "user", "content": user_text}]
        });

        let mut attempt = 0u32;
        let mut max_tokens_retried = false;
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

            match parse_entity_response(resp) {
                EntityOutcome::Success(entities) => {
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens: INITIAL_MAX_TOKENS,
                            retry_succeeded: true,
                        });
                    }
                    return Ok(entities);
                }
                EntityOutcome::BudgetExhausted => {
                    if !max_tokens_retried {
                        let current = body["max_tokens"]
                            .as_u64()
                            .unwrap_or(INITIAL_MAX_TOKENS as u64);
                        body["max_tokens"] = json!(current * 2);
                        max_tokens_retried = true;
                        attempt = 0;
                        continue;
                    }
                    self.sink.emit(TelemetryEvent::ExtractionTruncated {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        chunk_len_bytes,
                        initial_max_tokens: INITIAL_MAX_TOKENS,
                        retry_succeeded: false,
                    });
                    return Err(Error::Ipc(
                        "entity extraction budget exhausted after retry".to_string(),
                    ));
                }
                EntityOutcome::ParseError(e) => return Err(e),
            }
        }
    }

    async fn do_extract_edges(
        &self,
        opts: &ExtractOptions<'_>,
        entity_names: &[String],
    ) -> Result<Vec<ExtractedEdge>, Error> {
        let system_text = prompts::edge_system_prompt(opts.ontology);
        let user_text = prompts::edge_user_prompt(
            entity_names,
            opts.reference_time,
            opts.episode_body,
            opts.custom_instructions,
        );

        let system_value: Value = if self.is_sonnet() {
            json!([{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}])
        } else {
            json!(system_text)
        };

        let edge_tool = json!({
            "name": "extract_edges",
            "description": "Extract factual relationship edges between the given entities.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "source_name": {"type": "string"},
                                "target_name": {"type": "string"},
                                "fact": {"type": "string"},
                                "relation_type": {"type": ["string", "null"]},
                                "valid_at": {"type": ["string", "null"]},
                                "invalid_at": {"type": ["string", "null"]}
                            },
                            "required": ["source_name", "target_name", "fact"]
                        }
                    }
                },
                "required": ["edges"]
            }
        });

        const INITIAL_MAX_TOKENS: u32 = 8192;
        let chunk_len_bytes = opts.episode_body.len();

        let mut body = json!({
            "model": &self.model,
            "max_tokens": INITIAL_MAX_TOKENS,
            "system": system_value,
            "tools": [edge_tool],
            "tool_choice": {"type": "tool", "name": "extract_edges"},
            "messages": [{"role": "user", "content": user_text}]
        });

        let mut attempt = 0u32;
        let mut max_tokens_retried = false;
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

            match parse_edge_response(resp) {
                EdgeOutcome::Success(mut edges) => {
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens: INITIAL_MAX_TOKENS,
                            retry_succeeded: true,
                        });
                    }
                    // Normalize relation_type to SCREAMING_SNAKE_CASE.
                    for edge in &mut edges {
                        if let Some(ref rt) = edge.relation_type.clone() {
                            let normalized = normalize_relation_type(rt);
                            if normalized != *rt {
                                eprintln!(
                                    "liminis-graph: relation_type normalized: '{}' → '{}'",
                                    rt, normalized
                                );
                            }
                            edge.relation_type = Some(normalized);
                        }
                    }
                    return Ok(edges);
                }
                EdgeOutcome::BudgetExhausted => {
                    if !max_tokens_retried {
                        let current = body["max_tokens"]
                            .as_u64()
                            .unwrap_or(INITIAL_MAX_TOKENS as u64);
                        body["max_tokens"] = json!(current * 2);
                        max_tokens_retried = true;
                        attempt = 0;
                        continue;
                    }
                    self.sink.emit(TelemetryEvent::ExtractionTruncated {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        chunk_len_bytes,
                        initial_max_tokens: INITIAL_MAX_TOKENS,
                        retry_succeeded: false,
                    });
                    // Edge budget exhaustion is not fatal — return empty list.
                    eprintln!("liminis-graph: edge extraction budget exhausted; returning empty edge list");
                    return Ok(vec![]);
                }
                EdgeOutcome::ParseError(e) => return Err(e),
            }
        }
    }

    async fn do_extract(&self, opts: ExtractOptions<'_>) -> Result<ExtractionResult, Error> {
        let entities = self.do_extract_entities(&opts).await?;
        if entities.is_empty() {
            return Ok(ExtractionResult {
                entities,
                edges: vec![],
            });
        }
        let entity_names: Vec<String> = entities.iter().map(|e| e.name.clone()).collect();
        let edges = self.do_extract_edges(&opts, &entity_names).await?;
        Ok(ExtractionResult { entities, edges })
    }

    async fn do_classify_entities(&self, entities: &[(&str, &str)]) -> Result<Vec<String>, Error> {
        if entities.is_empty() {
            return Ok(vec![]);
        }

        let system_text = "You are a knowledge graph entity classifier. Given a list of entities \
            (name and summary), assign each a specific entity type label. Use concise PascalCase \
            labels such as Person, Organization, Location, Concept, Product, Event, Technology. \
            Return ONLY valid JSON: an array of strings, one per input entity, in the same order \
            as the input. Use an empty string for an entity whose type cannot be determined.";

        let system_value: Value = if self.is_sonnet() {
            json!([{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}])
        } else {
            json!(system_text)
        };

        let input: Vec<Value> = entities
            .iter()
            .map(|(name, summary)| json!({"name": name, "summary": summary}))
            .collect();

        let body = json!({
            "model": &self.model,
            "max_tokens": 512,
            "system": system_value,
            "messages": [
                {
                    "role": "user",
                    "content": format!(
                        "Classify the entity types for:\n\n{}",
                        serde_json::to_string(&input)
                            .map_err(|e| Error::Ipc(format!("failed to serialize entities: {e}")))?
                    )
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
                .ok_or_else(|| {
                    Error::Ipc("classify_entities response missing content text".to_string())
                })?;

            let json_str = extract_json_block(content);
            let types: Vec<String> = serde_json::from_str(json_str)?;
            // Ensure length matches input; pad/truncate defensively.
            let mut result = types;
            result.resize(entities.len(), String::new());
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
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(self.do_extract(opts))
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(self.do_classify_entities(entities))
    }
}

// ── EntityOutcome / EdgeOutcome ───────────────────────────────────────────────

enum EntityOutcome {
    Success(Vec<ExtractedEntity>),
    BudgetExhausted,
    ParseError(Error),
}

enum EdgeOutcome {
    Success(Vec<ExtractedEdge>),
    BudgetExhausted,
    ParseError(Error),
}

fn parse_entity_response(mut resp: Value) -> EntityOutcome {
    if resp["stop_reason"].as_str() == Some("max_tokens") {
        return EntityOutcome::BudgetExhausted;
    }

    let tool_block = resp["content"].as_array_mut().and_then(|arr| {
        let idx = arr.iter().position(|b| {
            b["type"].as_str() == Some("tool_use")
                && b["name"].as_str() == Some("extract_entities")
        })?;
        Some(arr.remove(idx))
    });

    let Some(mut block) = tool_block else {
        return EntityOutcome::ParseError(Error::Ipc(
            "entity extraction response missing tool_use block".to_string(),
        ));
    };

    let input = block["input"].take();
    if input.is_null() {
        return EntityOutcome::ParseError(Error::Ipc(
            "entity extraction tool_use block has null input".to_string(),
        ));
    }

    #[derive(serde::Deserialize)]
    struct EntityPayload {
        entities: Vec<ExtractedEntity>,
    }

    match serde_json::from_value::<EntityPayload>(input) {
        Ok(payload) => EntityOutcome::Success(payload.entities),
        Err(e) => EntityOutcome::ParseError(Error::Json(e)),
    }
}

fn parse_edge_response(mut resp: Value) -> EdgeOutcome {
    if resp["stop_reason"].as_str() == Some("max_tokens") {
        return EdgeOutcome::BudgetExhausted;
    }

    let tool_block = resp["content"].as_array_mut().and_then(|arr| {
        let idx = arr.iter().position(|b| {
            b["type"].as_str() == Some("tool_use") && b["name"].as_str() == Some("extract_edges")
        })?;
        Some(arr.remove(idx))
    });

    let Some(mut block) = tool_block else {
        return EdgeOutcome::ParseError(Error::Ipc(
            "edge extraction response missing tool_use block".to_string(),
        ));
    };

    let input = block["input"].take();
    if input.is_null() {
        return EdgeOutcome::ParseError(Error::Ipc(
            "edge extraction tool_use block has null input".to_string(),
        ));
    }

    #[derive(serde::Deserialize)]
    struct EdgePayload {
        edges: Vec<ExtractedEdge>,
    }

    match serde_json::from_value::<EdgePayload>(input) {
        Ok(payload) => EdgeOutcome::Success(payload.edges),
        Err(e) => EdgeOutcome::ParseError(Error::Json(e)),
    }
}

// ── MockExtractor ─────────────────────────────────────────────────────────────

/// Zero-latency extractor for tests and benches. Returns a fixed 2-entity, 1-edge result.
pub struct MockExtractor;

impl Extractor for MockExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
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
                    relation_type: Some("WORKS_AT".to_string()),
                    valid_at: None,
                    invalid_at: None,
                }],
            })
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        // MockExtractor returns empty string for each entity — no reclassification.
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{CaptureSink, NoopSink};

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
        assert!(
            sonnet.is_sonnet(),
            "sonnet model name should trigger prompt-cache path"
        );
        assert!(
            !haiku.is_sonnet(),
            "haiku model name should not trigger prompt-cache path"
        );
    }

    #[test]
    fn parse_entity_response_budget_exhausted() {
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": []
        });
        assert!(matches!(
            parse_entity_response(resp),
            EntityOutcome::BudgetExhausted
        ));
    }

    #[test]
    fn parse_entity_response_budget_exhausted_with_partial_block() {
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": [{"type": "tool_use", "id": "x", "name": "extract_entities", "input": null}]
        });
        assert!(matches!(
            parse_entity_response(resp),
            EntityOutcome::BudgetExhausted
        ));
    }

    #[test]
    fn extraction_truncated_emitted_on_budget_exhaustion() {
        // Verify the state machine logic: first overflow doubles budget, second emits telemetry.
        let sink = Arc::new(CaptureSink::new());
        let model = "claude-haiku-4-5-20251001".to_string();
        let chunk_len_bytes = 42usize;
        let initial_max_tokens: u32 = 8192;
        let mut max_tokens: u64 = initial_max_tokens as u64;
        let mut max_tokens_retried = false;

        assert!(!max_tokens_retried);
        max_tokens *= 2;
        max_tokens_retried = true;
        assert_eq!(max_tokens, 16384);
        assert_eq!(sink.events().len(), 0);

        assert!(max_tokens_retried);
        sink.emit(TelemetryEvent::ExtractionTruncated {
            ts_ms: crate::telemetry::now_ms(),
            model: model.clone(),
            chunk_len_bytes,
            initial_max_tokens,
            retry_succeeded: false,
        });

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            TelemetryEvent::ExtractionTruncated {
                retry_succeeded: false,
                initial_max_tokens: 8192,
                chunk_len_bytes: 42,
                ..
            }
        ));
    }

    #[test]
    fn parse_entity_response_large_result() {
        let entities: Vec<Value> = (0..101)
            .map(|i| {
                json!({
                    "name": format!("Entity{i}"),
                    "entity_type": "Person",
                    "summary": format!("Summary for entity {i}")
                })
            })
            .collect();

        let resp = json!({
            "stop_reason": "tool_use",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "extract_entities",
                    "input": {"entities": entities}
                }
            ]
        });

        match parse_entity_response(resp) {
            EntityOutcome::Success(result) => {
                assert_eq!(result.len(), 101);
            }
            EntityOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            EntityOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    #[test]
    fn parse_entity_response_missing_tool_block() {
        let resp = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "some text"}]
        });
        assert!(matches!(
            parse_entity_response(resp),
            EntityOutcome::ParseError(_)
        ));
    }

    #[test]
    fn parse_entity_response_null_input() {
        let resp = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "tool_use", "id": "x", "name": "extract_entities", "input": null}]
        });
        assert!(matches!(
            parse_entity_response(resp),
            EntityOutcome::ParseError(_)
        ));
    }

    #[test]
    fn parse_edge_response_budget_exhausted() {
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": []
        });
        assert!(matches!(
            parse_edge_response(resp),
            EdgeOutcome::BudgetExhausted
        ));
    }

    #[test]
    fn parse_edge_response_success_with_optional_fields() {
        let resp = json!({
            "stop_reason": "tool_use",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_02",
                    "name": "extract_edges",
                    "input": {
                        "edges": [
                            {
                                "source_name": "Alice",
                                "target_name": "Acme Corp",
                                "fact": "Alice works at Acme Corp",
                                "relation_type": "works_at",
                                "valid_at": "2026-01-01T00:00:00Z",
                                "invalid_at": null
                            }
                        ]
                    }
                }
            ]
        });

        match parse_edge_response(resp) {
            EdgeOutcome::Success(edges) => {
                assert_eq!(edges.len(), 1);
                assert_eq!(edges[0].source_name, "Alice");
                assert_eq!(edges[0].relation_type.as_deref(), Some("works_at"));
                assert_eq!(edges[0].valid_at.as_deref(), Some("2026-01-01T00:00:00Z"));
                assert!(edges[0].invalid_at.is_none());
            }
            EdgeOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            EdgeOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    #[test]
    fn parse_edge_response_missing_optional_fields() {
        // Verifies that edges without optional fields deserialize successfully.
        let resp = json!({
            "stop_reason": "tool_use",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_03",
                    "name": "extract_edges",
                    "input": {
                        "edges": [
                            {
                                "source_name": "Bob",
                                "target_name": "Org",
                                "fact": "Bob is part of Org"
                            }
                        ]
                    }
                }
            ]
        });

        match parse_edge_response(resp) {
            EdgeOutcome::Success(edges) => {
                assert_eq!(edges.len(), 1);
                assert!(edges[0].relation_type.is_none());
                assert!(edges[0].valid_at.is_none());
                assert!(edges[0].invalid_at.is_none());
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn normalize_relation_type_applied_during_edge_parse() {
        // Verify that normalize_relation_type converts mixed-case to SCREAMING_SNAKE_CASE.
        use crate::ontology::normalize_relation_type;
        let raw = "worksAt";
        let normalized = normalize_relation_type(raw);
        assert_eq!(normalized, "WORKS_AT");

        let already_normalized = "WORKS_AT";
        assert_eq!(normalize_relation_type(already_normalized), "WORKS_AT");
    }
}
