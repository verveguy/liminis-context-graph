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

    #[error("WAL parse error: {0}")]
    WalParse(String),
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Join(e.to_string())
    }
}
