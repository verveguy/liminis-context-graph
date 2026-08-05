//! LLM cassette record/replay decorators for the [`Extractor`] trait (#232).
//!
//! [`RecordingExtractor`] wraps a real extractor, forwards every call unchanged, and appends
//! one JSONL record per call to a [`CassetteWriter`]. [`ReplayingExtractor`] holds no inner
//! extractor at all — it serves calls purely from a cassette file loaded into memory, making
//! "no network access" (FR-002) true by construction rather than by discipline.
//!
//! `RecordingExtractor::extract` only ever records on success (its `?` on the inner call
//! returns before `record()` runs) — a failed call never produces a cassette entry, by design
//! (#306 FR-007). Failure data (the complete raw response body, `finish_reason`,
//! `completion_tokens`, and a classification) goes to a separate sidecar instead — see
//! [`crate::extraction_failures`] and ADR-0306 — written from inside `AnthropicExtractor`/
//! `OaiExtractor` themselves via `TelemetryEvent::ExtractionFailure`, a layer this module's
//! trait-boundary-only seam (ADR-0044) cannot see.
//!
//! # What is (and isn't) in the matching hash — FR-005
//!
//! Cassette records are matched by a SHA-256 hash of a canonical JSON value built from the
//! call's *semantic* content. What's included, per [`Extractor`] method:
//!
//! - **`extract`**: `source_type`, `group_id`, `reference_time`, `custom_instructions`,
//!   `episode_body`, plus the **rendered** entity system/user prompts and edge system prompt
//!   (via [`prompts::entity_system_prompt`], [`prompts::entity_user_prompt_for`],
//!   [`prompts::edge_system_prompt`]). Rendering the prompts — rather than hashing raw
//!   `ExtractOptions` fields alone — means an edit to a prompt template file (e.g.
//!   `extract_text.txt`) or to the injected ontology correctly invalidates stale cassette
//!   entries, surfacing as a loud [`Error::CassetteMiss`] instead of a silent divergence.
//!   **Known gap**: the edge *user* prompt (`prompts::edge_user_prompt`) cannot be rendered
//!   ahead of time because it needs `entity_names`, which only exists inside the wrapped
//!   extractor's own entity-extraction call — a template-only edit to that function's own text
//!   (not touching entity names or episode content) will not invalidate the cassette. See
//!   ADR-0044.
//! - **`classify_entities` / `classify_relations`**: the raw call arguments only (entities or
//!   edges, and the allowed-types list). Neither `AnthropicExtractor` nor `OaiExtractor` renders
//!   these classification prompts through a shared, extractable function the way `extract`'s
//!   prompts are shared — each builds its own inline system text — so there is no
//!   provider-agnostic prompt text to fold into the hash. A wording-only change to one
//!   extractor's inline classification prompt will not invalidate a cassette recorded against
//!   the other wording.
//!
//! Explicitly **excluded** from every hash: wall-clock timestamps, request nonces/IDs, and
//! anything provider/transport-specific (headers, API keys, URLs) — none of which ever reach
//! this module, since the decorator operates at the [`Extractor`] trait boundary, strictly
//! above HTTP request construction. This is also what satisfies FR-008: cassette records can
//! never contain credentials, because nothing at this boundary carries them.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    error::Error,
    extractor::{ExtractOptions, Extractor},
    prompts,
    types::ExtractionOutcome,
};

// ── Canonical request keys ──────────────────────────────────────────────────

fn request_key(value: &Value) -> String {
    // `serde_json` is built with the `preserve_order` feature (see workspace Cargo.toml), so
    // object keys serialize in insertion order rather than an unspecified hash order. Every
    // value built below inserts keys in a fixed, hand-written order, so this serialization is
    // deterministic across processes and runs — no separate canonicalization pass is needed.
    let canonical = serde_json::to_string(value).expect("Value serialization is infallible");
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{digest:x}")
}

fn extract_request_value(opts: &ExtractOptions<'_>) -> Value {
    let entity_system_prompt = prompts::entity_system_prompt(opts.source_type, opts.ontology);
    let entity_user_prompt = prompts::entity_user_prompt_for(
        opts.source_type,
        opts.episode_body,
        opts.custom_instructions,
    );
    let edge_system_prompt = prompts::edge_system_prompt(opts.ontology);
    json!({
        "call_type": "extract",
        "source_type": format!("{:?}", opts.source_type),
        "group_id": opts.group_id,
        "reference_time": opts.reference_time,
        "custom_instructions": opts.custom_instructions,
        "episode_body": opts.episode_body,
        "entity_system_prompt": entity_system_prompt,
        "entity_user_prompt": entity_user_prompt,
        "edge_system_prompt": edge_system_prompt,
    })
}

fn classify_entities_request_value(
    entities: &[(&str, &str)],
    allowed_types: Option<&[String]>,
) -> Value {
    let entities_json: Vec<Value> = entities
        .iter()
        .map(|(name, summary)| json!({"name": name, "summary": summary}))
        .collect();
    json!({
        "call_type": "classify_entities",
        "entities": entities_json,
        "allowed_types": allowed_types,
    })
}

fn classify_relations_request_value(
    edges: &[(&str, &str)],
    allowed_types: &[(String, Option<String>)],
) -> Value {
    let edges_json: Vec<Value> = edges
        .iter()
        .map(|(fact, current_type)| json!({"fact": fact, "current_type": current_type}))
        .collect();
    json!({
        "call_type": "classify_relations",
        "edges": edges_json,
        "allowed_types": allowed_types,
    })
}

// ── CassetteRecord ───────────────────────────────────────────────────────────

/// One JSONL line: a single recorded extraction exchange plus the metadata needed to interpret
/// and re-match it (FR-001/FR-009).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CassetteRecord {
    /// SHA-256 hex digest of the canonical request value — see the module doc for scope.
    pub key: String,
    /// Which `Extractor` method produced this record: `"extract"`, `"classify_entities"`, or
    /// `"classify_relations"`.
    pub call_type: String,
    pub provider: String,
    pub model: String,
    /// RFC 3339 timestamp of the recording call. Informational only — never part of `key`.
    pub timestamp: String,
    /// Human-readable request content (the same value the hash was computed over).
    pub request: Value,
    /// The call's return value, serialized (`ExtractionResult` for `extract`, `Vec<String>` for
    /// the two classify methods).
    pub response: Value,
}

/// Parses a cassette file into its raw records, rejecting corruption and duplicate keys
/// rather than tolerating either (#279 FR-002/FR-003). Shared by [`ReplayingExtractor::load`]
/// and, in `lcg_eval`, by the `--dry-run`/guard resolution path — one implementation of
/// "what is a valid cassette line," used twice instead of twice over.
///
/// Deliberately NOT used for the post-report cassette-count check (FR-006,
/// `lcg_eval::report::validate_recorded_cassette`) — see [`count_records`] for why that
/// check needs a different, more permissive function.
///
/// Malformed JSON, a non-object record, a record missing `key`, and a record whose `key` is
/// not a string are all reported as [`Error::CassetteCorrupt`]. A repeated `key` is reported
/// as [`Error::CassetteDuplicateKey`], naming the file, the key, and both line numbers — a
/// distinct diagnosis from corruption, not just a distinct exit code.
pub fn load_records(path: impl AsRef<Path>) -> Result<Vec<CassetteRecord>, Error> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|e| {
        Error::CassetteCorrupt(format!("failed to read cassette {}: {e}", path.display()))
    })?;

    let mut records = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, line) in content.lines().enumerate() {
        let line_no = i + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            Error::CassetteCorrupt(format!(
                "cassette {} line {line_no}: invalid JSON: {e}",
                path.display()
            ))
        })?;
        if !value.is_object() {
            return Err(Error::CassetteCorrupt(format!(
                "cassette {} line {line_no}: record is not an object",
                path.display()
            )));
        }
        match value.get("key") {
            Some(Value::String(_)) => {}
            Some(_) => {
                return Err(Error::CassetteCorrupt(format!(
                    "cassette {} line {line_no}: \"key\" is not a string",
                    path.display()
                )))
            }
            None => {
                return Err(Error::CassetteCorrupt(format!(
                    "cassette {} line {line_no}: record has no \"key\" field",
                    path.display()
                )))
            }
        }
        let record: CassetteRecord = serde_json::from_value(value).map_err(|e| {
            Error::CassetteCorrupt(format!("cassette {} line {line_no}: {e}", path.display()))
        })?;
        if let Some(first_line) = seen.insert(record.key.clone(), line_no) {
            return Err(Error::CassetteDuplicateKey(format!(
                "cassette {} line {line_no}: key '{}' duplicates the one at line {first_line}",
                path.display(),
                record.key
            )));
        }
        records.push(record);
    }
    Ok(records)
}

/// Counts the non-blank lines in a cassette file — no JSON parsing, no uniqueness check.
///
/// Used by `lcg_eval::report::validate_recorded_cassette` (FR-006) to check that a
/// `RecordingExtractor` write completed, which deliberately must NOT go through
/// [`load_records`]: `classify_entities`/`classify_relations` cassette keys hash only the
/// extracted entity/edge content, not chunk identity (`request_key` over
/// `classify_entities_request_value`/`classify_relations_request_value`), so two distinct
/// chunks that legitimately extract the same content — most commonly two chunks that both
/// extract nothing — produce the same key. `load_records` correctly rejects that as
/// unreplayable (FR-002), but rejecting it *here* would abort a live `--record-cassette` run
/// and discard its already-computed, already-paid-for report over a future-replay concern
/// that has nothing to do with whether this run's capture completed (handarbeit-pruefer
/// review finding on PR #280). A cassette this flags as complete may still fail
/// `load_records` later, at the point where duplicate keys actually matter: being replayed.
pub fn count_records(path: impl AsRef<Path>) -> Result<usize, Error> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|e| {
        Error::CassetteCorrupt(format!("failed to read cassette {}: {e}", path.display()))
    })?;
    Ok(content.lines().filter(|l| !l.trim().is_empty()).count())
}

// ── CassetteWriter ───────────────────────────────────────────────────────────

/// Append-only JSONL cassette writer (FR-009). Re-opening an existing path always appends —
/// never truncates — matching `WalWriter`'s convention, so re-running a recording session
/// accumulates rather than discarding prior entries.
pub struct CassetteWriter {
    file: Mutex<std::fs::File>,
}

impl CassetteWriter {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn append(&self, record: &CassetteRecord) -> Result<(), Error> {
        let line = serde_json::to_string(record)?;
        let mut file = self.file.lock().unwrap();
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    }
}

// ── RecordingExtractor ───────────────────────────────────────────────────────

/// Wraps `inner`, forwarding every call unchanged, and appends one [`CassetteRecord`] per call
/// to `writer`. `provider`/`model` are passed explicitly at construction time (FR-001 metadata)
/// since `Extractor` has no `model_name()` trait method and not every implementor has one to
/// report — the same rationale `LlmRouter::new` already uses for its model-name parameters.
pub struct RecordingExtractor {
    inner: Arc<dyn Extractor>,
    provider: String,
    model: String,
    writer: Arc<CassetteWriter>,
}

impl RecordingExtractor {
    pub fn new(
        inner: Arc<dyn Extractor>,
        provider: impl Into<String>,
        model: impl Into<String>,
        writer: Arc<CassetteWriter>,
    ) -> Self {
        Self {
            inner,
            provider: provider.into(),
            model: model.into(),
            writer,
        }
    }

    fn record(
        &self,
        call_type: &str,
        key: String,
        request: Value,
        response: &Value,
    ) -> Result<(), Error> {
        self.writer.append(&CassetteRecord {
            key,
            call_type: call_type.to_string(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            request,
            response: response.clone(),
        })
    }
}

impl Extractor for RecordingExtractor {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        let request = extract_request_value(&opts);
        let key = request_key(&request);
        Box::pin(async move {
            let result = self.inner.extract(opts).await?;
            let response = serde_json::to_value(&result)?;
            self.record("extract", key, request, &response)?;
            Ok(result)
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let request = classify_entities_request_value(entities, allowed_types);
        let key = request_key(&request);
        Box::pin(async move {
            let result = self
                .inner
                .classify_entities(entities, allowed_types)
                .await?;
            let response = serde_json::to_value(&result)?;
            self.record("classify_entities", key, request, &response)?;
            Ok(result)
        })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let request = classify_relations_request_value(edges, allowed_types);
        let key = request_key(&request);
        Box::pin(async move {
            let result = self.inner.classify_relations(edges, allowed_types).await?;
            let response = serde_json::to_value(&result)?;
            self.record("classify_relations", key, request, &response)?;
            Ok(result)
        })
    }
}

// ── ReplayingExtractor ───────────────────────────────────────────────────────

/// Serves `Extractor` calls entirely from a cassette loaded into memory. Holds no inner
/// extractor, makes no network call, and needs no credentials — FR-002 by construction.
///
/// A single flat index is shared across whatever extractor tree originally recorded it
/// (`LlmRouter` with primary/fallback, or a bare extractor) — matching by request-content hash
/// alone means replay needs no router-specific logic (User Story 4).
///
/// Duplicate keys are rejected at load time by [`load_records`] (#279 FR-002) — this index
/// is therefore a plain map, not a per-key queue, and a key requested twice at runtime
/// replays the same value both times rather than draining toward a miss.
#[derive(Debug)]
pub struct ReplayingExtractor {
    index: HashMap<String, Value>,
}

impl ReplayingExtractor {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let records = load_records(path)?;
        let mut index = HashMap::with_capacity(records.len());
        for record in records {
            index.insert(record.key, record.response);
        }
        Ok(Self { index })
    }

    /// Returns the recorded response for `key`, or a [`Error::CassetteMiss`] identifying the
    /// call type and key when no recorded entry matches (FR-003/SC-002).
    fn pop(&self, call_type: &str, key: &str) -> Result<Value, Error> {
        self.index.get(key).cloned().ok_or_else(|| {
            Error::CassetteMiss(format!(
                "no cassette record for {call_type} call with key {key}"
            ))
        })
    }
}

impl Extractor for ReplayingExtractor {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        let key = request_key(&extract_request_value(&opts));
        Box::pin(async move {
            let response = self.pop("extract", &key)?;
            Ok(serde_json::from_value(response)?)
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let key = request_key(&classify_entities_request_value(entities, allowed_types));
        Box::pin(async move {
            let response = self.pop("classify_entities", &key)?;
            Ok(serde_json::from_value(response)?)
        })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let key = request_key(&classify_relations_request_value(edges, allowed_types));
        Box::pin(async move {
            let response = self.pop("classify_relations", &key)?;
            Ok(serde_json::from_value(response)?)
        })
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SourceType;
    use tempfile::TempDir;

    fn opts<'a>(episode_body: &'a str) -> ExtractOptions<'a> {
        ExtractOptions {
            episode_body,
            group_id: "g1",
            source_type: SourceType::Text,
            custom_instructions: None,
            reference_time: "2026-01-01T00:00:00Z",
            ontology: None,
            chunk_key: None,
        }
    }

    #[test]
    fn extract_key_stable_across_calls() {
        let a = request_key(&extract_request_value(&opts("Alice works at Acme.")));
        let b = request_key(&extract_request_value(&opts("Alice works at Acme.")));
        assert_eq!(a, b);
    }

    #[test]
    fn extract_key_changes_with_episode_body() {
        let a = request_key(&extract_request_value(&opts("Alice works at Acme.")));
        let b = request_key(&extract_request_value(&opts("Bob works at Acme.")));
        assert_ne!(a, b);
    }

    #[test]
    fn extract_key_changes_with_source_type() {
        let mut o1 = opts("hello world");
        o1.source_type = SourceType::Text;
        let mut o2 = opts("hello world");
        o2.source_type = SourceType::Json;
        let a = request_key(&extract_request_value(&o1));
        let b = request_key(&extract_request_value(&o2));
        assert_ne!(
            a, b,
            "different source_type renders a different prompt and must produce a different key"
        );
    }

    #[test]
    fn extract_key_changes_with_ontology() {
        use crate::ontology::{EntityTypeDef, Ontology, OntologyMode};
        let o1 = opts("Alice works at Acme.");
        let mut o2 = opts("Alice works at Acme.");
        let ontology = Ontology {
            mode: OntologyMode::Open,
            entity_types: vec![EntityTypeDef {
                name: "Person".to_string(),
                description: Some("A human being".to_string()),
                parent: None,
            }],
            relation_types: vec![],
            ancestor_map: std::collections::HashMap::new(),
        };
        o2.ontology = Some(&ontology);
        let a = request_key(&extract_request_value(&o1));
        let b = request_key(&extract_request_value(&o2));
        assert_ne!(a, b);
    }

    #[test]
    fn classify_entities_key_order_dependent_but_stable() {
        let entities = [("Alice", "a person"), ("Acme", "a company")];
        let a = request_key(&classify_entities_request_value(&entities, None));
        let b = request_key(&classify_entities_request_value(&entities, None));
        assert_eq!(a, b);
    }

    #[test]
    fn classify_entities_key_changes_with_allowed_types() {
        let entities = [("Alice", "a person")];
        let allowed = vec!["Person".to_string()];
        let a = request_key(&classify_entities_request_value(&entities, None));
        let b = request_key(&classify_entities_request_value(&entities, Some(&allowed)));
        assert_ne!(a, b);
    }

    #[test]
    fn classify_relations_key_stable_and_sensitive_to_edges() {
        let edges_a = [("Alice works at Acme", "")];
        let edges_b = [("Alice founded Acme", "")];
        let allowed = vec![("WORKS_AT".to_string(), None)];
        let a = request_key(&classify_relations_request_value(&edges_a, &allowed));
        let b = request_key(&classify_relations_request_value(&edges_b, &allowed));
        assert_ne!(a, b);
    }

    #[test]
    fn writer_appends_without_truncating_across_multiple_opens() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cassette.jsonl");

        let record = |key: &str| CassetteRecord {
            key: key.to_string(),
            call_type: "extract".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            request: json!({}),
            response: json!({"entities": [], "edges": []}),
        };

        {
            let writer = CassetteWriter::open(&path).unwrap();
            writer.append(&record("k1")).unwrap();
        }
        {
            let writer = CassetteWriter::open(&path).unwrap();
            writer.append(&record("k2")).unwrap();
        }

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "re-opening must append, not truncate");
    }

    #[tokio::test]
    async fn record_then_replay_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cassette.jsonl");
        let writer = Arc::new(CassetteWriter::open(&path).unwrap());

        let inner: Arc<dyn Extractor> = Arc::new(crate::extractor::MockExtractor);
        let recorder = RecordingExtractor::new(inner, "mock", "mock-model", Arc::clone(&writer));

        let result = recorder
            .extract(opts("Alice works at Acme."))
            .await
            .unwrap();
        assert_eq!(result.result.entities.len(), 2);

        let replayer = ReplayingExtractor::load(&path).unwrap();
        let replayed = replayer
            .extract(opts("Alice works at Acme."))
            .await
            .unwrap();
        assert_eq!(replayed.result.entities.len(), result.result.entities.len());
        assert_eq!(
            replayed.result.entities[0].name,
            result.result.entities[0].name
        );
        assert_eq!(replayed.result.edges[0].fact, result.result.edges[0].fact);
    }

    #[tokio::test]
    async fn replay_miss_returns_cassette_miss_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cassette.jsonl");
        // Create an empty cassette file.
        std::fs::write(&path, "").unwrap();

        let replayer = ReplayingExtractor::load(&path).unwrap();
        let err = replayer.extract(opts("never recorded")).await.unwrap_err();
        assert!(
            matches!(err, Error::CassetteMiss(_)),
            "expected CassetteMiss, got {err:?}"
        );
    }

    #[tokio::test]
    async fn replay_rejects_duplicate_keys_at_load_time() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cassette.jsonl");
        let writer = CassetteWriter::open(&path).unwrap();

        // Two distinct responses recorded under the identical key, simulating two calls with
        // identical semantic content within one ingest run. FR-002: this must be rejected
        // outright, not served FIFO — a chunk would otherwise be scored against a stale verdict.
        let make_record = |name: &str| CassetteRecord {
            key: "dup-key".to_string(),
            call_type: "classify_entities".to_string(),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            request: json!({}),
            response: json!([name]),
        };
        writer.append(&make_record("first")).unwrap();
        writer.append(&make_record("second")).unwrap();

        let err = ReplayingExtractor::load(&path).unwrap_err();
        assert!(
            matches!(err, Error::CassetteDuplicateKey(_)),
            "expected CassetteDuplicateKey, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("dup-key"), "error should name the key: {msg}");
    }

    #[tokio::test]
    async fn replay_serves_a_key_requested_twice_without_draining() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cassette.jsonl");
        let writer = CassetteWriter::open(&path).unwrap();
        writer
            .append(&CassetteRecord {
                key: "k1".to_string(),
                call_type: "classify_entities".to_string(),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                request: json!({}),
                response: json!(["once"]),
            })
            .unwrap();

        let replayer = ReplayingExtractor::load(&path).unwrap();
        let first = replayer.pop("classify_entities", "k1").unwrap();
        let second = replayer.pop("classify_entities", "k1").unwrap();
        assert_eq!(first, json!(["once"]));
        assert_eq!(
            second,
            json!(["once"]),
            "a non-draining map serves the same value again"
        );
    }

    // ── load_records (#279 FR-002/FR-003) ────────────────────────────────────────

    fn write_lines(dir: &TempDir, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.path().join("cassette.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn load_records_missing_file_is_corrupt() {
        let err = load_records("/nonexistent/path/cassette.jsonl").unwrap_err();
        assert!(matches!(err, Error::CassetteCorrupt(_)));
    }

    #[test]
    fn load_records_malformed_json_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &["not json"]);
        let err = load_records(&path).unwrap_err();
        assert!(matches!(err, Error::CassetteCorrupt(_)), "got {err:?}");
    }

    #[test]
    fn load_records_non_object_record_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &["[1, 2, 3]"]);
        let err = load_records(&path).unwrap_err();
        assert!(matches!(err, Error::CassetteCorrupt(_)), "got {err:?}");
    }

    #[test]
    fn load_records_missing_key_field_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &[r#"{"call_type": "extract"}"#]);
        let err = load_records(&path).unwrap_err();
        assert!(matches!(err, Error::CassetteCorrupt(_)), "got {err:?}");
    }

    #[test]
    fn load_records_non_string_key_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &[r#"{"key": 5}"#]);
        let err = load_records(&path).unwrap_err();
        assert!(matches!(err, Error::CassetteCorrupt(_)), "got {err:?}");
    }

    #[test]
    fn load_records_duplicate_key_is_duplicate_not_corrupt() {
        let dir = TempDir::new().unwrap();
        let make = |name: &str| {
            serde_json::to_string(&CassetteRecord {
                key: "dup".to_string(),
                call_type: "extract".to_string(),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                request: json!({}),
                response: json!({"name": name}),
            })
            .unwrap()
        };
        let path = write_lines(&dir, &[&make("first"), &make("second")]);
        let err = load_records(&path).unwrap_err();
        assert!(matches!(err, Error::CassetteDuplicateKey(_)), "got {err:?}");
    }

    #[test]
    fn load_records_empty_file_loads_zero_records() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &[]);
        let records = load_records(&path).unwrap();
        assert_eq!(records.len(), 0);
    }

    #[test]
    fn load_records_valid_file_returns_every_record() {
        let dir = TempDir::new().unwrap();
        let record = CassetteRecord {
            key: "k1".to_string(),
            call_type: "extract".to_string(),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            request: json!({}),
            response: json!({"entities": []}),
        };
        let path = write_lines(&dir, &[&serde_json::to_string(&record).unwrap()]);
        let records = load_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "k1");
    }

    // ── count_records (#279 FR-006, handarbeit-pruefer review finding on PR #280) ────────

    #[test]
    fn count_records_counts_non_blank_lines() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &[r#"{"key": "a"}"#, "", r#"{"key": "b"}"#]);
        assert_eq!(count_records(&path).unwrap(), 2);
    }

    #[test]
    fn count_records_missing_file_is_corrupt() {
        let err = count_records("/nonexistent/path/cassette.jsonl").unwrap_err();
        assert!(matches!(err, Error::CassetteCorrupt(_)));
    }

    #[test]
    fn count_records_does_not_reject_duplicate_keys() {
        // The whole point of this function: two chunks that legitimately extract the same
        // content (classify_entities/classify_relations keys hash content, not chunk
        // identity) must not make a freshly recorded, fully-paid-for run's report
        // unreadable. load_records is right to reject this for replay; count_records must
        // not, because it answers a different question ("did the capture complete?").
        let dir = TempDir::new().unwrap();
        let make = |name: &str| {
            serde_json::to_string(&CassetteRecord {
                key: "dup".to_string(),
                call_type: "classify_entities".to_string(),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                request: json!({}),
                response: json!({"name": name}),
            })
            .unwrap()
        };
        let path = write_lines(&dir, &[&make("first"), &make("second")]);
        assert!(
            load_records(&path).is_err(),
            "load_records must still reject this"
        );
        assert_eq!(
            count_records(&path).unwrap(),
            2,
            "count_records must still count both lines"
        );
    }

    #[test]
    fn count_records_does_not_reject_corrupt_lines() {
        // Not because corruption is fine, but because FR-006's job is purely "how many lines
        // did the writer produce" — a truncated/corrupt line is still a line the writer
        // wrote, and rejecting it here would be the same premature-abort mistake as
        // rejecting on duplicate keys, just for a different guard.
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &[r#"{"key": "a"}"#, "NOT JSON"]);
        assert_eq!(count_records(&path).unwrap(), 2);
    }

    #[test]
    fn count_records_empty_file_is_zero() {
        let dir = TempDir::new().unwrap();
        let path = write_lines(&dir, &[]);
        assert_eq!(count_records(&path).unwrap(), 0);
    }
}
