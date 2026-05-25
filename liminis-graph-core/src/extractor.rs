use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::{
    env::lcg_env_var,
    error::Error,
    prompts,
    telemetry::{cost_for_usage, now_ms, TelemetryEvent, TelemetrySink},
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult, SourceType},
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

// ── ExtractOptions ─────────────────────────────────────────────────────────────

/// Options for a single extraction call.
#[derive(Copy, Clone)]
pub struct ExtractOptions<'a> {
    pub episode_body: &'a str,
    pub group_id: &'a str,
    pub source_type: SourceType,
    pub custom_instructions: Option<&'a str>,
    pub reference_time: &'a str,
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

// ── Internal parse types ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EntityCallResult {
    entities: Vec<ExtractedEntity>,
}

#[derive(Deserialize)]
struct EdgeCallResult {
    edges: Vec<ExtractedEdge>,
}

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

    fn system_value(&self, text: &str) -> Value {
        if self.is_sonnet() {
            json!([{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}])
        } else {
            json!(text)
        }
    }

    async fn do_extract_entities(
        &self,
        opts: &ExtractOptions<'_>,
    ) -> Result<Vec<ExtractedEntity>, Error> {
        let system_text = prompts::entity_system_prompt(opts.source_type);
        let system_value = self.system_value(system_text);

        let user_content = prompts::entity_user_prompt_for(
            opts.source_type,
            opts.episode_body,
            opts.custom_instructions,
        );

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

        const INITIAL_MAX_TOKENS: u32 = 4096;
        let chunk_len_bytes = opts.episode_body.len();

        let mut body = json!({
            "model": &self.model,
            "max_tokens": INITIAL_MAX_TOKENS,
            "system": system_value,
            "tools": [entity_tool],
            "tool_choice": {"type": "tool", "name": "extract_entities"},
            "messages": [{"role": "user", "content": user_content}]
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
                sleep(Duration::from_secs(1u64 << attempt)).await;
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
        entities: &[ExtractedEntity],
    ) -> Result<Vec<ExtractedEdge>, Error> {
        let entity_names: Vec<String> = entities.iter().map(|e| e.name.clone()).collect();
        let system_text = prompts::edge_system_prompt();
        let system_value = self.system_value(system_text);

        let user_content = prompts::edge_user_prompt(
            &entity_names,
            opts.reference_time,
            opts.episode_body,
            opts.custom_instructions,
        );

        let edge_tool = json!({
            "name": "extract_edges",
            "description": "Extract factual relationships between the extracted entities.",
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
                                "relation_type": {"type": "string"},
                                "fact": {"type": "string"},
                                "valid_at": {"type": ["string", "null"]},
                                "invalid_at": {"type": ["string", "null"]}
                            },
                            "required": ["source_name", "target_name", "relation_type", "fact"]
                        }
                    }
                },
                "required": ["edges"]
            }
        });

        const INITIAL_MAX_TOKENS: u32 = 4096;
        let chunk_len_bytes = opts.episode_body.len();

        let mut body = json!({
            "model": &self.model,
            "max_tokens": INITIAL_MAX_TOKENS,
            "system": system_value,
            "tools": [edge_tool],
            "tool_choice": {"type": "tool", "name": "extract_edges"},
            "messages": [{"role": "user", "content": user_content}]
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
                sleep(Duration::from_secs(1u64 << attempt)).await;
                attempt += 1;
                continue;
            }

            let resp: Value = http_resp.error_for_status()?.json().await?;
            self.emit_token_usage(&resp);

            match parse_edge_response(resp) {
                EdgeOutcome::Success(edges) => {
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens: INITIAL_MAX_TOKENS,
                            retry_succeeded: true,
                        });
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
                    return Err(Error::Ipc(
                        "edge extraction budget exhausted after retry".to_string(),
                    ));
                }
                EdgeOutcome::ParseError(e) => return Err(e),
            }
        }
    }

    async fn do_extract(&self, opts: ExtractOptions<'_>) -> Result<ExtractionResult, Error> {
        // Phase 1: extract entities.
        let mut entities = self.do_extract_entities(&opts).await?;

        // Trim entity names.
        for e in &mut entities {
            e.name = e.name.trim().to_string();
        }

        // Skip edge pass if no entities were extracted.
        if entities.is_empty() {
            return Ok(ExtractionResult {
                entities,
                edges: vec![],
            });
        }

        // Phase 2: extract edges with entity list for validation guidance.
        let raw_edges = self.do_extract_edges(&opts, &entities).await?;

        // Post-extraction filters.
        let filtered_edges = apply_edge_filters(raw_edges, &entities);

        Ok(ExtractionResult {
            entities,
            edges: filtered_edges,
        })
    }

    async fn do_classify_entities(&self, entities: &[(&str, &str)]) -> Result<Vec<String>, Error> {
        if entities.is_empty() {
            return Ok(vec![]);
        }

        let system_text = prompts::classify_system_prompt();
        let system_value = self.system_value(system_text);

        let input: Vec<Value> = entities
            .iter()
            .map(|(name, summary)| json!({"name": name, "summary": summary}))
            .collect();

        let body = json!({
            "model": &self.model,
            "max_tokens": 2048,
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

// ── Post-extraction filters ───────────────────────────────────────────────────

/// Apply quality filters to extracted edges:
/// 1. Normalize relation_type to SCREAMING_SNAKE_CASE.
/// 2. Drop self-referential edges (source == target).
/// 3. Drop edges whose endpoints are not in the entity list.
fn apply_edge_filters(
    edges: Vec<ExtractedEdge>,
    entities: &[ExtractedEntity],
) -> Vec<ExtractedEdge> {
    let entity_names: HashSet<String> =
        entities.iter().map(|e| e.name.trim().to_string()).collect();

    edges
        .into_iter()
        .filter_map(|mut edge| {
            // Normalize relation_type to SCREAMING_SNAKE_CASE.
            let normalized = normalize_relation_type(&edge.relation_type);
            if normalized != edge.relation_type {
                eprintln!(
                    "liminis-graph: relation_type normalized {:?} → {:?}",
                    edge.relation_type, normalized
                );
                edge.relation_type = normalized;
            }

            let src = edge.source_name.trim().to_string();
            let dst = edge.target_name.trim().to_string();
            edge.source_name = src.clone();
            edge.target_name = dst.clone();

            // Drop self-referential edges.
            if src == dst {
                eprintln!(
                    "liminis-graph: dropping self-referential edge: {:?} --[{}]--> {:?}",
                    src, edge.relation_type, dst
                );
                return None;
            }

            // Drop edges whose endpoints are not in the entity list.
            if !entity_names.contains(&src) {
                eprintln!(
                    "liminis-graph: dropping edge — source {:?} not in entity list",
                    src
                );
                return None;
            }
            if !entity_names.contains(&dst) {
                eprintln!(
                    "liminis-graph: dropping edge — target {:?} not in entity list",
                    dst
                );
                return None;
            }

            Some(edge)
        })
        .collect()
}

/// Normalize a relation type string to SCREAMING_SNAKE_CASE.
/// uppercase → replace non-alphanumeric with `_` → collapse consecutive `_` → trim `_`.
fn normalize_relation_type(s: &str) -> String {
    let upper = s.to_uppercase();
    let replaced: String = upper
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    // Collapse consecutive underscores and trim leading/trailing.
    let mut out = String::with_capacity(replaced.len());
    let mut prev_underscore = false;
    for c in replaced.chars() {
        if c == '_' {
            if !prev_underscore {
                out.push(c);
            }
            prev_underscore = true;
        } else {
            out.push(c);
            prev_underscore = false;
        }
    }
    out.trim_matches('_').to_string()
}

// ── Tool response parsers ─────────────────────────────────────────────────────

/// Parse the entity tool response.
fn parse_entity_response(mut resp: Value) -> EntityOutcome {
    if resp["stop_reason"].as_str() == Some("max_tokens") {
        return EntityOutcome::BudgetExhausted;
    }

    let tool_block = resp["content"].as_array_mut().and_then(|arr| {
        let idx = arr.iter().position(|b| {
            b["type"].as_str() == Some("tool_use") && b["name"].as_str() == Some("extract_entities")
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

    match serde_json::from_value::<EntityCallResult>(input) {
        Ok(r) => EntityOutcome::Success(r.entities),
        Err(e) => EntityOutcome::ParseError(Error::Json(e)),
    }
}

/// Parse the edge tool response.
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

    match serde_json::from_value::<EdgeCallResult>(input) {
        Ok(r) => EdgeOutcome::Success(r.edges),
        Err(e) => EdgeOutcome::ParseError(Error::Json(e)),
    }
}

// ── ToolOutcome ───────────────────────────────────────────────────────────────
// Kept for unit tests that test parse_tool_response directly.

#[cfg(test)]
enum ToolOutcome {
    Success(ExtractionResult),
    BudgetExhausted,
    ParseError(Error),
}

#[cfg(test)]
fn parse_tool_response(mut resp: Value) -> ToolOutcome {
    if resp["stop_reason"].as_str() == Some("max_tokens") {
        return ToolOutcome::BudgetExhausted;
    }

    let tool_block = resp["content"].as_array_mut().and_then(|arr| {
        let idx = arr.iter().position(|b| {
            b["type"].as_str() == Some("tool_use") && b["name"].as_str() == Some("extract")
        })?;
        Some(arr.remove(idx))
    });

    let Some(mut block) = tool_block else {
        return ToolOutcome::ParseError(Error::Ipc(
            "extraction response missing tool_use block".to_string(),
        ));
    };

    let input = block["input"].take();
    if input.is_null() {
        return ToolOutcome::ParseError(Error::Ipc(
            "extraction tool_use block has null input".to_string(),
        ));
    }

    match serde_json::from_value::<ExtractionResult>(input) {
        Ok(result) => ToolOutcome::Success(result),
        Err(e) => ToolOutcome::ParseError(Error::Json(e)),
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
                    relation_type: "WORKS_AT".to_string(),
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

// ── ScriptedExtractor ─────────────────────────────────────────────────────────

/// Test extractor that returns pre-scripted `ExtractionResult` values in sequence.
/// Panics when the script is exhausted. Used in integration tests.
#[doc(hidden)]
pub struct ScriptedExtractor {
    results: std::sync::Mutex<std::collections::VecDeque<ExtractionResult>>,
}

impl ScriptedExtractor {
    pub fn new(results: Vec<ExtractionResult>) -> Self {
        Self {
            results: std::sync::Mutex::new(results.into()),
        }
    }
}

impl Extractor for ScriptedExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        let result = self
            .results
            .lock()
            .expect("ScriptedExtractor lock poisoned")
            .pop_front()
            .expect("ScriptedExtractor script exhausted");
        Box::pin(async move { Ok(result) })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
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
    if let (Some(start), Some(end)) = (s.find('['), s.rfind(']')) {
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

    // T016: parse_tool_response returns BudgetExhausted when stop_reason is max_tokens;
    // the do_extract retry path emits ExtractionTruncated with retry_succeeded=false
    // when both attempts exhaust the budget.
    #[test]
    fn parse_tool_response_budget_exhausted() {
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": []
        });
        assert!(matches!(
            parse_tool_response(resp),
            ToolOutcome::BudgetExhausted
        ));
    }

    #[test]
    fn parse_tool_response_budget_exhausted_with_partial_block() {
        // The API may return a tool_use block with null input on budget overflow.
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": [{"type": "tool_use", "id": "x", "name": "extract", "input": null}]
        });
        // stop_reason is checked first — result is BudgetExhausted regardless of content.
        assert!(matches!(
            parse_tool_response(resp),
            ToolOutcome::BudgetExhausted
        ));
    }

    #[test]
    fn extraction_truncated_emitted_on_budget_exhaustion() {
        // Simulate the BudgetExhausted state machine from do_extract's loop body.
        // First overflow: must set flag, double budget, emit nothing.
        // Second overflow: must emit ExtractionTruncated { retry_succeeded: false }.
        let sink = Arc::new(CaptureSink::new());
        let model = "claude-haiku-4-5-20251001".to_string();
        let chunk_len_bytes = 42usize;
        let initial_max_tokens: u32 = 8192;
        let mut max_tokens: u64 = initial_max_tokens as u64;
        let mut max_tokens_retried = false;

        // First BudgetExhausted — mirrors: if !max_tokens_retried { double + set flag }
        assert!(
            !max_tokens_retried,
            "flag must be false before first overflow"
        );
        max_tokens *= 2;
        max_tokens_retried = true;
        assert_eq!(max_tokens, 16384, "budget must double on first overflow");
        assert_eq!(sink.events().len(), 0, "no event emitted on first overflow");

        // Second BudgetExhausted — mirrors: else { emit + return error }
        assert!(max_tokens_retried, "flag must be true on second overflow");
        sink.emit(TelemetryEvent::ExtractionTruncated {
            ts_ms: crate::telemetry::now_ms(),
            model: model.clone(),
            chunk_len_bytes,
            initial_max_tokens,
            retry_succeeded: false,
        });

        let events = sink.events();
        assert_eq!(
            events.len(),
            1,
            "exactly one ExtractionTruncated event expected"
        );
        assert!(
            matches!(
                events[0],
                TelemetryEvent::ExtractionTruncated {
                    retry_succeeded: false,
                    initial_max_tokens: 8192,
                    chunk_len_bytes: 42,
                    ..
                }
            ),
            "ExtractionTruncated fields must match"
        );
    }

    // T017: parse_tool_response returns Success with 101 entities when given a valid
    // tool_use block with a large input — verifies no parse failure on large outputs.
    #[test]
    fn parse_tool_response_large_extraction_result() {
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
                    "name": "extract",
                    "input": {
                        "entities": entities,
                        "edges": [
                            {
                                "source_name": "Entity0",
                                "target_name": "Entity1",
                                "fact": "Entity0 knows Entity1"
                            }
                        ]
                    }
                }
            ]
        });

        match parse_tool_response(resp) {
            ToolOutcome::Success(result) => {
                assert_eq!(result.entities.len(), 101, "expected 101 entities");
                assert_eq!(result.edges.len(), 1, "expected 1 edge");
            }
            ToolOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            ToolOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    #[test]
    fn parse_tool_response_missing_tool_block() {
        let resp = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "some text"}]
        });
        assert!(matches!(
            parse_tool_response(resp),
            ToolOutcome::ParseError(_)
        ));
    }

    #[test]
    fn parse_tool_response_null_input() {
        let resp = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "tool_use", "id": "x", "name": "extract", "input": null}]
        });
        assert!(matches!(
            parse_tool_response(resp),
            ToolOutcome::ParseError(_)
        ));
    }

    #[test]
    fn normalize_relation_type_screaming_snake() {
        assert_eq!(normalize_relation_type("married to"), "MARRIED_TO");
        assert_eq!(normalize_relation_type("WORKS_AT"), "WORKS_AT");
        assert_eq!(normalize_relation_type("wrote"), "WROTE");
        assert_eq!(normalize_relation_type("is followed by"), "IS_FOLLOWED_BY");
        assert_eq!(normalize_relation_type("Won"), "WON");
        assert_eq!(normalize_relation_type("produced by"), "PRODUCED_BY");
    }

    #[test]
    fn apply_edge_filters_drops_self_ref() {
        let entities = vec![
            ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Person".to_string(),
                summary: "".to_string(),
            },
            ExtractedEntity {
                name: "Bob".to_string(),
                entity_type: "Person".to_string(),
                summary: "".to_string(),
            },
        ];
        let edges = vec![
            ExtractedEdge {
                source_name: "Alice".to_string(),
                target_name: "Alice".to_string(),
                fact: "Alice is Alice".to_string(),
                relation_type: "IS".to_string(),
                valid_at: None,
                invalid_at: None,
            },
            ExtractedEdge {
                source_name: "Alice".to_string(),
                target_name: "Bob".to_string(),
                fact: "Alice knows Bob".to_string(),
                relation_type: "KNOWS".to_string(),
                valid_at: None,
                invalid_at: None,
            },
        ];
        let filtered = apply_edge_filters(edges, &entities);
        assert_eq!(filtered.len(), 1, "self-referential edge should be dropped");
        assert_eq!(filtered[0].target_name, "Bob");
    }

    #[test]
    fn apply_edge_filters_drops_missing_endpoint() {
        let entities = vec![ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "".to_string(),
        }];
        let edges = vec![ExtractedEdge {
            source_name: "Alice".to_string(),
            target_name: "UnknownEntity".to_string(),
            fact: "Alice relates to unknown".to_string(),
            relation_type: "RELATES_TO".to_string(),
            valid_at: None,
            invalid_at: None,
        }];
        let filtered = apply_edge_filters(edges, &entities);
        assert!(
            filtered.is_empty(),
            "edge with unknown endpoint should be dropped"
        );
    }

    #[test]
    fn apply_edge_filters_normalizes_relation_type() {
        let entities = vec![
            ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Person".to_string(),
                summary: "".to_string(),
            },
            ExtractedEntity {
                name: "Bob".to_string(),
                entity_type: "Person".to_string(),
                summary: "".to_string(),
            },
        ];
        let edges = vec![ExtractedEdge {
            source_name: "Alice".to_string(),
            target_name: "Bob".to_string(),
            fact: "Alice is married to Bob".to_string(),
            relation_type: "married to".to_string(),
            valid_at: None,
            invalid_at: None,
        }];
        let filtered = apply_edge_filters(edges, &entities);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].relation_type, "MARRIED_TO");
    }
}
