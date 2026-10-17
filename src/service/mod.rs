pub mod cli;
mod launchd;
#[cfg(unix)]
pub(crate) mod state;

use thiserror::Error;

pub use launchd::{LaunchdService, ServiceStatus};
#[cfg(unix)]
pub use state::StateLease;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("service management is only supported on macOS")]
    UnsupportedPlatform,
    #[error("unsafe service path: {0}")]
    UnsafePath(&'static str),
    #[error("service is disabled; enable it before starting")]
    Disabled,
    #[error("another host holds the state directory lock")]
    AlreadyRunning,
    #[error("service filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(unix)]
    #[error("service filesystem operation failed: {0}")]
    Filesystem(#[from] rustix::io::Errno),
    #[error("invalid launchctl response: {0}")]
    InvalidResponse(&'static str),
    #[error("service property list encoding failed")]
    Plist(#[from] plist::Error),
    #[error("launchctl {operation} failed with exit status {status:?}")]
    Launchctl {
        operation: &'static str,
        status: Option<i32>,
    },
    #[error("service operation was cancelled")]
    Cancelled,
    #[error("launchctl operation timed out")]
    Timeout,
    #[error("service filesystem worker failed")]
    Worker,
}
