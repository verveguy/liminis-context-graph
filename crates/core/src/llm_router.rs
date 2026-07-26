use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::{
    env::lcg_env_var,
    error::Error,
    extractor::{AnthropicExtractor, ExtractOptions, Extractor},
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
    types::ExtractionResult,
};

/// Routes extraction calls to a primary LLM with optional fallback (AD-5).
///
/// Parses `LCG_EXTRACTION_LLM` on `:` — first token is the primary model,
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
    primary: Arc<dyn Extractor>,
    primary_model_name: String,
    fallback: Option<Arc<dyn Extractor>>,
    fallback_model_name: String,
    primary_failed: AtomicBool,
    sink: Arc<dyn TelemetrySink>,
}

impl LlmRouter {
    /// Constructs directly from extractor instances and their model names — for tests and for
    /// callers that already hold a concrete `Arc<dyn Extractor>` (e.g. `OaiExtractor` as
    /// primary with no fallback). Model names are passed explicitly rather than derived via a
    /// trait method, since callers already know the string at construction time and not every
    /// `Extractor` impl (test doubles included) has a meaningful model name to report.
    pub fn new(
        primary: Arc<dyn Extractor>,
        primary_model_name: String,
        fallback: Option<Arc<dyn Extractor>>,
        fallback_model_name: String,
        sink: Arc<dyn TelemetrySink>,
    ) -> Self {
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
        Self::from_env_with(sink, |extractor, _model_name| extractor)
    }

    /// Like [`Self::from_env`], but passes each constructed leaf extractor (primary, and
    /// fallback if configured) through `wrap` — along with its model name — before storing it.
    /// `from_env` delegates here with an identity closure, so its behavior is byte-for-byte
    /// unchanged; this seam exists so a caller (e.g. `main.rs` under `LCG_RECORD_LLM`) can wrap
    /// each leaf in a `RecordingExtractor` individually, producing distinguishable,
    /// correctly-attributed cassette entries on primary→fallback failover (#232 User Story 4)
    /// without duplicating this function's `LCG_EXTRACTION_LLM` parsing logic elsewhere.
    pub fn from_env_with(
        sink: Arc<dyn TelemetrySink>,
        wrap: impl Fn(Arc<dyn Extractor>, &str) -> Arc<dyn Extractor>,
    ) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        // deprecated: remove in Phase B (see #59)
        let spec = lcg_env_var("LCG_EXTRACTION_LLM", "GRAPHITI_EXTRACTION_LLM")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());

        let mut parts = spec.splitn(2, ':');
        let primary_model = parts
            .next()
            .unwrap_or("claude-haiku-4-5-20251001")
            .to_string();
        let fallback_model = parts.next().map(str::to_string);

        let primary: Arc<dyn Extractor> = wrap(
            Arc::new(AnthropicExtractor::with_model(
                primary_model.clone(),
                api_key.clone(),
                Arc::clone(&sink),
            )),
            &primary_model,
        );
        let fallback_model_name = fallback_model.clone().unwrap_or_default();
        let fallback: Option<Arc<dyn Extractor>> = fallback_model.map(|m| {
            wrap(
                Arc::new(AnthropicExtractor::with_model(
                    m.clone(),
                    api_key,
                    Arc::clone(&sink),
                )),
                &m,
            )
        });

        Self {
            primary,
            primary_model_name: primary_model,
            fallback,
            fallback_model_name,
            primary_failed: AtomicBool::new(false),
            sink,
        }
    }

    async fn do_classify_entities(
        &self,
        entities: &[(&str, &str)],
        allowed_types: Option<&[String]>,
    ) -> Result<Vec<String>, Error> {
        if !self
            .primary_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            match self
                .primary
                .classify_entities(entities, allowed_types)
                .await
            {
                Ok(result) => return Ok(result),
                Err(err) => {
                    if let Some(fb) = &self.fallback {
                        if self
                            .primary_failed
                            .compare_exchange(
                                false,
                                true,
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                            )
                            .is_ok()
                        {
                            self.sink.emit(TelemetryEvent::LlmFallback {
                                ts_ms: now_ms(),
                                role: "classification".to_string(),
                                primary_model: self.primary_model_name.clone(),
                                fallback_model: self.fallback_model_name.clone(),
                                error_reason: err.to_string(),
                            });
                        }
                        return fb.classify_entities(entities, allowed_types).await;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(fb) = &self.fallback {
            fb.classify_entities(entities, allowed_types).await
        } else {
            Err(Error::Ipc(
                "BUG: primary_failed set without fallback".to_string(),
            ))
        }
    }

    async fn do_classify_relations(
        &self,
        edges: &[(&str, &str)],
        allowed_types: &[(String, Option<String>)],
    ) -> Result<Vec<String>, Error> {
        if !self
            .primary_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            match self.primary.classify_relations(edges, allowed_types).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    if let Some(fb) = &self.fallback {
                        if self
                            .primary_failed
                            .compare_exchange(
                                false,
                                true,
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                            )
                            .is_ok()
                        {
                            self.sink.emit(TelemetryEvent::LlmFallback {
                                ts_ms: now_ms(),
                                role: "relation_classification".to_string(),
                                primary_model: self.primary_model_name.clone(),
                                fallback_model: self.fallback_model_name.clone(),
                                error_reason: err.to_string(),
                            });
                        }
                        return fb.classify_relations(edges, allowed_types).await;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(fb) = &self.fallback {
            fb.classify_relations(edges, allowed_types).await
        } else {
            Err(Error::Ipc(
                "BUG: primary_failed set without fallback".to_string(),
            ))
        }
    }

    async fn do_extract(&self, opts: ExtractOptions<'_>) -> Result<ExtractionResult, Error> {
        // primary_failed is only ever set to true when a fallback is configured, so if it is
        // true here there must be a fallback to try.
        if !self.primary_failed.load(Ordering::Acquire) {
            match self.primary.extract(opts).await {
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
                                "liminis-context-graph: extraction primary '{}' failed ({}); switching to fallback '{}' for this session",
                                self.primary_model_name, err, self.fallback_model_name
                            );
                        }
                        return fb.extract(opts).await;
                    }
                    // No fallback — return the error without setting primary_failed so that
                    // transient failures do not permanently disable the primary.
                    return Err(err);
                }
            }
        }

        // primary_failed is true, which means a fallback is configured.
        if let Some(fb) = &self.fallback {
            fb.extract(opts).await
        } else {
            // Unreachable: primary_failed is only set when fallback.is_some().
            Err(Error::Ipc(
                "BUG: primary_failed set without fallback".to_string(),
            ))
        }
    }
}

impl Extractor for LlmRouter {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
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
