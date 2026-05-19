use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;

use crate::{
    error::Error,
    extractor::{AnthropicExtractor, Extractor},
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
    types::ExtractionResult,
};

/// Routes extraction calls to a primary LLM with optional fallback (AD-5).
///
/// Parses `GRAPHITI_EXTRACTION_LLM` on `:` — first token is the primary model,
/// second (optional) is the fallback model. Both use the same `ANTHROPIC_API_KEY`.
///
/// When a fallback is configured: on the first primary failure, emits
/// `TelemetryEvent::LlmFallback` once and routes all subsequent calls to the
/// fallback for the rest of the process lifetime.
///
/// When no fallback is configured: primary errors are returned to the caller
/// without latching `primary_failed`, so transient failures do not permanently
/// disable extraction.
pub struct LlmRouter {
    primary: AnthropicExtractor,
    primary_model_name: String,
    fallback: Option<AnthropicExtractor>,
    fallback_model_name: String,
    primary_failed: AtomicBool,
    sink: Arc<dyn TelemetrySink>,
}

impl LlmRouter {
    /// Constructs directly from extractor instances — for tests.
    pub fn new(
        primary: AnthropicExtractor,
        fallback: Option<AnthropicExtractor>,
        sink: Arc<dyn TelemetrySink>,
    ) -> Self {
        let primary_model_name = primary.model_name().to_string();
        let fallback_model_name = fallback.as_ref().map(|f| f.model_name().to_string()).unwrap_or_default();
        Self {
            primary,
            primary_model_name,
            fallback,
            fallback_model_name,
            primary_failed: AtomicBool::new(false),
            sink,
        }
    }

    pub fn from_env(sink: Arc<dyn TelemetrySink>) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let spec = std::env::var("GRAPHITI_EXTRACTION_LLM")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());

        let mut parts = spec.splitn(2, ':');
        let primary_model = parts.next().unwrap_or("claude-haiku-4-5-20251001").to_string();
        let fallback_model = parts.next().map(str::to_string);

        let primary = AnthropicExtractor::with_model(
            primary_model.clone(),
            api_key.clone(),
            Arc::clone(&sink),
        );
        let fallback_model_name = fallback_model.clone().unwrap_or_default();
        let fallback = fallback_model
            .map(|m| AnthropicExtractor::with_model(m, api_key, Arc::clone(&sink)));

        Self {
            primary,
            primary_model_name: primary_model,
            fallback,
            fallback_model_name,
            primary_failed: AtomicBool::new(false),
            sink,
        }
    }

    async fn do_extract(&self, body: &str, group_id: &str) -> Result<ExtractionResult, Error> {
        // primary_failed is only ever set to true when a fallback is configured, so if it is
        // true here there must be a fallback to try.
        if !self.primary_failed.load(Ordering::Acquire) {
            match self.primary.extract(body, group_id).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    if let Some(fb) = &self.fallback {
                        // Redirect to fallback and log the transition exactly once per session.
                        if self
                            .primary_failed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            self.sink.emit(TelemetryEvent::LlmFallback {
                                ts_ms: now_ms(),
                                role: "extraction".to_string(),
                                primary_model: self.primary_model_name.clone(),
                                fallback_model: self.fallback_model_name.clone(),
                                error_reason: err.to_string(),
                            });
                            eprintln!(
                                "liminis-graph: extraction primary '{}' failed ({}); switching to fallback '{}' for this session",
                                self.primary_model_name, err, self.fallback_model_name
                            );
                        }
                        return fb.extract(body, group_id).await;
                    }
                    // No fallback — return the error without setting primary_failed so that
                    // transient failures do not permanently disable the primary.
                    return Err(err);
                }
            }
        }

        // primary_failed is true, which means a fallback is configured.
        if let Some(fb) = &self.fallback {
            fb.extract(body, group_id).await
        } else {
            // Unreachable: primary_failed is only set when fallback.is_some().
            Err(Error::Ipc("BUG: primary_failed set without fallback".to_string()))
        }
    }
}

impl Extractor for LlmRouter {
    fn extract<'a>(
        &'a self,
        body: &'a str,
        group_id: &'a str,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(self.do_extract(body, group_id))
    }
}
