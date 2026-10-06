use thiserror::Error;

/// Convenience alias for results produced by the core engine.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors surfaced by the mail engine.
#[derive(Debug, Error)]
pub enum Error {
    #[error("not implemented yet")]
    Unimplemented,

    #[error("account not found: {0}")]
    AccountNotFound(i64),

    #[error("folder not found: {0}")]
    FolderNotFound(i64),

    #[error("message not found: {0}")]
    MessageNotFound(i64),

    #[error("authentication failed for account {0}")]
    Authentication(i64),

    #[error("network error: {0}")]
    Network(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}
