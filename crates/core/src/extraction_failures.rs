//! Failure-sidecar writer for extraction-call failures captured during cassette recording
//! (#306 FR-001/FR-002/FR-003). A `<cassette>.failures.jsonl` sidecar is written alongside the
//! cassette itself, one JSONL record per extraction-call failure (HTTP error,
//! budget-exhaustion-after-retry, or malformed parse) — the cassette's own success-only
//! content (FR-007) is never touched. Consumed only by [`ExtractionFailureSink`], a
//! [`TelemetrySink`] matching `TelemetryEvent::ExtractionFailure` and nothing else — every
//! other event type is ignored, the same single-event-type filtering convention
//! `lcg_eval::runner::CountingSink` already uses for `StructuredOutputParse`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::error::Error;
use crate::telemetry::{TelemetryEvent, TelemetrySink};

/// FR-003: caps a single sidecar file at 20MB before rotating to a new numbered file — bounds
/// a long-running service's aggregate sidecar growth without ever truncating an individual
/// record (SC-003: ~90KB/record, all-fail 228-chunk run stays under 20MB with zero rotation).
pub const DEFAULT_MAX_BYTES_PER_FILE: u64 = 20 * 1024 * 1024;

/// One JSONL row in the `<cassette>.failures.jsonl` sidecar (FR-001's Key Entities: "Failure
/// Record"). Mirrors `TelemetryEvent::ExtractionFailure`'s fields exactly — this is a plain
/// serialization boundary, not a distinct data model.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ExtractionFailureRecord {
    pub ts_ms: u64,
    pub model: String,
    /// `"entities"` or `"edges"`.
    pub call_type: String,
    pub chunk_key: Option<String>,
    /// `"http_error"`, `"truncation"`, or `"malformed"`.
    pub classification: String,
    /// The complete raw response body — never truncated to a prefix (FR-002).
    pub raw_body: String,
    pub finish_reason: Option<String>,
    pub completion_tokens: Option<u64>,
    pub max_tokens: u32,
}

struct WriterState {
    file: std::fs::File,
    file_seq: u32,
    bytes_in_current_file: u64,
}

/// Append-only, byte-size-rotating JSONL writer for the failures sidecar (FR-003 bounds the
/// aggregate, never the individual record — FR-002 requires each record stay complete).
/// Rotation mirrors `WalWriter`'s `max_bytes_per_file` model: the first file keeps the
/// unnumbered `<cassette>.failures.jsonl` name (matching the User Story 1 acceptance scenario's
/// path exactly); once appending would push it over `max_bytes_per_file`, it's closed and a
/// numbered `<cassette>.failures.N.jsonl` is opened instead. `max_bytes_per_file = 0` disables
/// rotation, matching `WalWriter::new`'s own convention. Mutex-guarded, append-only, flushed
/// per write — the same concurrency model `CassetteWriter` already uses.
pub struct ExtractionFailureWriter {
    base_path: PathBuf,
    max_bytes_per_file: u64,
    inner: Mutex<WriterState>,
}

impl ExtractionFailureWriter {
    /// Opens (creating if absent) the sidecar for `cassette_path`, e.g. `run.jsonl` →
    /// `run.jsonl.failures.jsonl`. Created eagerly, empty if no failures ever occur, matching
    /// `CassetteWriter::open`'s own eager-creation behavior. Re-opening an existing path
    /// resumes appending to it rather than truncating.
    ///
    /// If rotation already happened in a previous process (FR-003's "long-running service" —
    /// e.g. this writer is reconstructed across a service restart), resumes appending to the
    /// highest-numbered rotated file already on disk rather than the unnumbered base file: the
    /// base file was already rotated away from and its on-disk size is stale, so reopening it
    /// here would silently resume writing into a file this writer previously closed, and the
    /// next rotation would reuse `file_seq = 1` and treat an existing, possibly near-cap
    /// `<cassette>.failures.1.jsonl` as empty — letting it silently grow past
    /// `max_bytes_per_file`.
    pub fn open(cassette_path: impl AsRef<Path>, max_bytes_per_file: u64) -> Result<Self, Error> {
        let base_path = sidecar_path(cassette_path.as_ref());
        if let Some(parent) = base_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Ensure the unnumbered path exists even when rotation resumes elsewhere — the User
        // Story 1 acceptance scenario expects `<cassette>.failures.jsonl` to exist.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base_path)?;

        let (file_seq, active_path) =
            highest_existing_numbered_file(&base_path).unwrap_or((0, base_path.clone()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_path)?;
        let bytes_in_current_file = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            base_path,
            max_bytes_per_file,
            inner: Mutex::new(WriterState {
                file,
                file_seq,
                bytes_in_current_file,
            }),
        })
    }

    /// The base (unnumbered) sidecar path — stable regardless of how many times rotation has
    /// occurred, useful for tests and callers that only need "does a sidecar exist here".
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    pub fn append(&self, record: &ExtractionFailureRecord) -> Result<(), Error> {
        let line = serde_json::to_string(record)?;
        let chunk_bytes = (line.len() + 1) as u64;
        let mut state = self.inner.lock().unwrap();
        if self.max_bytes_per_file > 0
            && state.bytes_in_current_file > 0
            && state.bytes_in_current_file + chunk_bytes > self.max_bytes_per_file
        {
            state.file_seq += 1;
            let rotated_path = numbered_path(&self.base_path, state.file_seq);
            state.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&rotated_path)?;
            state.bytes_in_current_file = 0;
        }
        writeln!(state.file, "{line}")?;
        state.file.flush()?;
        state.bytes_in_current_file += chunk_bytes;
        Ok(())
    }
}

/// `<cassette_path>.failures.jsonl` — e.g. `run.cassette.jsonl` becomes
/// `run.cassette.jsonl.failures.jsonl`.
fn sidecar_path(cassette_path: &Path) -> PathBuf {
    let mut os = cassette_path.as_os_str().to_owned();
    os.push(".failures.jsonl");
    PathBuf::from(os)
}

/// `<cassette>.failures.N.jsonl` for the `seq`-th rotated file (`seq >= 1`).
fn numbered_path(base_path: &Path, seq: u32) -> PathBuf {
    let base_str = base_path.to_string_lossy();
    let stripped = base_str.strip_suffix(".jsonl").unwrap_or(&base_str);
    PathBuf::from(format!("{stripped}.{seq}.jsonl"))
}

/// Scans `base_path`'s directory for the highest-numbered `<cassette>.failures.N.jsonl` file
/// already on disk, returning `(seq, path)` — `None` if no rotation has ever happened for this
/// sidecar. Used by [`ExtractionFailureWriter::open`] to resume rotation state across a process
/// restart instead of assuming `file_seq: 0`.
fn highest_existing_numbered_file(base_path: &Path) -> Option<(u32, PathBuf)> {
    let parent = base_path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let base_str = base_path.to_string_lossy();
    let stripped = base_str.strip_suffix(".jsonl").unwrap_or(&base_str);
    let stem = Path::new(stripped).file_name()?.to_str()?;
    let prefix = format!("{stem}.");

    let mut max_seq = None;
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if let Ok(seq) = rest.parse::<u32>() {
            max_seq = Some(max_seq.map_or(seq, |m: u32| m.max(seq)));
        }
    }
    max_seq.map(|seq| (seq, numbered_path(base_path, seq)))
}

/// `TelemetrySink` that writes only `TelemetryEvent::ExtractionFailure` events to the sidecar
/// — every other event type (including the lightweight `ExtractionTruncated`/
/// `StructuredOutputParse` siblings) is silently ignored.
pub struct ExtractionFailureSink {
    writer: ExtractionFailureWriter,
}

impl ExtractionFailureSink {
    pub fn new(writer: ExtractionFailureWriter) -> Self {
        Self { writer }
    }
}

impl TelemetrySink for ExtractionFailureSink {
    fn emit(&self, event: TelemetryEvent) {
        if let TelemetryEvent::ExtractionFailure {
            ts_ms,
            model,
            call_type,
            chunk_key,
            classification,
            raw_body,
            finish_reason,
            completion_tokens,
            max_tokens,
        } = event
        {
            let record = ExtractionFailureRecord {
                ts_ms,
                model,
                call_type,
                chunk_key,
                classification,
                raw_body,
                finish_reason,
                completion_tokens,
                max_tokens,
            };
            if let Err(e) = self.writer.append(&record) {
                eprintln!(
                    "liminis-context-graph: failed to write extraction-failure sidecar record: {e}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::now_ms;
    use tempfile::TempDir;

    fn failure_event(call_type: &str, classification: &str, raw_body: &str) -> TelemetryEvent {
        TelemetryEvent::ExtractionFailure {
            ts_ms: now_ms(),
            model: "test-model".to_string(),
            call_type: call_type.to_string(),
            chunk_key: Some("chunk-1".to_string()),
            classification: classification.to_string(),
            raw_body: raw_body.to_string(),
            finish_reason: Some("max_tokens".to_string()),
            completion_tokens: Some(8192),
            max_tokens: 8192,
        }
    }

    #[test]
    fn open_creates_sidecar_file_eagerly() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        let writer =
            ExtractionFailureWriter::open(&cassette_path, DEFAULT_MAX_BYTES_PER_FILE).unwrap();
        assert!(writer.base_path().exists());
        assert_eq!(
            writer.base_path(),
            dir.path().join("run.cassette.jsonl.failures.jsonl")
        );
        assert_eq!(std::fs::read_to_string(writer.base_path()).unwrap(), "");
    }

    #[test]
    fn append_writes_one_jsonl_line_with_complete_body() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        let writer =
            ExtractionFailureWriter::open(&cassette_path, DEFAULT_MAX_BYTES_PER_FILE).unwrap();
        let record = ExtractionFailureRecord {
            ts_ms: 1,
            model: "m".to_string(),
            call_type: "edges".to_string(),
            chunk_key: Some("chunk-42".to_string()),
            classification: "malformed".to_string(),
            raw_body: "{\"choices\":[{\"message\":{\"content\":\"Unterminated string".to_string(),
            finish_reason: Some("stop".to_string()),
            completion_tokens: Some(100),
            max_tokens: 8192,
        };
        writer.append(&record).unwrap();

        let content = std::fs::read_to_string(writer.base_path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: ExtractionFailureRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.chunk_key, Some("chunk-42".to_string()));
        assert_eq!(parsed.classification, "malformed");
        assert_eq!(
            parsed.raw_body, record.raw_body,
            "body must be stored whole, not truncated"
        );
    }

    #[test]
    fn append_across_multiple_opens_accumulates_without_truncating() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        {
            let writer =
                ExtractionFailureWriter::open(&cassette_path, DEFAULT_MAX_BYTES_PER_FILE).unwrap();
            writer.append(&record_n(1)).unwrap();
        }
        {
            let writer =
                ExtractionFailureWriter::open(&cassette_path, DEFAULT_MAX_BYTES_PER_FILE).unwrap();
            writer.append(&record_n(2)).unwrap();
        }
        let sidecar_path = dir.path().join("run.cassette.jsonl.failures.jsonl");
        let content = std::fs::read_to_string(&sidecar_path).unwrap();
        assert_eq!(
            content.lines().count(),
            2,
            "re-opening must append, not truncate"
        );
    }

    fn record_n(n: u32) -> ExtractionFailureRecord {
        ExtractionFailureRecord {
            ts_ms: n as u64,
            model: "m".to_string(),
            call_type: "entities".to_string(),
            chunk_key: None,
            classification: "http_error".to_string(),
            raw_body: String::new(),
            finish_reason: None,
            completion_tokens: None,
            max_tokens: 8192,
        }
    }

    #[test]
    fn rotates_to_a_numbered_file_once_max_bytes_exceeded() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        // Small enough that a second record forces rotation, but each single record still
        // fits (FR-003: bound the aggregate, never the individual record).
        let record = record_n(1);
        let one_line_bytes = serde_json::to_string(&record).unwrap().len() as u64 + 1;
        let writer = ExtractionFailureWriter::open(&cassette_path, one_line_bytes + 1).unwrap();

        writer.append(&record).unwrap();
        writer.append(&record_n(2)).unwrap();
        writer.append(&record_n(3)).unwrap();

        let base = dir.path().join("run.cassette.jsonl.failures.jsonl");
        let rotated_1 = dir.path().join("run.cassette.jsonl.failures.1.jsonl");
        let rotated_2 = dir.path().join("run.cassette.jsonl.failures.2.jsonl");
        assert!(base.exists());
        assert!(
            rotated_1.exists(),
            "expected a rotated file after the 2nd record"
        );
        assert!(
            rotated_2.exists(),
            "expected a second rotated file after the 3rd record"
        );
        assert_eq!(std::fs::read_to_string(&base).unwrap().lines().count(), 1);
        assert_eq!(
            std::fs::read_to_string(&rotated_1).unwrap().lines().count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(&rotated_2).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn reopening_after_rotation_resumes_from_the_highest_numbered_file_not_the_base() {
        // FR-003 names a "long-running service" — a process restart is a normal part of that
        // lifetime. A writer reopened after rotation already happened must not silently resume
        // writing into the (already rotated-away-from, stale-sized) base file, and must not
        // treat an existing near-cap numbered file as empty on the next rotation decision.
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        let record = record_n(1);
        let one_line_bytes = serde_json::to_string(&record).unwrap().len() as u64 + 1;
        let max_bytes = one_line_bytes + 1;

        {
            let writer = ExtractionFailureWriter::open(&cassette_path, max_bytes).unwrap();
            writer.append(&record).unwrap();
            writer.append(&record_n(2)).unwrap();
            // Now: base has 1 record, run.cassette.jsonl.failures.1.jsonl has 1 record and is
            // already at the rotation cap.
        }

        // Simulate a process restart: a brand-new writer instance for the same cassette path.
        let writer = ExtractionFailureWriter::open(&cassette_path, max_bytes).unwrap();
        writer.append(&record_n(3)).unwrap();

        let base = dir.path().join("run.cassette.jsonl.failures.jsonl");
        let rotated_1 = dir.path().join("run.cassette.jsonl.failures.1.jsonl");
        let rotated_2 = dir.path().join("run.cassette.jsonl.failures.2.jsonl");

        assert_eq!(
            std::fs::read_to_string(&base).unwrap().lines().count(),
            1,
            "the base file must not be silently resumed once rotation already moved past it"
        );
        assert_eq!(
            std::fs::read_to_string(&rotated_1).unwrap().lines().count(),
            1,
            "the already-full rotated file must not silently grow past the cap"
        );
        assert_eq!(
            std::fs::read_to_string(&rotated_2).unwrap().lines().count(),
            1,
            "the post-restart record must land in a fresh rotated file, not file .1"
        );
    }

    #[test]
    fn zero_max_bytes_disables_rotation() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        let writer = ExtractionFailureWriter::open(&cassette_path, 0).unwrap();
        for i in 0..5 {
            writer.append(&record_n(i)).unwrap();
        }
        let content = std::fs::read_to_string(writer.base_path()).unwrap();
        assert_eq!(content.lines().count(), 5);
        assert!(!dir
            .path()
            .join("run.cassette.jsonl.failures.1.jsonl")
            .exists());
    }

    #[test]
    fn sink_writes_only_extraction_failure_events() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        let writer =
            ExtractionFailureWriter::open(&cassette_path, DEFAULT_MAX_BYTES_PER_FILE).unwrap();
        let sidecar_path = writer.base_path().to_path_buf();
        let sink = ExtractionFailureSink::new(writer);

        sink.emit(TelemetryEvent::WalAppend {
            ts_ms: 0,
            duration_us: 1,
            bytes: 1,
        });
        sink.emit(failure_event("entities", "http_error", "raw body text"));

        let content = std::fs::read_to_string(&sidecar_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "only the ExtractionFailure event should be written"
        );
        let parsed: ExtractionFailureRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.call_type, "entities");
        assert_eq!(parsed.classification, "http_error");
        assert_eq!(parsed.raw_body, "raw body text");
        assert_eq!(parsed.chunk_key, Some("chunk-1".to_string()));
        assert_eq!(parsed.finish_reason, Some("max_tokens".to_string()));
        assert_eq!(parsed.completion_tokens, Some(8192));
        assert_eq!(parsed.max_tokens, 8192);
    }

    #[test]
    fn sink_ignores_extraction_truncated_and_structured_output_parse() {
        let dir = TempDir::new().unwrap();
        let cassette_path = dir.path().join("run.cassette.jsonl");
        let writer =
            ExtractionFailureWriter::open(&cassette_path, DEFAULT_MAX_BYTES_PER_FILE).unwrap();
        let sidecar_path = writer.base_path().to_path_buf();
        let sink = ExtractionFailureSink::new(writer);

        sink.emit(TelemetryEvent::ExtractionTruncated {
            ts_ms: 0,
            model: "m".to_string(),
            chunk_len_bytes: 10,
            initial_max_tokens: 8192,
            retry_succeeded: false,
            chunk_key: Some("chunk-1".to_string()),
        });
        sink.emit(TelemetryEvent::StructuredOutputParse {
            ts_ms: 0,
            model: "m".to_string(),
            call_type: "entities".to_string(),
            outcome: "malformed".to_string(),
        });

        assert_eq!(std::fs::read_to_string(&sidecar_path).unwrap(), "");
    }
}
