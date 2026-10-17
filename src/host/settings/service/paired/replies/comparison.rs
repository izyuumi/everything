use std::fmt;

use rusqlite::Transaction;
use tokio::sync::OwnedMutexGuard;

use crate::devices::{
    OwnedQueuedSettingsReply, SettingsReplyBuildRequest, SettingsReplyPublicationError,
    SettingsReplyQualification,
};
use crate::protocol::MessageId;
use crate::storage::{CommittedSettingsReply, ConsumedSettingsReply, Store};

use super::super::access::AccessReplyComparison;
use super::{
    SettingsQueuedReplyCheckError, SettingsReceiptAuthority, SettingsReplyProjectionError,
};

pub(crate) struct SettingsReplyComparison {
    message_id: MessageId,
    encrypted_sha256: [u8; 32],
    access: AccessReplyComparison,
}

impl SettingsReceiptAuthority {
    pub(crate) fn recheck_queued_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
    ) -> Result<(), SettingsQueuedReplyCheckError> {
        let committed = checked_queued_reply(queued)?;
        self.recheck_queued_access_reply_on(
            store,
            transaction,
            mutation,
            queued,
            &committed.comparison().access,
        )
    }

    pub(crate) fn capture_reply_comparison_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<SettingsReplyComparison, SettingsReplyPublicationError> {
        self.recheck_consumed_reply_on(store, transaction, consumed)?;
        let sealed = consumed.sealed();
        let SettingsReplyQualification::Access(qualified) = sealed.qualification();
        let access = self
            .capture_access_reply_comparison(qualified)
            .map_err(SettingsReplyProjectionError::from)?;
        Ok(SettingsReplyComparison {
            message_id: sealed.header().message_id,
            encrypted_sha256: *sealed.encrypted_sha256(),
            access,
        })
    }
}

fn checked_queued_reply(
    queued: &OwnedQueuedSettingsReply,
) -> Result<&CommittedSettingsReply, SettingsQueuedReplyCheckError> {
    let SettingsReplyBuildRequest::Access(request) = queued.request()?;
    let committed = queued.committed()?;
    let comparison = committed.comparison();
    let header = committed.header();
    let reply = request.reply_reservation();
    if comparison.message_id != header.message_id
        || comparison.encrypted_sha256 != *committed.encrypted_sha256()
        || header.sender != reply.local_device_id
        || header.recipient != reply.peer_device_id
        || header.created_at_ms < reply.created_at.get()
        || header.created_at_ms >= header.expires_at_ms
        || header.expires_at_ms != reply.expires_at.get()
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    Ok(committed)
}

impl fmt::Debug for SettingsReplyComparison {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyComparison([sealed comparison only])")
    }
}
