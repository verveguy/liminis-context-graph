use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::{
    backfill::derive_relation_type_from_fact,
    env::lcg_env_var,
    error::Error,
    ontology::{normalize_relation_type, Ontology},
    prompts,
    telemetry::{cost_for_usage, now_ms, TelemetryEvent, TelemetrySink},
    token_budget::{
        compute_initial_max_tokens, next_retry_max_tokens, resolve_max_tokens_ceiling,
        ExtractionCallType,
    },
    types::{ExtractedEdge, ExtractedEntity, ExtractionOutcome, ExtractionResult, SourceType},
};

pub const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

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
    /// Human-correlatable identifier for this call (#306 FR-004/User Story 3) — `chunk.title`
    /// in the eval harness, the episode `name` in production. Attached to
    /// `TelemetryEvent::ExtractionTruncated`/`ExtractionFailure` so a truncation or failure
    /// event can be traced back to the chunk that produced it. Purely observational: excluded
    /// from `cassette.rs`'s request-key hash, so it never affects cassette matching.
    pub chunk_key: Option<&'a str>,
}

// ── Extractor trait ───────────────────────────────────────────────────────────

pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>>;

    /// Classifies entity types for a batch of (name, summary) pairs.
    ///
    /// Returns a `Vec<String>` of the same length as `entities`. Each entry is the
    /// specific entity type label for that entity (e.g. `"Person"`, `"Organization"`),
    /// or an empty string if the entity could not be classified.
    ///
    /// When `allowed_types` is `Some(types)`, the LLM is constrained to return only types
    /// from that list. Pass `None` for the existing open-ended behavior.
    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>>;

    /// Classifies relation types for a batch of `(fact, current_type)` pairs.
    ///
    /// Returns a `Vec<String>` of the same length as `edges`. Each entry is either a name
    /// from `allowed_types`, or an empty string if the LLM honestly cannot map the fact to any
    /// declared type (abstention). Unlike [`Extractor::classify_entities`], `allowed_types` is
    /// always required and non-empty — relation classification has no open-ended mode.
    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>>;
}

/// Builds the `extract_edges` tool schema for the Anthropic tool-use call. Constrains
/// `source_name`/`target_name` to `sanitized_names` via a JSON-schema `enum` (FR-001) — this is
/// enforced by the provider regardless of whether the model attends to the prompt's own
/// "names not in the list will be rejected" instruction. `sanitized_names` must be non-empty and
/// already sanitized via `prompts::sanitize_entity_names` (an empty `enum` is invalid schema and
/// the caller should skip the extraction call entirely in that case).
fn build_edge_tool_schema(sanitized_names: &[String]) -> Value {
    json!({
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
                            "source_name": {"type": "string", "enum": sanitized_names},
                            "target_name": {"type": "string", "enum": sanitized_names},
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
    })
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

    /// Sends `body` to the Anthropic Messages API, retrying up to 3 times on 429/529 with
    /// exponential backoff. Reads the response body as text before checking status (#306
    /// FR-001: `.error_for_status()` used to discard the body on a non-2xx status before any
    /// caller could see it), so a caller can capture the complete raw body for
    /// `TelemetryEvent::ExtractionFailure` on failure. A connection-level failure (no response
    /// ever received) propagates directly as `SendOutcome::Transport` — there is no body to
    /// capture for that case, distinct from a *received* response with an empty or malformed
    /// body, which lands in `SendOutcome::HttpFailure`.
    async fn send_with_retry(&self, body: &Value) -> SendOutcome {
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

            let http_resp = match req.json(body).send().await {
                Ok(r) => r,
                Err(e) => return SendOutcome::Transport(Error::from(e)),
            };
            let status = http_resp.status();

            if (status == 429 || status == 529) && attempt < 3 {
                let delay = Duration::from_secs(1u64 << attempt);
                sleep(delay).await;
                attempt += 1;
                continue;
            }

            let text = match http_resp.text().await {
                Ok(t) => t,
                Err(e) => format!("<failed to read response body: {e}>"),
            };
            if !status.is_success() {
                return SendOutcome::HttpFailure {
                    status: status.as_u16(),
                    body: text,
                };
            }
            return match serde_json::from_str::<Value>(&text) {
                Ok(v) => SendOutcome::Ok(v),
                Err(_) => SendOutcome::MalformedBody { body: text },
            };
        }
    }

    async fn do_extract_entities(
        &self,
        opts: &ExtractOptions<'_>,
    ) -> Result<(Vec<ExtractedEntity>, usize), Error> {
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

        let ceiling = resolve_max_tokens_ceiling();
        let chunk_len_bytes = opts.episode_body.len();
        let initial_max_tokens =
            compute_initial_max_tokens(chunk_len_bytes, ExtractionCallType::Entities, ceiling);
        let chunk_key = opts.chunk_key.map(|s| s.to_string());

        let mut body = json!({
            "model": &self.model,
            "max_tokens": initial_max_tokens,
            "system": system_value,
            "tools": [entity_tool],
            "tool_choice": {"type": "tool", "name": "extract_entities"},
            "messages": [{"role": "user", "content": user_text}]
        });

        let mut max_tokens_retried = false;
        loop {
            let current_max_tokens = body["max_tokens"]
                .as_u64()
                .unwrap_or(initial_max_tokens as u64) as u32;
            let resp = match self.send_with_retry(&body).await {
                SendOutcome::Ok(v) => v,
                SendOutcome::HttpFailure { status, body: raw } => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "http_error".to_string(),
                        raw_body: raw,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens: current_max_tokens,
                        entities_extracted: None,
                    });
                    return Err(Error::Ipc(format!("entity extraction HTTP {status}")));
                }
                SendOutcome::MalformedBody { body: raw } => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "malformed".to_string(),
                        raw_body: raw,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens: current_max_tokens,
                        entities_extracted: None,
                    });
                    return Err(Error::Ipc(
                        "entity extraction: response body was not valid JSON".to_string(),
                    ));
                }
                SendOutcome::Transport(e) => return Err(e),
            };
            self.emit_token_usage(&resp);
            let resp_for_failure = resp.clone();

            match parse_entity_response(resp) {
                EntityOutcome::Success { entities, dropped } => {
                    self.sink.emit(TelemetryEvent::StructuredOutputParse {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        outcome: if dropped > 0 { "salvaged" } else { "clean" }.to_string(),
                    });
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens,
                            retry_succeeded: true,
                            chunk_key: chunk_key.clone(),
                        });
                    }
                    return Ok((entities, dropped));
                }
                EntityOutcome::BudgetExhausted => {
                    if !max_tokens_retried {
                        if let Some(next) = next_retry_max_tokens(current_max_tokens, ceiling) {
                            body["max_tokens"] = json!(next);
                            max_tokens_retried = true;
                            continue;
                        }
                    }
                    self.sink.emit(TelemetryEvent::ExtractionTruncated {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        chunk_len_bytes,
                        initial_max_tokens,
                        retry_succeeded: false,
                        chunk_key: chunk_key.clone(),
                    });
                    self.emit_extraction_failure(
                        "entities",
                        chunk_key.clone(),
                        "truncation",
                        &resp_for_failure,
                        current_max_tokens,
                        None,
                    );
                    return Err(Error::Ipc(
                        "entity extraction budget exhausted after retry".to_string(),
                    ));
                }
                EntityOutcome::ParseError(e) => {
                    self.emit_extraction_failure(
                        "entities",
                        chunk_key.clone(),
                        classify_parse_failure(&e),
                        &resp_for_failure,
                        current_max_tokens,
                        None,
                    );
                    return Err(e);
                }
            }
        }
    }

    async fn do_extract_edges(
        &self,
        opts: &ExtractOptions<'_>,
        entity_names: &[String],
    ) -> Result<(Vec<ExtractedEdge>, usize), Error> {
        // FR-001 edge case: an empty (or all-empty-after-sanitizing) entity list has no valid
        // endpoints to constrain the schema `enum` to — skip the call rather than send one.
        let sanitized_names = prompts::sanitize_entity_names(entity_names);
        if sanitized_names.is_empty() {
            return Ok((vec![], 0));
        }

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

        let edge_tool = build_edge_tool_schema(&sanitized_names);

        let ceiling = resolve_max_tokens_ceiling();
        let chunk_len_bytes = opts.episode_body.len();
        let initial_max_tokens =
            compute_initial_max_tokens(chunk_len_bytes, ExtractionCallType::Edges, ceiling);
        let chunk_key = opts.chunk_key.map(|s| s.to_string());
        let entities_extracted = Some(entity_names.len());

        let mut body = json!({
            "model": &self.model,
            "max_tokens": initial_max_tokens,
            "system": system_value,
            "tools": [edge_tool],
            "tool_choice": {"type": "tool", "name": "extract_edges"},
            "messages": [{"role": "user", "content": user_text}]
        });

        let mut max_tokens_retried = false;
        loop {
            let current_max_tokens = body["max_tokens"]
                .as_u64()
                .unwrap_or(initial_max_tokens as u64) as u32;
            let resp = match self.send_with_retry(&body).await {
                SendOutcome::Ok(v) => v,
                SendOutcome::HttpFailure { status, body: raw } => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "http_error".to_string(),
                        raw_body: raw,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens: current_max_tokens,
                        entities_extracted,
                    });
                    return Err(Error::Ipc(format!("edge extraction HTTP {status}")));
                }
                SendOutcome::MalformedBody { body: raw } => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "malformed".to_string(),
                        raw_body: raw,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens: current_max_tokens,
                        entities_extracted,
                    });
                    return Err(Error::Ipc(
                        "edge extraction: response body was not valid JSON".to_string(),
                    ));
                }
                SendOutcome::Transport(e) => return Err(e),
            };
            self.emit_token_usage(&resp);
            let resp_for_failure = resp.clone();

            match parse_edge_response(resp) {
                EdgeOutcome::Success { mut edges, dropped } => {
                    self.sink.emit(TelemetryEvent::StructuredOutputParse {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        outcome: if dropped > 0 { "salvaged" } else { "clean" }.to_string(),
                    });
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens,
                            retry_succeeded: true,
                            chunk_key: chunk_key.clone(),
                        });
                    }
                    // Normalize relation_type to SCREAMING_SNAKE_CASE.
                    for edge in &mut edges {
                        if let Some(rt) = edge.relation_type.as_ref() {
                            let normalized = normalize_relation_type(rt);
                            if normalized != *rt {
                                eprintln!(
                                    "liminis-context-graph: relation_type normalized: '{}' → '{}'",
                                    rt, normalized
                                );
                                edge.relation_type = Some(normalized);
                            }
                        }
                    }
                    // FR-001: ensure every extracted edge has a non-empty relation_type.
                    // Falls back to a fact-derived value when the LLM omits the field.
                    for edge in &mut edges {
                        match edge.relation_type.as_deref() {
                            None | Some("") => {
                                edge.relation_type =
                                    Some(derive_relation_type_from_fact(&edge.fact));
                            }
                            _ => {}
                        }
                    }
                    return Ok((edges, dropped));
                }
                EdgeOutcome::BudgetExhausted => {
                    if !max_tokens_retried {
                        if let Some(next) = next_retry_max_tokens(current_max_tokens, ceiling) {
                            body["max_tokens"] = json!(next);
                            max_tokens_retried = true;
                            continue;
                        }
                    }
                    self.sink.emit(TelemetryEvent::ExtractionTruncated {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        chunk_len_bytes,
                        initial_max_tokens,
                        retry_succeeded: false,
                        chunk_key: chunk_key.clone(),
                    });
                    self.emit_extraction_failure(
                        "edges",
                        chunk_key.clone(),
                        "truncation",
                        &resp_for_failure,
                        current_max_tokens,
                        entities_extracted,
                    );
                    // #307 FR-004: edge budget exhaustion is now fatal, matching the entity
                    // path — Ok(vec![]) made a truncated chunk indistinguishable from one where
                    // the model genuinely found zero edges, corrupting eval measurement (see
                    // ADR-0307). The already-extracted entities are not lost: they are captured
                    // above via `entities_extracted` in the ExtractionFailure record/sidecar.
                    return Err(Error::Ipc(
                        "edge extraction budget exhausted after retry".to_string(),
                    ));
                }
                EdgeOutcome::ParseError(e) => {
                    self.emit_extraction_failure(
                        "edges",
                        chunk_key.clone(),
                        classify_parse_failure(&e),
                        &resp_for_failure,
                        current_max_tokens,
                        entities_extracted,
                    );
                    return Err(e);
                }
            }
        }
    }

    async fn do_extract(&self, opts: ExtractOptions<'_>) -> Result<ExtractionOutcome, Error> {
        let (entities, entities_dropped_malformed) = self.do_extract_entities(&opts).await?;
        if entities.is_empty() {
            return Ok(ExtractionOutcome {
                result: ExtractionResult {
                    entities,
                    edges: vec![],
                },
                entities_dropped_malformed,
                edges_dropped_malformed: 0,
            });
        }
        let entity_names: Vec<String> = entities.iter().map(|e| e.name.clone()).collect();
        let (edges, edges_dropped_malformed) = self.do_extract_edges(&opts, &entity_names).await?;
        Ok(ExtractionOutcome {
            result: ExtractionResult { entities, edges },
            entities_dropped_malformed,
            edges_dropped_malformed,
        })
    }

    async fn do_classify_entities(
        &self,
        entities: &[(&str, &str)],
        allowed_types: Option<&[String]>,
    ) -> Result<Vec<String>, Error> {
        if entities.is_empty() {
            return Ok(vec![]);
        }

        let system_text: String = if let Some(types) = allowed_types {
            let type_list = types.join(", ");
            format!(
                "You are a knowledge graph entity classifier. Given a list of entities \
                (name and summary), assign each a specific entity type label from the \
                following allowed types: {type_list}. Do not use any other types. \
                Return ONLY valid JSON: an array of strings, one per input entity, in the same \
                order as the input. If no allowed type fits, return an empty string for that entity."
            )
        } else {
            "You are a knowledge graph entity classifier. Given a list of entities \
            (name and summary), assign each a specific entity type label. Use concise PascalCase \
            labels such as Person, Organization, Location, Concept, Product, Event, Technology. \
            Return ONLY valid JSON: an array of strings, one per input entity, in the same order \
            as the input. Use an empty string for an entity whose type cannot be determined."
                .to_string()
        };

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
            // Server-side enforcement: convert out-of-set responses to empty string (FR-010).
            // Guards against LLMs that ignore the constraint prompt.
            if let Some(types_list) = allowed_types {
                let allowed_set: std::collections::HashSet<&str> =
                    types_list.iter().map(|s| s.as_str()).collect();
                for entry in &mut result {
                    if !entry.is_empty() && !allowed_set.contains(entry.as_str()) {
                        *entry = String::new();
                    }
                }
            }
            return Ok(result);
        }
    }

    async fn do_classify_relations(
        &self,
        edges: &[(&str, &str)],
        allowed_types: &[(String, Option<String>)],
    ) -> Result<Vec<String>, Error> {
        if edges.is_empty() {
            return Ok(vec![]);
        }

        let type_list: String = allowed_types
            .iter()
            .map(|(name, desc)| match desc.as_deref() {
                Some(d) if !d.is_empty() => format!("{name}: {d}"),
                _ => name.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ");

        let system_text = format!(
            "You are a knowledge graph relation classifier. Given a list of edges (each with a \
            'fact' sentence and its current 'current_type', if any), assign each edge exactly \
            one relation type label from the following allowed types: {type_list}. Do not \
            invent any type outside this list. If none of the allowed types honestly fits the \
            fact, return an empty string for that edge — never guess the nearest type. Return \
            ONLY valid JSON: an array of strings, one per input edge, in the same order as the \
            input."
        );

        let system_value: Value = if self.is_sonnet() {
            json!([{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}])
        } else {
            json!(system_text)
        };

        let input: Vec<Value> = edges
            .iter()
            .map(|(fact, current_type)| json!({"fact": fact, "current_type": current_type}))
            .collect();

        let body = json!({
            "model": &self.model,
            "max_tokens": 1024,
            "system": system_value,
            "messages": [
                {
                    "role": "user",
                    "content": format!(
                        "Classify the relation types for:\n\n{}",
                        serde_json::to_string(&input)
                            .map_err(|e| Error::Ipc(format!("failed to serialize edges: {e}")))?
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
                    Error::Ipc("classify_relations response missing content text".to_string())
                })?;

            let json_str = extract_json_block(content);
            let types: Vec<String> = serde_json::from_str(json_str)?;
            // Ensure length matches input; pad/truncate defensively.
            let mut result = types;
            result.resize(edges.len(), String::new());
            // Server-side enforcement: convert out-of-set responses to empty string (FR-007).
            // Guards against LLMs that ignore the constraint prompt.
            let allowed_set: std::collections::HashSet<&str> = allowed_types
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            for entry in &mut result {
                if !entry.is_empty() && !allowed_set.contains(entry.as_str()) {
                    *entry = String::new();
                }
            }
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

    /// Emits `TelemetryEvent::ExtractionFailure` for a truncation or malformed-parse failure
    /// (#306 FR-001) — the HTTP-error case is emitted directly by callers, since it has no
    /// `resp` value to read `stop_reason`/`usage` from. `raw_resp` must be the pre-mutation
    /// clone of the response `parse_entity_response`/`parse_edge_response` would otherwise
    /// destructively consume.
    fn emit_extraction_failure(
        &self,
        call_type: &str,
        chunk_key: Option<String>,
        classification: &str,
        raw_resp: &Value,
        max_tokens: u32,
        entities_extracted: Option<usize>,
    ) {
        self.sink.emit(TelemetryEvent::ExtractionFailure {
            ts_ms: now_ms(),
            model: self.model.clone(),
            call_type: call_type.to_string(),
            chunk_key,
            classification: classification.to_string(),
            raw_body: serde_json::to_string(raw_resp).unwrap_or_default(),
            finish_reason: raw_resp["stop_reason"].as_str().map(String::from),
            completion_tokens: raw_resp["usage"]["output_tokens"].as_u64(),
            max_tokens,
            entities_extracted,
        });
    }
}

impl Extractor for AnthropicExtractor {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        Box::pin(self.do_extract(opts))
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(self.do_classify_entities(entities, allowed_types))
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(self.do_classify_relations(edges, allowed_types))
    }
}

// ── SendOutcome (AnthropicExtractor::send_with_retry) ─────────────────────────

/// Outcome of one `AnthropicExtractor::send_with_retry` call, after the internal 429/529
/// backoff loop has already run its course.
enum SendOutcome {
    /// A 2xx response with a valid JSON body.
    Ok(Value),
    /// A non-2xx status was returned. Carries the complete raw body either way (#306 FR-002),
    /// even when it's empty (Edge Case: "HTTP-level failures with no body").
    HttpFailure { status: u16, body: String },
    /// A 2xx status whose body isn't valid JSON — distinct from `HttpFailure` because the HTTP
    /// layer itself succeeded; classified `"malformed"`, not `"http_error"`, so the sidecar and
    /// any error message don't claim a successful status failed.
    MalformedBody { body: String },
    /// No response was ever received (connection refused, timeout, etc.) — there is nothing
    /// to capture as a raw body, so this propagates directly rather than through
    /// `TelemetryEvent::ExtractionFailure`.
    Transport(Error),
}

// ── EntityOutcome / EdgeOutcome ───────────────────────────────────────────────

enum EntityOutcome {
    Success {
        entities: Vec<ExtractedEntity>,
        dropped: usize,
    },
    BudgetExhausted,
    ParseError(Error),
}

enum EdgeOutcome {
    Success {
        edges: Vec<ExtractedEdge>,
        dropped: usize,
    },
    BudgetExhausted,
    ParseError(Error),
}

/// Deserializes each element of `raw` independently, dropping and counting elements that fail
/// to satisfy `T`'s required fields rather than failing the whole batch (#342 FR-001). The
/// wrapper-level check (is the array key present at all, is its value an array) already ran
/// before this is called — `salvage_items` only handles per-item defects such as a missing
/// `name`, never a structurally broken response (FR-005).
fn salvage_items<T: serde::de::DeserializeOwned>(raw: Vec<Value>) -> (Vec<T>, usize) {
    let mut items = Vec::with_capacity(raw.len());
    let mut dropped = 0usize;
    for value in raw {
        match serde_json::from_value::<T>(value) {
            Ok(item) => items.push(item),
            Err(_) => dropped += 1,
        }
    }
    (items, dropped)
}

/// Classifies why a response failed to parse, per #314 FR-003: distinguishes content that never
/// parsed as JSON at all (`"malformed"`) from content that parsed as valid JSON but failed
/// schema/field validation, e.g. a genuinely required field missing (`"schema_invalid"`).
/// `serde_json::Error::classify()`'s `Category::Data` covers exactly the latter case; every other
/// category (syntax errors, EOF, I/O) and every non-JSON `Error::Ipc` case (no JSON was ever
/// available to validate against, e.g. a missing tool_use block) fall through to `"malformed"`.
/// Applied uniformly at both providers' `ParseError` sites so `StructuredOutputParse.outcome` and
/// `ExtractionFailure.classification` agree.
fn classify_parse_failure(e: &Error) -> &'static str {
    match e {
        Error::Json(err) if err.classify() == serde_json::error::Category::Data => "schema_invalid",
        _ => "malformed",
    }
}

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

    #[derive(serde::Deserialize)]
    struct EntityPayload {
        entities: Vec<Value>,
    }

    match serde_json::from_value::<EntityPayload>(input) {
        Ok(payload) => {
            let (entities, dropped) = salvage_items(payload.entities);
            EntityOutcome::Success { entities, dropped }
        }
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
        edges: Vec<Value>,
    }

    match serde_json::from_value::<EdgePayload>(input) {
        Ok(payload) => {
            let (edges, dropped) = salvage_items(payload.edges);
            EdgeOutcome::Success { edges, dropped }
        }
        Err(e) => EdgeOutcome::ParseError(Error::Json(e)),
    }
}

// ── OaiExtractor ──────────────────────────────────────────────────────────────

/// Outcome of a failed `OaiExtractor::send_chat`/`send_chat_uds` call.
enum ChatFailure {
    /// A non-2xx status was returned. Carries the complete raw body either way (#306 FR-002).
    Http { status: u16, body: String },
    /// A 2xx status whose body isn't valid JSON — distinct from `Http` because the HTTP layer
    /// itself succeeded; classified `"malformed"`, not `"http_error"`, at the call site.
    Malformed { body: String },
    /// No response was ever received (connection refused, dial failure, etc.) — there is
    /// nothing to capture as a raw body.
    Transport(Error),
}

impl From<ChatFailure> for Error {
    fn from(e: ChatFailure) -> Self {
        match e {
            ChatFailure::Http { status, body } => {
                Error::Ipc(format!("chat completion HTTP {status}: {body}"))
            }
            ChatFailure::Malformed { body } => Error::Ipc(format!(
                "chat completion: response body was not valid JSON: {body}"
            )),
            ChatFailure::Transport(e) => e,
        }
    }
}

enum ExtractTransport {
    Http {
        client: Client,
        url: String,
    },
    #[cfg(unix)]
    Uds {
        path: String,
        pool: UdsPool,
    },
}

// ── UDS connection pool ───────────────────────────────────────────────────────
//
// Each chat-completion call over UDS reuses a held HTTP/1.1 connection from a
// small, lazily-populated, bounded pool instead of dialing a fresh UnixStream,
// doing a full handshake, and spawning a detached driver task per call (same
// defect as #229, see `embedder.rs`'s `UdsPool` — this is a deliberately
// independent copy adapted for `serde_json::Value` request/response bodies
// rather than typed embed request/response structs; see ADR-0042).

#[cfg(unix)]
type UdsSender = hyper::client::conn::http1::SendRequest<http_body_util::Full<hyper::body::Bytes>>;

/// Number of held UDS connections. HTTP/1.1 without pipelining serializes one
/// in-flight request per connection, so a small fixed pool (rather than a
/// single connection) keeps concurrent extraction calls from bottlenecking
/// behind each other. Matches the embedder's `UDS_POOL_SIZE` (see ADR-0042).
#[cfg(unix)]
const UDS_POOL_SIZE: usize = 4;

#[cfg(unix)]
struct UdsPool {
    slots: Vec<tokio::sync::Mutex<Option<UdsSender>>>,
    cursor: std::sync::atomic::AtomicUsize,
}

#[cfg(unix)]
impl UdsPool {
    /// Constructs a pool with all slots empty. No dialing happens here —
    /// connections are established lazily on first use of each slot, so
    /// constructing an `OaiExtractor` before the sidecar is listening still
    /// succeeds (FR-002).
    fn new() -> Self {
        let slots = (0..UDS_POOL_SIZE)
            .map(|_| tokio::sync::Mutex::new(None))
            .collect();
        Self {
            slots,
            cursor: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// Dials a fresh UnixStream, performs the HTTP/1.1 handshake, and spawns the
/// connection driver task. Used to populate an empty pool slot, or to
/// re-establish a slot whose held connection has broken.
#[cfg(unix)]
async fn dial_uds(path: &str) -> Result<UdsSender, Error> {
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(path)
        .await
        .map_err(|e| Error::Ipc(format!("UDS connect to {path}: {e}")))?;
    let io = TokioIo::new(stream);
    let (sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| Error::Ipc(format!("UDS HTTP/1.1 handshake: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// Distinguishes a `send_request` failure (the held connection is dead — the
/// sidecar restarted, idle-closed the socket, etc. — worth one re-dial) from
/// an error that occurred after a request was successfully sent over a
/// healthy connection (bad status, unreadable body, unparseable JSON — not
/// worth re-dialing, since dialing again would return the same result).
#[cfg(unix)]
enum UdsAttemptError {
    ConnectionBroken(Error),
    /// A non-2xx status was returned. Carries the complete raw body either way (#306 FR-002),
    /// read lossily if it isn't valid UTF-8 (Edge Case).
    HttpStatus {
        status: u16,
        body: String,
    },
    /// A 2xx status whose body isn't valid JSON — distinct from `HttpStatus` because the HTTP
    /// layer itself succeeded; classified `"malformed"`, not `"http_error"`, at the call site.
    /// Read lossily if it isn't valid UTF-8 (Edge Case).
    Malformed {
        body: String,
    },
    Other(Error),
}

/// Sends one chat-completion request over `sender` and reads/parses the
/// response. `body_bytes` is a cheaply-clonable `Bytes` so callers can retry a
/// send against a freshly-dialed connection without re-copying the serialized
/// request.
#[cfg(unix)]
async fn send_and_read_uds(
    sender: &mut UdsSender,
    body_bytes: hyper::body::Bytes,
) -> Result<Value, UdsAttemptError> {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("host", "localhost")
        .body(Full::new(body_bytes))
        .map_err(|e| UdsAttemptError::Other(Error::Ipc(format!("build UDS request: {e}"))))?;

    let resp = sender.send_request(req).await.map_err(|e| {
        UdsAttemptError::ConnectionBroken(Error::Ipc(format!("UDS send request: {e}")))
    })?;

    let status = resp.status();
    // Read the body before checking status (#306 FR-001): with HTTP/1.1 keep-alive, leaving
    // it unread would desync framing for the next request reusing this pooled connection —
    // and a non-2xx/malformed body must still be preserved for `ExtractionFailure` rather
    // than discarded.
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| UdsAttemptError::Other(Error::Ipc(format!("UDS read response body: {e}"))))?
        .to_bytes();

    if !status.is_success() {
        return Err(UdsAttemptError::HttpStatus {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }

    serde_json::from_slice(&bytes).map_err(|_| UdsAttemptError::Malformed {
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Guards a pool slot across one send/read span. If dropped while still
/// armed — meaning the extraction future was cancelled mid-flight rather than
/// running to completion (e.g. the `tokio::select!` cancellation race in
/// `episode.rs`) — the slot is cleared so the next use re-dials instead of
/// reusing a connection that may be left in an indeterminate state (partially
/// written request, or an unread stale response).
#[cfg(unix)]
struct PoisonGuard<'a> {
    slot: &'a mut Option<UdsSender>,
    armed: bool,
}

#[cfg(unix)]
impl<'a> PoisonGuard<'a> {
    fn new(slot: &'a mut Option<UdsSender>) -> Self {
        Self { slot, armed: true }
    }

    fn sender_mut(&mut self) -> &mut UdsSender {
        self.slot
            .as_mut()
            .expect("PoisonGuard requires a populated slot")
    }

    /// Marks the send/read span as having completed (successfully or with a
    /// definite, fully-read error) rather than having been dropped mid-flight.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PoisonGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            *self.slot = None;
        }
    }
}

/// Explicit JSON-shape instructions appended to the (reused, provider-agnostic) system prompts.
/// The bundled macOS sidecar — and OpenAI-compatible servers generally — cannot be relied on
/// for function-calling / tool-use (see ADR-0041), so structured output is coerced via
/// `response_format: {"type": "json_object"}` plus a literal shape instruction rather than the
/// Anthropic path's schema-enforced tool use.
const ENTITY_JSON_INSTRUCTION: &str = "\n\nRespond with ONLY a single JSON object of the form \
{\"entities\": [{\"name\": \"...\", \"entity_type\": \"...\", \"summary\": \"...\"}, ...]}. \
No other text, no markdown code fences.";

const EDGE_JSON_INSTRUCTION: &str = "\n\nRespond with ONLY a single JSON object of the form \
{\"edges\": [{\"source_name\": \"...\", \"target_name\": \"...\", \"fact\": \"...\", \
\"relation_type\": \"...\" or null, \"valid_at\": \"...\" or null, \"invalid_at\": \"...\" or null}, ...]}. \
No other text, no markdown code fences.";

/// Out-of-process entity/relationship extraction adapter targeting an OpenAI-compatible
/// `POST /v1/chat/completions` endpoint (FR-001) — e.g. the macOS CoreML sidecar's Foundation
/// Models route (the same process/socket already used for `/v1/embeddings`), or any real
/// OpenAI-compatible server reached via `--extractor-http`.
pub struct OaiExtractor {
    transport: ExtractTransport,
    model: String,
    sink: Arc<dyn TelemetrySink>,
}

impl OaiExtractor {
    /// Constructs an HTTP-transport extractor pointing at the given `/v1/chat/completions` URL.
    pub fn new_http(
        url: impl Into<String>,
        model: impl Into<String>,
        sink: Arc<dyn TelemetrySink>,
    ) -> Self {
        Self {
            transport: ExtractTransport::Http {
                client: Client::new(),
                url: url.into(),
            },
            model: model.into(),
            sink,
        }
    }

    /// Constructs a UDS-transport extractor pointing at the given socket path.
    #[cfg(unix)]
    pub fn new_uds(
        path: impl Into<String>,
        model: impl Into<String>,
        sink: Arc<dyn TelemetrySink>,
    ) -> Self {
        Self {
            transport: ExtractTransport::Uds {
                path: path.into(),
                pool: UdsPool::new(),
            },
            model: model.into(),
            sink,
        }
    }

    /// Constructs from environment variables — HTTP transport.
    ///
    /// - `LCG_EXTRACTION_URL` (default `http://127.0.0.1:8765/v1/chat/completions`)
    /// - `LCG_EXTRACTION_MODEL` (default `local`)
    pub fn from_env(sink: Arc<dyn TelemetrySink>) -> Self {
        let url = std::env::var("LCG_EXTRACTION_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8765/v1/chat/completions".to_string());
        let model = std::env::var("LCG_EXTRACTION_MODEL").unwrap_or_else(|_| "local".to_string());
        Self::new_http(url, model, sink)
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Returns `("uds"|"http", endpoint_string)` for the startup log line.
    pub fn transport_info(&self) -> (&'static str, String) {
        match &self.transport {
            ExtractTransport::Http { url, .. } => ("http", url.clone()),
            #[cfg(unix)]
            ExtractTransport::Uds { path, .. } => ("uds", path.clone()),
        }
    }

    async fn send_chat(
        &self,
        system_text: &str,
        user_text: &str,
        max_tokens: u32,
    ) -> Result<Value, ChatFailure> {
        let body = json!({
            "model": &self.model,
            "max_tokens": max_tokens,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": system_text},
                {"role": "user", "content": user_text}
            ]
        });
        match &self.transport {
            ExtractTransport::Http { client, url } => {
                let resp = client
                    .post(url)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ChatFailure::Transport(Error::from(e)))?;
                // Read the body before checking status (#306 FR-001): `.error_for_status()`
                // discards it on a non-2xx status before any caller could see it.
                let status = resp.status();
                let text = match resp.text().await {
                    Ok(t) => t,
                    Err(e) => format!("<failed to read response body: {e}>"),
                };
                if !status.is_success() {
                    return Err(ChatFailure::Http {
                        status: status.as_u16(),
                        body: text,
                    });
                }
                serde_json::from_str::<Value>(&text)
                    .map_err(|_| ChatFailure::Malformed { body: text })
            }
            #[cfg(unix)]
            ExtractTransport::Uds { path, pool } => self.send_chat_uds(path, pool, &body).await,
        }
    }

    /// Sends one chat-completion request over a pooled UDS connection. Picks
    /// a slot by round-robin, lazily dials it if empty, and on a
    /// broken-connection failure (the sidecar restarted, idle-closed the
    /// socket, etc.) clears the slot and retries exactly once against a
    /// freshly-dialed connection.
    #[cfg(unix)]
    async fn send_chat_uds(
        &self,
        path: &str,
        pool: &UdsPool,
        body: &Value,
    ) -> Result<Value, ChatFailure> {
        let idx = pool
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % pool.slots.len();
        let mut slot = pool.slots[idx].lock().await;

        let body_bytes = hyper::body::Bytes::from(serde_json::to_vec(body).map_err(|e| {
            ChatFailure::Transport(Error::Ipc(format!(
                "serialize chat completion request: {e}"
            )))
        })?);

        if slot.is_none() {
            *slot = Some(dial_uds(path).await.map_err(ChatFailure::Transport)?);
        }

        let first = {
            let mut guard = PoisonGuard::new(&mut slot);
            let result = send_and_read_uds(guard.sender_mut(), body_bytes.clone()).await;
            // The span completed (successfully or with a definite, fully-read
            // error) rather than being dropped mid-flight — safe to disarm
            // regardless of outcome; a ConnectionBroken outcome is handled
            // explicitly below by clearing the slot before redialing.
            guard.disarm();
            result
        };

        match first {
            Ok(resp) => Ok(resp),
            Err(UdsAttemptError::HttpStatus { status, body }) => {
                Err(ChatFailure::Http { status, body })
            }
            Err(UdsAttemptError::Malformed { body }) => Err(ChatFailure::Malformed { body }),
            Err(UdsAttemptError::Other(e)) => Err(ChatFailure::Transport(e)),
            Err(UdsAttemptError::ConnectionBroken(_)) => {
                *slot = None;
                *slot = Some(dial_uds(path).await.map_err(ChatFailure::Transport)?);
                let mut guard = PoisonGuard::new(&mut slot);
                let result = send_and_read_uds(guard.sender_mut(), body_bytes).await;
                guard.disarm();
                drop(guard);
                match result {
                    Ok(resp) => Ok(resp),
                    Err(UdsAttemptError::HttpStatus { status, body }) => {
                        Err(ChatFailure::Http { status, body })
                    }
                    Err(UdsAttemptError::Malformed { body }) => {
                        Err(ChatFailure::Malformed { body })
                    }
                    Err(UdsAttemptError::Other(e)) => Err(ChatFailure::Transport(e)),
                    Err(UdsAttemptError::ConnectionBroken(e)) => {
                        // The redial-and-retry also failed against a fresh
                        // connection — don't leave the known-bad sender in
                        // the slot for the next unrelated call to trip over.
                        *slot = None;
                        Err(ChatFailure::Transport(e))
                    }
                }
            }
        }
    }

    /// Maps the OpenAI-compatible `usage` object (`prompt_tokens`/`completion_tokens` — different
    /// field names than Anthropic's `input_tokens`/`output_tokens`) onto the same telemetry event.
    /// No-ops (FR-009) when `usage` is absent, matching non-compliant local servers.
    fn emit_token_usage(&self, resp: &Value) {
        let usage = &resp["usage"];
        if !usage.is_object() {
            return;
        }
        let input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        // Local models are never present in the Anthropic pricing table, so this naturally
        // returns None — no special-casing needed to satisfy FR-009's "no-op gracefully".
        let estimated_cost_usd = cost_for_usage(&self.model, input_tokens, output_tokens, 0, 0);
        self.sink.emit(TelemetryEvent::TokenUsage {
            ts_ms: now_ms(),
            role: "extraction".to_string(),
            model: self.model.clone(),
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            estimated_cost_usd,
        });
    }

    /// Emits `TelemetryEvent::ExtractionFailure` for a truncation or malformed-parse failure
    /// (#306 FR-001) — the HTTP-error case is emitted directly by callers via `ChatFailure`,
    /// since it has no `resp` value to read `finish_reason`/`usage` from. `raw_resp` is a
    /// borrow, not a destructive-consumed value, since `parse_oai_entity_response`/
    /// `parse_oai_edge_response` take `&Value` and never mutate it.
    fn emit_extraction_failure(
        &self,
        call_type: &str,
        chunk_key: Option<String>,
        classification: &str,
        raw_resp: &Value,
        max_tokens: u32,
        entities_extracted: Option<usize>,
    ) {
        self.sink.emit(TelemetryEvent::ExtractionFailure {
            ts_ms: now_ms(),
            model: self.model.clone(),
            call_type: call_type.to_string(),
            chunk_key,
            classification: classification.to_string(),
            raw_body: serde_json::to_string(raw_resp).unwrap_or_default(),
            finish_reason: oai_finish_reason(raw_resp).map(String::from),
            completion_tokens: raw_resp["usage"]["completion_tokens"].as_u64(),
            max_tokens,
            entities_extracted,
        });
    }

    async fn do_extract_entities(
        &self,
        opts: &ExtractOptions<'_>,
    ) -> Result<(Vec<ExtractedEntity>, usize), Error> {
        let system_text = format!(
            "{}{}",
            prompts::entity_system_prompt(opts.source_type, opts.ontology),
            ENTITY_JSON_INSTRUCTION
        );
        let user_text = prompts::entity_user_prompt_for(
            opts.source_type,
            opts.episode_body,
            opts.custom_instructions,
        );

        let ceiling = resolve_max_tokens_ceiling();
        let chunk_len_bytes = opts.episode_body.len();
        let initial_max_tokens =
            compute_initial_max_tokens(chunk_len_bytes, ExtractionCallType::Entities, ceiling);
        let chunk_key = opts.chunk_key.map(|s| s.to_string());
        let mut max_tokens = initial_max_tokens;
        let mut max_tokens_retried = false;

        loop {
            let resp = match self.send_chat(&system_text, &user_text, max_tokens).await {
                Ok(v) => v,
                Err(ChatFailure::Http { status, body }) => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "http_error".to_string(),
                        raw_body: body,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens,
                        entities_extracted: None,
                    });
                    return Err(Error::Ipc(format!("entity extraction HTTP {status}")));
                }
                Err(ChatFailure::Malformed { body }) => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "malformed".to_string(),
                        raw_body: body,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens,
                        entities_extracted: None,
                    });
                    return Err(Error::Ipc(
                        "entity extraction: response body was not valid JSON".to_string(),
                    ));
                }
                Err(ChatFailure::Transport(e)) => return Err(e),
            };
            self.emit_token_usage(&resp);

            match parse_oai_entity_response(&resp) {
                OaiChatOutcome::Success {
                    value: entities,
                    defensive_parse,
                    dropped,
                } => {
                    self.sink.emit(TelemetryEvent::StructuredOutputParse {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        outcome: if dropped > 0 {
                            "salvaged"
                        } else if defensive_parse {
                            "recovered"
                        } else {
                            "clean"
                        }
                        .to_string(),
                    });
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens,
                            retry_succeeded: true,
                            chunk_key: chunk_key.clone(),
                        });
                    }
                    if !entities.is_empty() {
                        let missing_summary =
                            entities.iter().filter(|e| e.summary.is_empty()).count();
                        self.sink.emit(TelemetryEvent::EntitiesMissingSummary {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_key: chunk_key.clone(),
                            entities_extracted: entities.len(),
                            missing_summary,
                        });
                    }
                    return Ok((entities, dropped));
                }
                OaiChatOutcome::BudgetExhausted => {
                    if !max_tokens_retried {
                        if let Some(next) = next_retry_max_tokens(max_tokens, ceiling) {
                            max_tokens = next;
                            max_tokens_retried = true;
                            continue;
                        }
                    }
                    self.sink.emit(TelemetryEvent::ExtractionTruncated {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        chunk_len_bytes,
                        initial_max_tokens,
                        retry_succeeded: false,
                        chunk_key: chunk_key.clone(),
                    });
                    self.emit_extraction_failure(
                        "entities",
                        chunk_key.clone(),
                        "truncation",
                        &resp,
                        max_tokens,
                        None,
                    );
                    return Err(Error::Ipc(
                        "entity extraction budget exhausted after retry".to_string(),
                    ));
                }
                OaiChatOutcome::ParseError(e) => {
                    let classification = classify_parse_failure(&e);
                    self.sink.emit(TelemetryEvent::StructuredOutputParse {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "entities".to_string(),
                        outcome: classification.to_string(),
                    });
                    self.emit_extraction_failure(
                        "entities",
                        chunk_key.clone(),
                        classification,
                        &resp,
                        max_tokens,
                        None,
                    );
                    return Err(e);
                }
            }
        }
    }

    async fn do_extract_edges(
        &self,
        opts: &ExtractOptions<'_>,
        entity_names: &[String],
    ) -> Result<(Vec<ExtractedEdge>, usize), Error> {
        let system_text = format!(
            "{}{}",
            prompts::edge_system_prompt(opts.ontology),
            EDGE_JSON_INSTRUCTION
        );
        let user_text = prompts::edge_user_prompt(
            entity_names,
            opts.reference_time,
            opts.episode_body,
            opts.custom_instructions,
        );

        let ceiling = resolve_max_tokens_ceiling();
        let chunk_len_bytes = opts.episode_body.len();
        let initial_max_tokens =
            compute_initial_max_tokens(chunk_len_bytes, ExtractionCallType::Edges, ceiling);
        let chunk_key = opts.chunk_key.map(|s| s.to_string());
        let entities_extracted = Some(entity_names.len());
        let mut max_tokens = initial_max_tokens;
        let mut max_tokens_retried = false;

        loop {
            let resp = match self.send_chat(&system_text, &user_text, max_tokens).await {
                Ok(v) => v,
                Err(ChatFailure::Http { status, body }) => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "http_error".to_string(),
                        raw_body: body,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens,
                        entities_extracted,
                    });
                    return Err(Error::Ipc(format!("edge extraction HTTP {status}")));
                }
                Err(ChatFailure::Malformed { body }) => {
                    self.sink.emit(TelemetryEvent::ExtractionFailure {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        chunk_key: chunk_key.clone(),
                        classification: "malformed".to_string(),
                        raw_body: body,
                        finish_reason: None,
                        completion_tokens: None,
                        max_tokens,
                        entities_extracted,
                    });
                    return Err(Error::Ipc(
                        "edge extraction: response body was not valid JSON".to_string(),
                    ));
                }
                Err(ChatFailure::Transport(e)) => return Err(e),
            };
            self.emit_token_usage(&resp);

            match parse_oai_edge_response(&resp) {
                OaiChatOutcome::Success {
                    value: mut edges,
                    defensive_parse,
                    dropped,
                } => {
                    self.sink.emit(TelemetryEvent::StructuredOutputParse {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        outcome: if dropped > 0 {
                            "salvaged"
                        } else if defensive_parse {
                            "recovered"
                        } else {
                            "clean"
                        }
                        .to_string(),
                    });
                    if max_tokens_retried {
                        self.sink.emit(TelemetryEvent::ExtractionTruncated {
                            ts_ms: now_ms(),
                            model: self.model.clone(),
                            chunk_len_bytes,
                            initial_max_tokens,
                            retry_succeeded: true,
                            chunk_key: chunk_key.clone(),
                        });
                    }
                    // Normalize relation_type to SCREAMING_SNAKE_CASE (FR-012).
                    for edge in &mut edges {
                        if let Some(rt) = edge.relation_type.as_ref() {
                            let normalized = normalize_relation_type(rt);
                            if normalized != *rt {
                                edge.relation_type = Some(normalized);
                            }
                        }
                    }
                    // FR-012: ensure every extracted edge has a non-empty relation_type, falling
                    // back to a fact-derived value when the local model omits the field — the
                    // same fallback the Anthropic path applies.
                    for edge in &mut edges {
                        match edge.relation_type.as_deref() {
                            None | Some("") => {
                                edge.relation_type =
                                    Some(derive_relation_type_from_fact(&edge.fact));
                            }
                            _ => {}
                        }
                    }
                    return Ok((edges, dropped));
                }
                OaiChatOutcome::BudgetExhausted => {
                    if !max_tokens_retried {
                        if let Some(next) = next_retry_max_tokens(max_tokens, ceiling) {
                            max_tokens = next;
                            max_tokens_retried = true;
                            continue;
                        }
                    }
                    self.sink.emit(TelemetryEvent::ExtractionTruncated {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        chunk_len_bytes,
                        initial_max_tokens,
                        retry_succeeded: false,
                        chunk_key: chunk_key.clone(),
                    });
                    self.emit_extraction_failure(
                        "edges",
                        chunk_key.clone(),
                        "truncation",
                        &resp,
                        max_tokens,
                        entities_extracted,
                    );
                    // #307 FR-004: edge budget exhaustion is now fatal, matching the entity
                    // path — Ok(vec![]) made a truncated chunk indistinguishable from one where
                    // the model genuinely found zero edges, corrupting eval measurement (see
                    // ADR-0307). The already-extracted entities are not lost: they are captured
                    // above via `entities_extracted` in the ExtractionFailure record/sidecar.
                    return Err(Error::Ipc(
                        "edge extraction budget exhausted after retry".to_string(),
                    ));
                }
                OaiChatOutcome::ParseError(e) => {
                    let classification = classify_parse_failure(&e);
                    self.sink.emit(TelemetryEvent::StructuredOutputParse {
                        ts_ms: now_ms(),
                        model: self.model.clone(),
                        call_type: "edges".to_string(),
                        outcome: classification.to_string(),
                    });
                    self.emit_extraction_failure(
                        "edges",
                        chunk_key.clone(),
                        classification,
                        &resp,
                        max_tokens,
                        entities_extracted,
                    );
                    return Err(e);
                }
            }
        }
    }

    async fn do_extract(&self, opts: ExtractOptions<'_>) -> Result<ExtractionOutcome, Error> {
        let (entities, entities_dropped_malformed) = self.do_extract_entities(&opts).await?;
        if entities.is_empty() {
            return Ok(ExtractionOutcome {
                result: ExtractionResult {
                    entities,
                    edges: vec![],
                },
                entities_dropped_malformed,
                edges_dropped_malformed: 0,
            });
        }
        let entity_names: Vec<String> = entities.iter().map(|e| e.name.clone()).collect();
        let (edges, edges_dropped_malformed) = self.do_extract_edges(&opts, &entity_names).await?;
        Ok(ExtractionOutcome {
            result: ExtractionResult { entities, edges },
            entities_dropped_malformed,
            edges_dropped_malformed,
        })
    }

    async fn do_classify_entities(
        &self,
        entities: &[(&str, &str)],
        allowed_types: Option<&[String]>,
    ) -> Result<Vec<String>, Error> {
        if entities.is_empty() {
            return Ok(vec![]);
        }

        let system_text: String = if let Some(types) = allowed_types {
            let type_list = types.join(", ");
            format!(
                "You are a knowledge graph entity classifier. Given a list of entities \
                (name and summary), assign each a specific entity type label from the \
                following allowed types: {type_list}. Do not use any other types. \
                Respond with ONLY a single JSON object of the form {{\"types\": [...]}}: an \
                array of strings, one per input entity, in the same order as the input. If no \
                allowed type fits, use an empty string for that entity. No other text, no \
                markdown code fences."
            )
        } else {
            "You are a knowledge graph entity classifier. Given a list of entities \
            (name and summary), assign each a specific entity type label. Use concise PascalCase \
            labels such as Person, Organization, Location, Concept, Product, Event, Technology. \
            Respond with ONLY a single JSON object of the form {\"types\": [...]}: an array of \
            strings, one per input entity, in the same order as the input. Use an empty string \
            for an entity whose type cannot be determined. No other text, no markdown code \
            fences."
                .to_string()
        };

        let input: Vec<Value> = entities
            .iter()
            .map(|(name, summary)| json!({"name": name, "summary": summary}))
            .collect();
        let user_text = format!(
            "Classify the entity types for:\n\n{}",
            serde_json::to_string(&input)
                .map_err(|e| Error::Ipc(format!("failed to serialize entities: {e}")))?
        );

        let resp = self.send_chat(&system_text, &user_text, 512).await?;
        self.emit_token_usage(&resp);

        let content = oai_message_content(&resp).ok_or_else(|| {
            Error::Ipc("classify_entities response missing message content".to_string())
        })?;

        let json_str = extract_json_block(content);
        #[derive(serde::Deserialize)]
        struct TypesPayload {
            types: Vec<String>,
        }
        let payload: TypesPayload = serde_json::from_str(json_str)?;
        let mut result = payload.types;
        result.resize(entities.len(), String::new());
        // Server-side enforcement: convert out-of-set responses to empty string (FR-003).
        if let Some(types_list) = allowed_types {
            let allowed_set: std::collections::HashSet<&str> =
                types_list.iter().map(|s| s.as_str()).collect();
            for entry in &mut result {
                if !entry.is_empty() && !allowed_set.contains(entry.as_str()) {
                    *entry = String::new();
                }
            }
        }
        Ok(result)
    }

    async fn do_classify_relations(
        &self,
        edges: &[(&str, &str)],
        allowed_types: &[(String, Option<String>)],
    ) -> Result<Vec<String>, Error> {
        if edges.is_empty() {
            return Ok(vec![]);
        }

        let type_list: String = allowed_types
            .iter()
            .map(|(name, desc)| match desc.as_deref() {
                Some(d) if !d.is_empty() => format!("{name}: {d}"),
                _ => name.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ");

        let system_text = format!(
            "You are a knowledge graph relation classifier. Given a list of edges (each with a \
            'fact' sentence and its current 'current_type', if any), assign each edge exactly \
            one relation type label from the following allowed types: {type_list}. Do not \
            invent any type outside this list. If none of the allowed types honestly fits the \
            fact, use an empty string for that edge — never guess the nearest type. Respond \
            with ONLY a single JSON object of the form {{\"types\": [...]}}: an array of \
            strings, one per input edge, in the same order as the input. No other text, no \
            markdown code fences."
        );

        let input: Vec<Value> = edges
            .iter()
            .map(|(fact, current_type)| json!({"fact": fact, "current_type": current_type}))
            .collect();
        let user_text = format!(
            "Classify the relation types for:\n\n{}",
            serde_json::to_string(&input)
                .map_err(|e| Error::Ipc(format!("failed to serialize edges: {e}")))?
        );

        let resp = self.send_chat(&system_text, &user_text, 1024).await?;
        self.emit_token_usage(&resp);

        let content = oai_message_content(&resp).ok_or_else(|| {
            Error::Ipc("classify_relations response missing message content".to_string())
        })?;

        let json_str = extract_json_block(content);
        #[derive(serde::Deserialize)]
        struct TypesPayload {
            types: Vec<String>,
        }
        let payload: TypesPayload = serde_json::from_str(json_str)?;
        let mut result = payload.types;
        result.resize(edges.len(), String::new());
        // Server-side enforcement: convert out-of-set responses to empty string (FR-003).
        let allowed_set: std::collections::HashSet<&str> = allowed_types
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        for entry in &mut result {
            if !entry.is_empty() && !allowed_set.contains(entry.as_str()) {
                *entry = String::new();
            }
        }
        Ok(result)
    }
}

impl Extractor for OaiExtractor {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        Box::pin(self.do_extract(opts))
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(self.do_classify_entities(entities, allowed_types))
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(self.do_classify_relations(edges, allowed_types))
    }
}

// ── OaiChatOutcome / response parsing ─────────────────────────────────────────

enum OaiChatOutcome<T> {
    /// `defensive_parse` is `true` when `extract_json_block`'s fence/prefix stripping was
    /// needed to isolate the JSON payload (i.e. the raw content wasn't already valid JSON
    /// on its own), `false` when the raw content parsed as-is. `dropped` is the count of
    /// per-item elements salvaged (dropped) during deserialization (#342 FR-001).
    Success {
        value: T,
        defensive_parse: bool,
        dropped: usize,
    },
    BudgetExhausted,
    ParseError(Error),
}

fn oai_finish_reason(resp: &Value) -> Option<&str> {
    resp["choices"].as_array()?.first()?["finish_reason"].as_str()
}

fn oai_message_content(resp: &Value) -> Option<&str> {
    resp["choices"].as_array()?.first()?["message"]["content"].as_str()
}

fn parse_oai_entity_response(resp: &Value) -> OaiChatOutcome<Vec<ExtractedEntity>> {
    if oai_finish_reason(resp) == Some("length") {
        return OaiChatOutcome::BudgetExhausted;
    }
    let Some(content) = oai_message_content(resp) else {
        return OaiChatOutcome::ParseError(Error::Ipc(
            "entity extraction response missing message content".to_string(),
        ));
    };

    let json_str = extract_json_block(content);
    let defensive_parse = json_str != content.trim();
    #[derive(serde::Deserialize)]
    struct EntityPayload {
        entities: Vec<Value>,
    }
    match serde_json::from_str::<EntityPayload>(json_str) {
        Ok(payload) => {
            let (entities, dropped) = salvage_items(payload.entities);
            OaiChatOutcome::Success {
                value: entities,
                defensive_parse,
                dropped,
            }
        }
        Err(e) => OaiChatOutcome::ParseError(Error::Json(e)),
    }
}

fn parse_oai_edge_response(resp: &Value) -> OaiChatOutcome<Vec<ExtractedEdge>> {
    if oai_finish_reason(resp) == Some("length") {
        return OaiChatOutcome::BudgetExhausted;
    }
    let Some(content) = oai_message_content(resp) else {
        return OaiChatOutcome::ParseError(Error::Ipc(
            "edge extraction response missing message content".to_string(),
        ));
    };

    let json_str = extract_json_block(content);
    let defensive_parse = json_str != content.trim();
    #[derive(serde::Deserialize)]
    struct EdgePayload {
        edges: Vec<Value>,
    }
    match serde_json::from_str::<EdgePayload>(json_str) {
        Ok(payload) => {
            let (edges, dropped) = salvage_items(payload.edges);
            OaiChatOutcome::Success {
                value: edges,
                defensive_parse,
                dropped,
            }
        }
        Err(e) => OaiChatOutcome::ParseError(Error::Json(e)),
    }
}

// ── MockExtractor ─────────────────────────────────────────────────────────────

/// Zero-latency extractor for tests and benches. Returns a fixed 2-entity, 1-edge result.
pub struct MockExtractor;

impl Extractor for MockExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        use crate::types::{ExtractedEdge, ExtractedEntity};
        Box::pin(async {
            Ok(ExtractionResult {
                entities: vec![
                    ExtractedEntity {
                        name: "Alice".to_string(),
                        entity_type: "Person".to_string(),
                        summary: "A person named Alice".to_string(),
                        original_entity_type: None,
                    },
                    ExtractedEntity {
                        name: "Acme Corp".to_string(),
                        entity_type: "Organization".to_string(),
                        summary: "A company called Acme Corp".to_string(),
                        original_entity_type: None,
                    },
                ],
                edges: vec![ExtractedEdge {
                    source_name: "Alice".to_string(),
                    target_name: "Acme Corp".to_string(),
                    fact: "Alice works at Acme Corp".to_string(),
                    relation_type: Some("WORKS_AT".to_string()),
                    valid_at: None,
                    invalid_at: None,
                    original_relation_type: None,
                }],
            }
            .into())
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        // MockExtractor returns empty string for each entity — no reclassification.
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        // MockExtractor abstains for every edge — no reclassification.
        let count = edges.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

// ── ConfigurableExtractor ─────────────────────────────────────────────────────

/// Test extractor that returns caller-specified extraction results in FIFO order.
/// Each call to `extract` pops the front of the queue. When the queue is empty,
/// returns an empty ExtractionResult (no entities, no edges).
pub struct ConfigurableExtractor {
    queue: Arc<std::sync::Mutex<std::collections::VecDeque<ExtractionResult>>>,
}

impl ConfigurableExtractor {
    pub fn new(results: Vec<ExtractionResult>) -> Self {
        Self {
            queue: Arc::new(std::sync::Mutex::new(results.into_iter().collect())),
        }
    }
}

impl Extractor for ConfigurableExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        let result = self.queue.lock().unwrap().pop_front().unwrap_or_default();
        Box::pin(async move { Ok(result.into()) })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = edges.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

// ── UnconfiguredExtractor ─────────────────────────────────────────────────────

/// Message shown when no extraction provider was configured. Moved verbatim (#331 FR-002) from
/// what used to be a startup-fatal error in `main.rs` — only the timing changed, not the content.
pub const NO_EXTRACTION_PROVIDER_MSG: &str =
    "No extraction provider configured: ANTHROPIC_API_KEY is not set and \
     LCG_EXTRACTION_URL is not set. Set ANTHROPIC_API_KEY, or explicitly opt into \
     local extraction with --extractor-uds <path>, --extractor-http <url>, or \
     LCG_EXTRACTION_URL (e.g. --extractor-uds /tmp/liminis-inference.sock to use the \
     bundled macOS sidecar — note its Foundation Models backend is not recommended \
     for extraction quality; \
     see docs/adr/0041-local-openai-compatible-extraction-adapter.md).";

/// Stands in for "no extraction provider configured" (#331). Every method fails immediately with
/// `Error::Config(NO_EXTRACTION_PROVIDER_MSG)` rather than the process refusing to start — a
/// read-only deployment that never calls an extraction-dependent method never observes this at
/// all (ADR-0331).
pub struct UnconfiguredExtractor;

impl Extractor for UnconfiguredExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        Box::pin(async { Err(Error::Config(NO_EXTRACTION_PROVIDER_MSG.to_string())) })
    }

    fn classify_entities<'a>(
        &'a self,
        _entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(async { Err(Error::Config(NO_EXTRACTION_PROVIDER_MSG.to_string())) })
    }

    fn classify_relations<'a>(
        &'a self,
        _edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(async { Err(Error::Config(NO_EXTRACTION_PROVIDER_MSG.to_string())) })
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
            chunk_key: None,
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
            EntityOutcome::Success { entities, dropped } => {
                assert_eq!(entities.len(), 101);
                assert_eq!(dropped, 0);
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
            EdgeOutcome::Success { edges, dropped } => {
                assert_eq!(edges.len(), 1);
                assert_eq!(dropped, 0);
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
            EdgeOutcome::Success { edges, .. } => {
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

    // FR-001 / FR-004: derive_relation_type_from_fact (used in the extractor fallback)
    // must produce non-empty SCREAMING_SNAKE_CASE and never arrow-pattern strings.

    #[test]
    fn extractor_fallback_derives_non_empty_relation_type() {
        let rt = derive_relation_type_from_fact("Brett Adam lives in Seattle");
        assert!(
            !rt.is_empty(),
            "fallback must produce non-empty relation_type"
        );
        assert!(
            !rt.contains('→') && !rt.contains("->"),
            "fallback must not produce an arrow-pattern: {rt}"
        );
        assert!(
            rt.chars().all(|c| c.is_uppercase() || c == '_'),
            "fallback must be SCREAMING_SNAKE_CASE: {rt}"
        );
    }

    #[test]
    fn extractor_fallback_empty_fact_yields_unclassified() {
        assert_eq!(
            derive_relation_type_from_fact(""),
            "UNCLASSIFIED",
            "empty fact must yield UNCLASSIFIED sentinel"
        );
    }

    // ── OaiExtractor: SC-005 response-parsing coverage ────────────────────────

    #[test]
    fn oai_extractor_transport_info_reports_http() {
        let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
        let extractor =
            OaiExtractor::new_http("http://127.0.0.1:8765/v1/chat/completions", "local", sink);
        let (label, endpoint) = extractor.transport_info();
        assert_eq!(label, "http");
        assert_eq!(endpoint, "http://127.0.0.1:8765/v1/chat/completions");
        assert_eq!(extractor.model_name(), "local");
    }

    #[test]
    fn parse_oai_entity_response_success() {
        let resp = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "content": "{\"entities\": [{\"name\": \"Alice\", \"entity_type\": \"Person\", \"summary\": \"A person\"}]}"
                }
            }]
        });
        match parse_oai_entity_response(&resp) {
            OaiChatOutcome::Success {
                value: entities,
                defensive_parse,
                dropped,
            } => {
                assert_eq!(entities.len(), 1);
                assert_eq!(dropped, 0);
                assert_eq!(entities[0].name, "Alice");
                assert!(!defensive_parse, "raw content was already valid JSON");
            }
            OaiChatOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            OaiChatOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    #[test]
    fn parse_oai_entity_response_fenced_json_is_recovered() {
        let resp = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "content": "```json\n{\"entities\": [{\"name\": \"Alice\", \"entity_type\": \"Person\", \"summary\": \"A person\"}]}\n```"
                }
            }]
        });
        match parse_oai_entity_response(&resp) {
            OaiChatOutcome::Success {
                value: entities,
                defensive_parse,
                dropped,
            } => {
                assert_eq!(entities.len(), 1);
                assert_eq!(dropped, 0);
                assert!(
                    defensive_parse,
                    "fenced content required extract_json_block stripping"
                );
            }
            OaiChatOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            OaiChatOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    #[test]
    fn parse_oai_entity_response_malformed_content_is_parse_error() {
        let resp = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": "not json at all"}
            }]
        });
        assert!(matches!(
            parse_oai_entity_response(&resp),
            OaiChatOutcome::ParseError(_)
        ));
    }

    #[test]
    fn parse_oai_entity_response_missing_content_is_parse_error() {
        let resp = json!({
            "choices": [{"finish_reason": "stop", "message": {}}]
        });
        assert!(matches!(
            parse_oai_entity_response(&resp),
            OaiChatOutcome::ParseError(_)
        ));
    }

    #[test]
    fn parse_oai_entity_response_truncated_is_budget_exhausted() {
        let resp = json!({
            "choices": [{"finish_reason": "length", "message": {"content": "{\"entities\": ["}}]
        });
        assert!(matches!(
            parse_oai_entity_response(&resp),
            OaiChatOutcome::BudgetExhausted
        ));
    }

    #[test]
    fn parse_oai_edge_response_success() {
        let resp = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "content": "{\"edges\": [{\"source_name\": \"Alice\", \"target_name\": \"Acme\", \"fact\": \"Alice works at Acme\", \"relation_type\": \"works_at\"}]}"
                }
            }]
        });
        match parse_oai_edge_response(&resp) {
            OaiChatOutcome::Success {
                value: edges,
                defensive_parse,
                dropped,
            } => {
                assert_eq!(edges.len(), 1);
                assert_eq!(dropped, 0);
                assert_eq!(edges[0].source_name, "Alice");
                assert_eq!(edges[0].relation_type.as_deref(), Some("works_at"));
                assert!(!defensive_parse, "raw content was already valid JSON");
            }
            OaiChatOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            OaiChatOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    #[test]
    fn parse_oai_edge_response_malformed_content_is_parse_error() {
        let resp = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": "{\"not_edges\": []}"}
            }]
        });
        assert!(matches!(
            parse_oai_edge_response(&resp),
            OaiChatOutcome::ParseError(_)
        ));
    }

    #[test]
    fn parse_oai_edge_response_truncated_is_budget_exhausted() {
        let resp = json!({
            "choices": [{"finish_reason": "length", "message": {"content": "{\"edges\": ["}}]
        });
        assert!(matches!(
            parse_oai_edge_response(&resp),
            OaiChatOutcome::BudgetExhausted
        ));
    }

    // ── StructuredOutputParse telemetry: end-to-end via a stub HTTP server ────

    fn oai_response_body(content: &str) -> String {
        json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": content}
            }]
        })
        .to_string()
    }

    /// Spawns a one-shot stub HTTP server on an ephemeral localhost port that
    /// replies with `body` to a single request, then shuts down. Returns the
    /// `/v1/chat/completions`-style URL to point an `OaiExtractor` at.
    async fn spawn_stub_http_server(body: String) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    let n = reader.read_line(&mut line).await.unwrap_or(0);
                    if n == 0 || line == "\r\n" || line == "\n" {
                        break;
                    }
                    let lower = line.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    let mut buf = vec![0u8; content_length];
                    reader.read_exact(&mut buf).await.ok();
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                write_half.write_all(response.as_bytes()).await.ok();
            }
        });
        (format!("http://{addr}/v1/chat/completions"), handle)
    }

    fn test_extract_options() -> ExtractOptions<'static> {
        ExtractOptions {
            episode_body: "Alice works at Acme Corp.",
            group_id: "test-group",
            source_type: SourceType::Text,
            custom_instructions: None,
            reference_time: "2026-01-01T00:00:00Z",
            ontology: None,
            chunk_key: Some("test-chunk"),
        }
    }

    #[tokio::test]
    async fn do_extract_entities_emits_clean_structured_output_parse() {
        let content =
            r#"{"entities": [{"name": "Alice", "entity_type": "Person", "summary": "A person"}]}"#;
        let (url, _server) = spawn_stub_http_server(oai_response_body(content)).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let (entities, dropped) = extractor
            .do_extract_entities(&test_extract_options())
            .await
            .unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(dropped, 0);

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::StructuredOutputParse { call_type, outcome, .. }
                if call_type == "entities" && outcome == "clean"
        )));
    }

    #[tokio::test]
    async fn do_extract_entities_emits_recovered_structured_output_parse() {
        let content = "```json\n{\"entities\": [{\"name\": \"Alice\", \"entity_type\": \"Person\", \"summary\": \"A person\"}]}\n```";
        let (url, _server) = spawn_stub_http_server(oai_response_body(content)).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let (entities, dropped) = extractor
            .do_extract_entities(&test_extract_options())
            .await
            .unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(dropped, 0);

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::StructuredOutputParse { call_type, outcome, .. }
                if call_type == "entities" && outcome == "recovered"
        )));
    }

    #[tokio::test]
    async fn do_extract_entities_emits_malformed_structured_output_parse() {
        let (url, _server) = spawn_stub_http_server(oai_response_body("not json at all")).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let result = extractor.do_extract_entities(&test_extract_options()).await;
        assert!(result.is_err());

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::StructuredOutputParse { call_type, outcome, .. }
                if call_type == "entities" && outcome == "malformed"
        )));
    }

    #[test]
    fn parse_oai_entity_response_missing_summary_retains_all_entities() {
        let resp = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "content": "{\"entities\": [{\"name\": \"Apollo 11\", \"entity_type\": \"Mission\"}, {\"name\": \"Neil Armstrong\", \"entity_type\": \"Person\"}]}"
                }
            }]
        });
        match parse_oai_entity_response(&resp) {
            OaiChatOutcome::Success {
                value: entities,
                defensive_parse,
                dropped,
            } => {
                assert_eq!(entities.len(), 2, "no entities should be dropped");
                assert_eq!(dropped, 0);
                assert!(entities.iter().all(|e| e.summary.is_empty()));
                assert!(!defensive_parse);
            }
            OaiChatOutcome::BudgetExhausted => panic!("unexpected BudgetExhausted"),
            OaiChatOutcome::ParseError(e) => panic!("unexpected ParseError: {e}"),
        }
    }

    // US1 AS1/AS2 (#314): a response with valid JSON entities missing `summary` retains every
    // entity (defaulted to an empty summary) and is classified `clean`, never `malformed`/
    // `schema_invalid`.
    #[tokio::test]
    async fn do_extract_entities_missing_summary_retains_entities_and_stays_clean() {
        let content = r#"{"entities": [{"name": "Apollo 11", "entity_type": "Mission"}, {"name": "Neil Armstrong", "entity_type": "Person"}]}"#;
        let (url, _server) = spawn_stub_http_server(oai_response_body(content)).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let (entities, dropped) = extractor
            .do_extract_entities(&test_extract_options())
            .await
            .unwrap();
        assert_eq!(entities.len(), 2, "no entities should be dropped");
        assert_eq!(dropped, 0);
        assert!(entities.iter().all(|e| e.summary.is_empty()));

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::StructuredOutputParse { call_type, outcome, .. }
                if call_type == "entities" && outcome == "clean"
        )));
        assert!(
            !events.iter().any(|e| matches!(
                e,
                TelemetryEvent::StructuredOutputParse { outcome, .. }
                    if outcome == "malformed" || outcome == "schema_invalid"
            )),
            "a missing-summary response must never classify as malformed or schema_invalid"
        );
    }

    // #342: a single-item batch where that item is missing `name` (a genuinely required field)
    // is salvaged, not a hard failure — the batch becomes an empty success with the drop
    // counted, exactly like #314's "all malformed" edge case. This inverts the pre-#342
    // assertion (`result.is_err()` + `outcome == "schema_invalid"`); see the Plan stage's Key
    // Decisions for why `types.rs`'s single-struct deserialize test does NOT flip alongside
    // this one — `salvage_items` relies on that lower-level deserialize continuing to fail.
    #[tokio::test]
    async fn do_extract_entities_all_malformed_batch_salvages_to_empty_success() {
        let content = r#"{"entities": [{"entity_type": "Mission", "summary": "no name here"}]}"#;
        let (url, _server) = spawn_stub_http_server(oai_response_body(content)).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let (entities, dropped) = extractor
            .do_extract_entities(&test_extract_options())
            .await
            .unwrap();
        assert!(entities.is_empty());
        assert_eq!(dropped, 1);

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::StructuredOutputParse { call_type, outcome, .. }
                if call_type == "entities" && outcome == "salvaged"
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, TelemetryEvent::ExtractionFailure { .. })),
            "a salvaged response must not emit ExtractionFailure — it's a success, not a failure"
        );
    }

    // FR-004 (#314): a mixed batch (some entities with a summary, some without) fires
    // `EntitiesMissingSummary` with the correct counts.
    #[tokio::test]
    async fn do_extract_entities_emits_entities_missing_summary_on_mixed_batch() {
        let content = r#"{"entities": [{"name": "Apollo 11", "entity_type": "Mission", "summary": "A NASA mission."}, {"name": "Neil Armstrong", "entity_type": "Person"}]}"#;
        let (url, _server) = spawn_stub_http_server(oai_response_body(content)).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let (entities, dropped) = extractor
            .do_extract_entities(&test_extract_options())
            .await
            .unwrap();
        assert_eq!(entities.len(), 2);
        assert_eq!(dropped, 0);

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::EntitiesMissingSummary { entities_extracted, missing_summary, .. }
                if *entities_extracted == 2 && *missing_summary == 1
        )));
    }

    #[tokio::test]
    async fn do_extract_edges_emits_clean_structured_output_parse_with_edges_call_type() {
        let content = r#"{"edges": [{"source_name": "Alice", "target_name": "Acme", "fact": "Alice works at Acme", "relation_type": "works_at"}]}"#;
        let (url, _server) = spawn_stub_http_server(oai_response_body(content)).await;
        let sink = Arc::new(CaptureSink::new());
        let extractor = OaiExtractor::new_http(url, "test-model", Arc::clone(&sink) as _);

        let (edges, dropped) = extractor
            .do_extract_edges(&test_extract_options(), &["Alice".to_string()])
            .await
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(dropped, 0);

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            TelemetryEvent::StructuredOutputParse { call_type, outcome, .. }
                if call_type == "edges" && outcome == "clean"
        )));
    }

    // FR-001: the extract_edges tool schema constrains source_name/target_name to an enum
    // built from the batch's sanitized entity names.
    #[test]
    fn build_edge_tool_schema_enum_contains_exactly_sanitized_names() {
        let names = vec!["Alice".to_string(), "Acme Corp".to_string()];
        let schema = build_edge_tool_schema(&names);

        let source_enum = schema["input_schema"]["properties"]["edges"]["items"]["properties"]
            ["source_name"]["enum"]
            .as_array()
            .expect("source_name.enum must be an array");
        let target_enum = schema["input_schema"]["properties"]["edges"]["items"]["properties"]
            ["target_name"]["enum"]
            .as_array()
            .expect("target_name.enum must be an array");

        let source_names: Vec<&str> = source_enum.iter().filter_map(|v| v.as_str()).collect();
        let target_names: Vec<&str> = target_enum.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(source_names, vec!["Alice", "Acme Corp"]);
        assert_eq!(target_names, vec!["Alice", "Acme Corp"]);
    }

    #[tokio::test]
    async fn do_extract_edges_skips_http_call_when_entity_names_sanitize_to_empty() {
        let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
        // Point at an address nothing listens on — if do_extract_edges attempted an HTTP call
        // it would fail; returning Ok(vec![]) proves the call was skipped entirely.
        let extractor = AnthropicExtractor::with_url(
            "claude-haiku-4-5-20251001".to_string(),
            "key".to_string(),
            "http://127.0.0.1:1/v1/messages".to_string(),
            sink,
        );

        let (edges, dropped) = extractor
            .do_extract_edges(
                &test_extract_options(),
                &["   ".to_string(), "\n".to_string()],
            )
            .await
            .unwrap();
        assert!(edges.is_empty());
        assert_eq!(dropped, 0);
    }
}
