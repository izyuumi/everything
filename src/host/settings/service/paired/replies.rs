mod comparison;

pub(crate) use comparison::SettingsReplyComparison;

use std::fmt;

use rusqlite::Transaction;
use thiserror::Error;

use crate::devices::{
    SettingsAccessReplyContractError, SettingsOwnerError, SettingsReceiveError,
    SettingsReplyPublicationError, SettingsReplyQualification,
};
use crate::storage::{ConsumedSettingsReply, Store};

use super::access::SettingsAccessQualificationError;
use super::{SettingsReceiptAuthority, SettingsServiceError};

#[derive(Error)]
pub(crate) enum SettingsReplyProjectionError {
    #[error("Settings reply current authority check failed")]
    Authority(#[source] Box<SettingsServiceError>),
    #[error("Settings reply build ownership check failed")]
    Build(#[from] SettingsReceiveError),
    #[error("Settings Access reply current authority check failed")]
    AccessQualification(#[from] SettingsAccessQualificationError),
    #[error("Settings Access reply data is invalid")]
    AccessData(#[from] SettingsAccessReplyContractError),
}

impl From<SettingsServiceError> for SettingsReplyProjectionError {
    fn from(error: SettingsServiceError) -> Self {
        Self::Authority(Box::new(error))
    }
}

impl fmt::Debug for SettingsReplyProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyProjectionError([retained cause])")
    }
}

pub(crate) enum SettingsQueuedReplyCheckError {
    Owner(SettingsOwnerError),
    Current(SettingsReplyPublicationError),
}

impl From<SettingsOwnerError> for SettingsQueuedReplyCheckError {
    fn from(cause: SettingsOwnerError) -> Self {
        Self::Owner(cause)
    }
}

impl From<SettingsReplyPublicationError> for SettingsQueuedReplyCheckError {
    fn from(cause: SettingsReplyPublicationError) -> Self {
        Self::Current(cause)
    }
}

impl From<SettingsReplyProjectionError> for SettingsQueuedReplyCheckError {
    fn from(cause: SettingsReplyProjectionError) -> Self {
        Self::Current(cause.into())
    }
}

impl fmt::Debug for SettingsQueuedReplyCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsQueuedReplyCheckError([retained cause])")
    }
}

impl fmt::Display for SettingsQueuedReplyCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Owner(_) => "Settings queued reply ownership bookkeeping failed",
            Self::Current(_) => "Settings queued reply current authority check failed",
        })
    }
}

impl std::error::Error for SettingsQueuedReplyCheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Owner(_) => None,
            Self::Current(cause) => Some(cause),
        }
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn recheck_consumed_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<(), SettingsReplyPublicationError> {
        consumed.check_on(transaction)?;
        let SettingsReplyQualification::Access(_) = consumed.sealed().qualification();
        self.recheck_consumed_access_reply_on(store, transaction, consumed)?;
        Ok(())
    }
}
