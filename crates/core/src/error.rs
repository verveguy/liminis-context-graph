#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Lbug(#[from] lbug::Error),

    #[error("invalid path")]
    InvalidPath,

    #[error("query failed: {0}")]
    QueryFailed(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("task join error: {0}")]
    Join(String),

    #[error("WAL I/O error: {0}")]
    WalIo(#[from] std::io::Error),

    #[error("WAL JSON error: {0}")]
    WalJson(String),

    #[error("DB unavailable, recovery required: {0}")]
    DbUnavailable(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("configuration error: {0}")]
    Config(String),

    /// Replay found no cassette record matching the request's semantic-content hash (FR-003).
    /// Distinct from `Error::Ipc` so callers/tests can match on it directly rather than
    /// string-sniffing an error message. See `crate::cassette` for the record/replay design.
    #[error("cassette miss: {0}")]
    CassetteMiss(String),

    /// A cassette file is malformed: unreadable, invalid JSON, a non-object record, a
    /// record missing `key`, or a record whose `key` is not a string. Distinct from
    /// `Error::CassetteDuplicateKey` so callers can tell "this file is corrupt" from "this
    /// file has a workflow problem" by type, not by sniffing the message (#279 FR-003).
    #[error("cassette corrupt: {0}")]
    CassetteCorrupt(String),

    /// A cassette file contains two records with the same key (#279 FR-002). Replay used to
    /// serve duplicates FIFO, silently scoring a chunk against a stale verdict; loading now
    /// rejects this outright instead.
    #[error("cassette duplicate key: {0}")]
    CassetteDuplicateKey(String),

    /// A WAL checkpoint `name` fails the filesystem-safe charset/length check (issue #365,
    /// `checkpoint::validate_name`). Distinct from `Error::Ipc` so `knowledge_wal_mark_create`
    /// can surface a specific, actionable message rather than a generic one.
    #[error("invalid checkpoint name {0:?}: must be 1-200 chars of [A-Za-z0-9_-]")]
    CheckpointInvalidName(String),

    /// `knowledge_wal_mark_create` was called with a `name` that already identifies an active
    /// (non-deleted) checkpoint (issue #365, FR-006). The existing record is left unmodified.
    #[error("checkpoint {0:?} already exists")]
    CheckpointDuplicateName(String),

    /// `knowledge_wal_mark_delete` was called with a `name` that does not currently identify an
    /// active checkpoint — never created, or already deleted (issue #365, FR-008).
    #[error("checkpoint {0:?} not found")]
    CheckpointNotFound(String),

    /// A `group_id` is empty, or its filesystem-directory-name mapping (`wal_group`, issue #378
    /// FR-005) would be empty or exceed `checkpoint::validate_name`'s 200-char bound. Distinct
    /// from `Error::Ipc` so callers can surface a specific, actionable message.
    #[error(
        "invalid group_id {0:?}: must be non-empty and map to a directory name of 1-200 chars"
    )]
    InvalidGroupId(String),

    /// `knowledge_rebuild_from_wal` was asked to replay a group that already has a recorded
    /// `WalPosition` (`applied_seq.is_some()`) whose current on-disk generation is unknown —
    /// `.wal-generation.json` is missing or unreadable (issue #414, FR-002). Distinct from
    /// `Error::Ipc` so callers/tests can match on it directly rather than string-sniffing,
    /// matching SC-003's "zero silent skips" requirement. Refusal, not a warning: replay does
    /// not proceed, and no configuration flag, environment variable, or request parameter
    /// bypasses this check (Out of Scope).
    #[error("WAL stream generation unknown: {0}")]
    WalGenerationUnknown(String),
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Join(e.to_string())
    }
}

/// User-facing message returned when a missing-index query fails auto-heal (either because a
/// rebuild attempt itself failed, or because `indices_built` was already `true` so no rebuild
/// was attempted). Shared by the search handlers' and the ingest dedup path's auto-heal logic.
pub const MISSING_INDEX_USER_MSG: &str =
    "Knowledge graph indices not yet built. Call knowledge_build_indices to resolve.";

/// True if `err` is lbug's "no index with this name" binder exception, raised when a search
/// query targets an FTS/HNSW index that hasn't been (re)built yet. Used by the search handlers'
/// auto-heal path (ADR-0025) to distinguish "indices missing" from any other query failure.
pub fn is_missing_index_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("Binder exception:") && s.contains("doesn't have an index with name")
}

/// True if `err` is lbug's "index already exists" binder exception, raised by
/// `CREATE_VECTOR_INDEX`/`CREATE_FTS_INDEX` when the target index was already built (e.g. by a
/// prior `init_schema` or a previous `build_indices_and_constraints` call). This is the
/// idempotent, expected case index-build callers must swallow — anything else (a missing table,
/// a malformed column, resource exhaustion, ...) is a genuine failure and must propagate.
pub fn is_already_exists_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("Binder exception:") && s.contains("already exists in table")
}

/// True if `err` is lbug's "table does not exist" binder exception, raised when a query
/// references a node/rel label that isn't present in the schema (e.g. `Entity` renamed or
/// dropped out from under an otherwise-open database). Used by `handle_knowledge_status`
/// (issue #325) to distinguish "graph open but a core table is broken" — which must degrade to
/// a status response — from any other query failure, which must still propagate (FR-006).
/// Textually disjoint from [`is_missing_index_error`] ("doesn't have an index with name") and
/// [`is_already_exists_error`] ("already exists in table").
///
/// Anchored on `Table <single-identifier> does not exist` rather than three independent
/// substrings, so it does not also match lbug's distinct `Table function <name> does not exist`
/// binder exception (raised for an unknown table function/pragma) — a different failure that
/// must still propagate rather than degrade.
pub fn is_missing_table_error(err: &Error) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"Table \S+ does not exist").unwrap());
    let s = err.to_string();
    s.contains("Binder exception:") && re.is_match(&s)
}

#[cfg(test)]
mod is_missing_table_error_tests {
    use super::*;

    #[test]
    fn matches_genuine_missing_table() {
        let err = Error::QueryFailed(
            "Query execution failed: Binder exception: Table Entity does not exist.".to_string(),
        );
        assert!(is_missing_table_error(&err));
    }

    #[test]
    fn does_not_match_missing_table_function() {
        // A distinct lbug binder exception for an unknown table function/pragma — must not be
        // misclassified as a missing node/rel table, or a genuine failure would silently
        // degrade instead of propagating (FR-006).
        let err = Error::QueryFailed(
            "Query execution failed: Binder exception: Table function some_pragma does not \
             exist."
                .to_string(),
        );
        assert!(!is_missing_table_error(&err));
    }
}
