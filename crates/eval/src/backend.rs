//! Parses a `--backend NAME=SPEC` argument's `SPEC` half into a configured
//! `Arc<dyn Extractor>`, reusing `AnthropicExtractor`/`OaiExtractor` verbatim (FR-003) —
//! this module contains no HTTP/JSON client logic of its own.

use std::collections::HashMap;
use std::sync::Arc;

use lcg_core::{AnthropicExtractor, Extractor, OaiExtractor, TelemetrySink};

#[derive(Debug, Clone, PartialEq)]
pub enum BackendKind {
    Anthropic { model: Option<String> },
    OaiHttp { url: String, model: Option<String> },
    OaiUds { path: String, model: Option<String> },
}

/// Parses a backend spec of the form `anthropic[:model=<M>]`,
/// `oai-http:url=<URL>[,model=<M>]`, or `oai-uds:path=<PATH>[,model=<M>]`.
pub fn parse_backend_spec(spec: &str) -> Result<BackendKind, String> {
    let (kind, rest) = match spec.split_once(':') {
        Some((k, r)) => (k, Some(r)),
        None => (spec, None),
    };

    let params: HashMap<String, String> = rest
        .map(|r| {
            r.split(',')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();

    match kind {
        "anthropic" => Ok(BackendKind::Anthropic {
            model: params.get("model").cloned(),
        }),
        "oai-http" => {
            let url = params
                .get("url")
                .cloned()
                .ok_or_else(|| "oai-http backend requires url=<URL>".to_string())?;
            Ok(BackendKind::OaiHttp {
                url,
                model: params.get("model").cloned(),
            })
        }
        "oai-uds" => {
            let path = params
                .get("path")
                .cloned()
                .ok_or_else(|| "oai-uds backend requires path=<PATH>".to_string())?;
            Ok(BackendKind::OaiUds {
                path,
                model: params.get("model").cloned(),
            })
        }
        other => Err(format!(
            "unknown backend kind '{other}' (expected anthropic, oai-http, or oai-uds)"
        )),
    }
}

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5-20251001";
const DEFAULT_LOCAL_MODEL: &str = "local";

/// Builds a live extractor for `kind`. Every call gets its own `Arc<dyn TelemetrySink>` so
/// callers can capture per-backend `StructuredOutputParse` telemetry independently.
pub fn build_extractor(
    kind: &BackendKind,
    sink: Arc<dyn TelemetrySink>,
) -> Result<Arc<dyn Extractor>, String> {
    match kind {
        BackendKind::Anthropic { model } => {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                "ANTHROPIC_API_KEY must be set for an anthropic backend".to_string()
            })?;
            let model = model
                .clone()
                .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string());
            Ok(Arc::new(AnthropicExtractor::with_model(
                model, api_key, sink,
            )))
        }
        BackendKind::OaiHttp { url, model } => {
            let model = model
                .clone()
                .unwrap_or_else(|| DEFAULT_LOCAL_MODEL.to_string());
            Ok(Arc::new(OaiExtractor::new_http(url.clone(), model, sink)))
        }
        BackendKind::OaiUds { path, model } => {
            #[cfg(unix)]
            {
                let model = model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_LOCAL_MODEL.to_string());
                Ok(Arc::new(OaiExtractor::new_uds(path.clone(), model, sink)))
            }
            #[cfg(not(unix))]
            {
                let _ = (path, model, sink);
                Err("oai-uds backend is only supported on unix platforms".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcg_core::NoopSink;

    #[test]
    fn parses_bare_anthropic() {
        assert_eq!(
            parse_backend_spec("anthropic").unwrap(),
            BackendKind::Anthropic { model: None }
        );
    }

    #[test]
    fn parses_anthropic_with_model() {
        assert_eq!(
            parse_backend_spec("anthropic:model=claude-sonnet-4-6").unwrap(),
            BackendKind::Anthropic {
                model: Some("claude-sonnet-4-6".to_string())
            }
        );
    }

    #[test]
    fn parses_oai_http_with_url_and_model() {
        assert_eq!(
            parse_backend_spec(
                "oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=local"
            )
            .unwrap(),
            BackendKind::OaiHttp {
                url: "http://127.0.0.1:8765/v1/chat/completions".to_string(),
                model: Some("local".to_string())
            }
        );
    }

    #[test]
    fn oai_http_without_url_is_rejected() {
        let err = parse_backend_spec("oai-http:model=local").unwrap_err();
        assert!(err.contains("url="));
    }

    #[test]
    fn parses_oai_uds_with_path() {
        assert_eq!(
            parse_backend_spec("oai-uds:path=/tmp/x.sock").unwrap(),
            BackendKind::OaiUds {
                path: "/tmp/x.sock".to_string(),
                model: None
            }
        );
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = parse_backend_spec("bogus").unwrap_err();
        assert!(err.contains("unknown backend kind"));
    }

    #[test]
    fn build_anthropic_without_api_key_env_is_rejected() {
        // SAFETY: test-only env manipulation, single-threaded within this test's scope
        // is not guaranteed across the whole suite, so only assert the Err variant
        // when the var happens to be unset; skip otherwise to avoid cross-test flakiness.
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let kind = BackendKind::Anthropic { model: None };
        match build_extractor(&kind, Arc::new(NoopSink)) {
            Err(err) => assert!(err.contains("ANTHROPIC_API_KEY")),
            Ok(_) => panic!("expected an error without ANTHROPIC_API_KEY set"),
        }
    }

    #[test]
    fn build_oai_http_succeeds_without_network() {
        let kind = BackendKind::OaiHttp {
            url: "http://127.0.0.1:1/v1/chat/completions".to_string(),
            model: Some("local".to_string()),
        };
        assert!(build_extractor(&kind, Arc::new(NoopSink)).is_ok());
    }
}
