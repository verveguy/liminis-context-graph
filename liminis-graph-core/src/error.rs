#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Lbug(#[from] lbug::Error),

    #[error("invalid path")]
    InvalidPath,

    #[error("query failed: {0}")]
    QueryFailed(String),
}
