mod bridge;
pub mod commands;
mod files;
mod repository;
mod revocation;
mod runtime;
mod transport;
mod types;

use thiserror::Error;

pub use bridge::*;
pub use files::*;
pub use repository::{DelegationSnapshot, DelegationState, WorkerPeerState};
pub use runtime::*;
pub use transport::*;
pub use types::*;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("invalid worker input: {0}")]
    Invalid(&'static str),
    #[error("unsupported worker capability: {0}")]
    Unsupported(&'static str),
    #[error("worker identity or immutable request conflicts: {0}")]
    Conflict(&'static str),
    #[error("worker capacity exhausted: {0}")]
    Capacity(&'static str),
    #[error("worker peer is not pinned or has been revoked")]
    UntrustedPeer,
    #[error("worker operation is not permitted for this endpoint role")]
    WrongRole,
    #[error("worker operation expired or the wall clock moved backwards")]
    Expired,
    #[error("worker owner approval is required")]
    ApprovalRequired,
    #[error("worker state requires a leased Agent")]
    LeaseRequired,
    #[error("worker schema is unavailable; complete the ordered host migration")]
    SchemaUnavailable,
    #[error("worker operation was cancelled")]
    Cancelled,
    #[error("worker shutdown still owns native execution or unconfirmed cleanup")]
    ShutdownPending,
    #[error("worker operation failed")]
    Worker,
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error(transparent)]
    Device(#[from] crate::devices::DeviceError),
    #[error(transparent)]
    Remote(#[from] crate::transport::remote::RemoteError),
    #[error(transparent)]
    Agent(#[from] crate::agent::AgentError),
    #[error(transparent)]
    Scheduler(#[from] crate::scheduler::SchedulerError),
    #[error(transparent)]
    Permission(#[from] crate::permissions::PermissionError),
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
    #[error(transparent)]
    Broker(#[from] crate::tools::broker::BrokerError),
    #[error(transparent)]
    Selector(#[from] crate::selector::SelectorError),
    #[error(transparent)]
    Credential(#[from] crate::credentials::CredentialError),
    #[error(transparent)]
    Model(#[from] crate::model::ModelError),
    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolError),
}
