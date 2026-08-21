//! WAL embedding-model identity stamp (issue #440, FR-005/FR-006) — a small, opaque, write-once
//! record persisted per group WAL directory alongside `.wal-generation.json`
//! (`crate::wal_generation`), recording the embedding model identifier and vector dimension that
//! were active when the stream was first written.
//!
//! This is a deliberately separate sidecar file, not new fields bolted onto
//! `.wal-generation.json`: the "opaque reset token" concern and the "which embedder wrote this"
//! concern are independent and should stay independently readable/writable, without coupling
//! this module's simpler semantics to `wal_generation.rs`'s carefully-reasoned concurrency and
//! self-heal logic for the reset token.
//!
//! On-disk layout: `<wal_dir>/.wal-embedding-model.json` holding
//! `{"model": "<identifier>", "dim": <vector dimension>}`.
//!
//! Like `.wal-generation.json`, this file is load-bearing but never a hard-failure surface: a
//! missing or corrupt record is treated as "unknown," never as a mismatch (see
//! `read_model_identity`) — a WAL predating this feature (Edge Cases, FR-009) must keep replaying
//! exactly as it did before, using recompute wherever source text is present, with no migration
//! step and no possibility of a damaged-but-harmless artifact masquerading as a detected
//! mismatch.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Error;

/// File name (not full path) of the embedding-model identity sidecar.
pub const WAL_EMBEDDING_MODEL_FILE: &str = ".wal-embedding-model.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelIdentityRecord {
    model: String,
    dim: i64,
}

fn identity_file_path(wal_dir: &Path) -> PathBuf {
    wal_dir.join(WAL_EMBEDDING_MODEL_FILE)
}

/// Reads the recorded `(model, dim)` identity for `wal_dir`, or `None` if no record exists, the
/// file is unreadable, or its contents don't parse as a well-formed, non-empty record. A
/// corrupted record is deliberately indistinguishable from "no record" — damage to this file
/// must never be misread as a detected mismatch.
pub fn read_model_identity(wal_dir: &Path) -> Option<(String, i64)> {
    let text = fs::read_to_string(identity_file_path(wal_dir)).ok()?;
    let record: ModelIdentityRecord = serde_json::from_str(&text).ok()?;
    if record.model.trim().is_empty() || record.dim <= 0 {
        return None;
    }
    Some((record.model, record.dim))
}

/// Ensures `wal_dir` has a recorded embedding-model identity, minting one from `(model, dim)` if
/// none exists yet. Idempotent: a second call is a no-op that returns the already-recorded
/// identity, even if `(model, dim)` differs from what's passed — this function only *mints*, it
/// never overwrites (a later mismatch is detected, not silently corrected, per FR-006).
///
/// Callers MUST only invoke this when the stream has no prior content (mirroring
/// `wal_generation::ensure_generation`'s contract) — see `AppState::with_wal_writer`, gated on
/// `WalWriter::global_seq() == 0`.
///
/// Race-safe via the same publish-by-`hard_link` pattern as `wal_generation::ensure_generation`:
/// the target path only ever comes into existence already fully written, so a racing loser's
/// `read_model_identity` immediately after a failed `hard_link` is guaranteed to see the winner's
/// complete content.
pub fn ensure_model_identity(
    wal_dir: &Path,
    model: &str,
    dim: i64,
) -> Result<(String, i64), Error> {
    ensure_model_identity_impl(wal_dir, model, dim, true)
}

fn ensure_model_identity_impl(
    wal_dir: &Path,
    model: &str,
    dim: i64,
    self_heal_on_wreckage: bool,
) -> Result<(String, i64), Error> {
    if let Some(existing) = read_model_identity(wal_dir) {
        return Ok(existing);
    }

    let record = ModelIdentityRecord {
        model: model.to_string(),
        dim,
    };
    let json = serde_json::to_string(&record)?;
    let path = identity_file_path(wal_dir);
    let tmp_path = wal_dir.join(format!("{WAL_EMBEDDING_MODEL_FILE}.tmp-{}", Uuid::new_v4()));

    let write_tmp = (|| -> Result<(), Error> {
        let mut tmp_file = fs::File::create(&tmp_path)?;
        tmp_file.write_all(json.as_bytes())?;
        tmp_file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_tmp {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    let link_result = fs::hard_link(&tmp_path, &path);
    let _ = fs::remove_file(&tmp_path);

    match link_result {
        Ok(()) => {
            sync_dir(wal_dir);
            Ok((record.model, record.dim))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(winner) = read_model_identity(wal_dir) {
                return Ok(winner);
            }
            if self_heal_on_wreckage {
                let _ = fs::remove_file(&path);
                return ensure_model_identity_impl(wal_dir, model, dim, false);
            }
            Err(Error::WalIo(std::io::Error::other(
                "embedding-model identity file exists but could not be read after concurrent creation",
            )))
        }
        Err(e) => Err(Error::WalIo(e)),
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Pure comparison: `true` only when `recorded` is known and differs (by model name or by
/// dimension) from `running`. Mirrors `wal_generation::generation_mismatch`'s "unknown never
/// mismatches" rule — `recorded: None` (never stamped, or currently unreadable/corrupt) is never
/// a mismatch (FR-009's "still replay without any migration step").
///
/// A dimension-only difference (e.g. via `LCG_EMBEDDING_DIM` override with the same model name)
/// counts as a mismatch too — the spec's Edge Cases explicitly call this "a model-identity
/// mismatch, not just a model-name mismatch."
pub fn model_identity_mismatch(recorded: Option<(&str, i64)>, running: (&str, i64)) -> bool {
    match recorded {
        Some((model, dim)) => model != running.0 || dim != running.1,
        None => false,
    }
}

/// Formats a warning message when replaying `wal_dir` under `running`'s embedder identity would
/// mismatch the identity recorded for that WAL (FR-006), or `None` when there's no mismatch
/// (including "unknown" — no sidecar present, e.g. every WAL written before this issue, FR-009).
/// Callers decide how to surface the message (`[WAL WARN]` log line, a `knowledge_status` field,
/// etc.) — this only computes it, so the wording stays consistent across all three replay call
/// sites without each reimplementing the comparison.
pub fn check_model_identity_for_replay(wal_dir: &Path, running: (&str, i64)) -> Option<String> {
    let recorded = read_model_identity(wal_dir);
    let recorded_ref = recorded.as_ref().map(|(m, d)| (m.as_str(), *d));
    if !model_identity_mismatch(recorded_ref, running) {
        return None;
    }
    let (rec_model, rec_dim) = recorded.expect("model_identity_mismatch implies recorded is Some");
    Some(format!(
        "embedding-model mismatch for {}: WAL was written under model={rec_model:?} \
         dim={rec_dim}, but the running embedder is model={:?} dim={} — vectors will be \
         recomputed from source text where available, and mismatched fallback-bound vectors may \
         not be comparable to the running embedder's query vectors (issue #440)",
        wal_dir.display(),
        running.0,
        running.1
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_model_identity_is_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_model_identity(tmp.path()), None);
    }

    #[test]
    fn read_model_identity_is_none_for_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(identity_file_path(tmp.path()), "not json").unwrap();
        assert_eq!(read_model_identity(tmp.path()), None);
    }

    #[test]
    fn read_model_identity_is_none_for_empty_model_value() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(identity_file_path(tmp.path()), r#"{"model":"","dim":768}"#).unwrap();
        assert_eq!(read_model_identity(tmp.path()), None);
    }

    #[test]
    fn read_model_identity_is_none_for_non_positive_dim() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            identity_file_path(tmp.path()),
            r#"{"model":"bge-base-en-v1.5","dim":0}"#,
        )
        .unwrap();
        assert_eq!(read_model_identity(tmp.path()), None);
    }

    #[test]
    fn ensure_model_identity_mints_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let (model, dim) = ensure_model_identity(tmp.path(), "bge-base-en-v1.5", 768).unwrap();
        assert_eq!(model, "bge-base-en-v1.5");
        assert_eq!(dim, 768);
        assert_eq!(
            read_model_identity(tmp.path()),
            Some(("bge-base-en-v1.5".to_string(), 768))
        );
    }

    #[test]
    fn ensure_model_identity_is_idempotent_and_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let first = ensure_model_identity(tmp.path(), "model-a", 768).unwrap();
        let second = ensure_model_identity(tmp.path(), "model-b", 1024).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, ("model-a".to_string(), 768));
    }

    #[test]
    fn ensure_model_identity_self_heals_pre_existing_wreckage() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(identity_file_path(tmp.path()), "").unwrap();

        let (model, dim) = ensure_model_identity(tmp.path(), "model-a", 768).unwrap();
        assert_eq!(model, "model-a");
        assert_eq!(dim, 768);
    }

    #[test]
    fn check_model_identity_for_replay_is_none_when_unstamped() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            check_model_identity_for_replay(tmp.path(), ("bge-base-en-v1.5", 768)),
            None
        );
    }

    #[test]
    fn check_model_identity_for_replay_is_none_when_matching() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_model_identity(tmp.path(), "bge-base-en-v1.5", 768).unwrap();
        assert_eq!(
            check_model_identity_for_replay(tmp.path(), ("bge-base-en-v1.5", 768)),
            None
        );
    }

    #[test]
    fn check_model_identity_for_replay_reports_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_model_identity(tmp.path(), "bge-base-en-v1.5", 768).unwrap();
        let msg = check_model_identity_for_replay(tmp.path(), ("other-model", 1024)).unwrap();
        assert!(msg.contains("bge-base-en-v1.5"));
        assert!(msg.contains("other-model"));
    }

    #[test]
    fn model_identity_mismatch_requires_known_and_different() {
        assert!(!model_identity_mismatch(None, ("a", 768)));
        assert!(!model_identity_mismatch(Some(("a", 768)), ("a", 768)));
        assert!(model_identity_mismatch(Some(("a", 768)), ("b", 768)));
        assert!(model_identity_mismatch(Some(("a", 768)), ("a", 1024)));
        assert!(model_identity_mismatch(Some(("a", 768)), ("b", 1024)));
    }
}
