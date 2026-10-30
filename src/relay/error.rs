use crate::{crypto::CryptoError, relay_protocol::RelayProtocolError};
use thiserror::Error;
use tokio_rusqlite::rusqlite;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("SQLite worker failed: {0}")]
    DatabaseWorker(String),
    #[error("JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("crypto failed: {0}")]
    Crypto(#[from] CryptoError),
    #[error("relay protocol failed: {0}")]
    Protocol(#[from] RelayProtocolError),
    #[error("invalid configuration: {0}")]
    Configuration(String),
    #[error("system clock is invalid")]
    Clock,
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("device identity is already enrolled or revoked and cannot be replaced")]
    DeviceConflict,
    #[error("recipient is unavailable")]
    RecipientUnavailable,
    #[error("queue or metadata quota reached")]
    QueueFull,
    #[error("message identifier was used for different ciphertext")]
    MessageConflict,
    #[error("device rate limit reached")]
    RateLimited,
    #[error("WebSocket failed: {0}")]
    WebSocket(String),
    #[error("device already has an active session")]
    SessionAlreadyActive,
    #[error("operation timed out")]
    Timeout,
    #[error("operation cancelled")]
    Cancelled,
}
