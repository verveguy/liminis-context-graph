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
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Join(e.to_string())
    }
}

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
