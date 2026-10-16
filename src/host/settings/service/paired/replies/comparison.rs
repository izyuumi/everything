use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::Transaction;

use crate::devices::{SettingsReplyPublicationError, SettingsReplyQualification};
use crate::host::settings::contract::SettingKey;
use crate::host::settings::operations::PairedReplyHistoryAnchor;
use crate::protocol::{MessageId, UnixMillis};
use crate::storage::settings_originals::{
    RemoteCloseReason, RemoteOriginalIdentity, RemoteOriginalLink, RemoteOriginalTerminal,
    RemotePublicCommand,
};
use crate::storage::{ConsumedSettingsReply, Store};

use super::super::QualifiedOriginalReply;
use super::{SettingsReceiptAuthority, SettingsReplyProjectionError, SettingsServiceError};

pub(crate) struct SettingsReplyComparison {
    message_id: MessageId,
    encrypted_sha256: [u8; 32],
    observation: ReplyComparisonObservation,
}

enum ReplyComparisonObservation {
    Original {
        invocation: Arc<()>,
        linked: bool,
        close: Option<OriginalCloseFloor>,
        terminal: Option<RemoteOriginalTerminal>,
        disclosed: DisclosedSettingsKeys,
        operation: Option<PairedReplyHistoryAnchor>,
        wall: UnixMillis,
        monotonic: Instant,
    },
    Bind {
        creation: Arc<()>,
        wall: UnixMillis,
        monotonic: Instant,
    },
}

struct OriginalCloseFloor {
    requested_at: UnixMillis,
    reason: RemoteCloseReason,
}

struct DisclosedSettingsKeys {
    embeddings_enabled: bool,
    embeddings_dimensions: bool,
}

impl SettingsReceiptAuthority {
    pub(crate) fn capture_reply_comparison_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<SettingsReplyComparison, SettingsReplyPublicationError> {
        self.recheck_consumed_reply_on(store, transaction, consumed)?;
        let sealed = consumed.sealed();
        let observation = match sealed.qualification() {
            SettingsReplyQualification::Original(qualified) => {
                capture_original_on(transaction, qualified)
                    .map_err(SettingsReplyProjectionError::from)?
            }
            SettingsReplyQualification::Bind(qualified) => ReplyComparisonObservation::Bind {
                creation: Arc::clone(&qualified.creation),
                wall: qualified.observed_wall,
                monotonic: qualified.observed_monotonic,
            },
        };
        Ok(SettingsReplyComparison {
            message_id: sealed.header().message_id,
            encrypted_sha256: *sealed.encrypted_sha256(),
            observation,
        })
    }
}

fn capture_original_on(
    transaction: &Transaction<'_>,
    qualified: &QualifiedOriginalReply<'_>,
) -> Result<ReplyComparisonObservation, SettingsServiceError> {
    let RemoteOriginalIdentity::Accepted(original) = &qualified.original.identity else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    let RemotePublicCommand::Update {
        enabled,
        dimensions,
        ..
    } = &original.command
    else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    let mut disclosed = DisclosedSettingsKeys {
        embeddings_enabled: enabled.is_some(),
        embeddings_dimensions: dimensions.is_some(),
    };
    let operation = match (&qualified.original.link, &qualified.operation) {
        (None, None) => None,
        (Some(RemoteOriginalLink::Operation { operation_id }), Some(operation))
            if *operation_id == original.request_id
                && operation.identity.binding.request_id == original.request_id =>
        {
            if let Some(write) = operation.write() {
                for key in &write.intent.changed_keys {
                    match key {
                        SettingKey::EmbeddingsEnabled => disclosed.embeddings_enabled = true,
                        SettingKey::EmbeddingsDimensions => disclosed.embeddings_dimensions = true,
                        _ => return Err(SettingsServiceError::CorruptJournal),
                    }
                }
            }
            Some(PairedReplyHistoryAnchor::capture_on(
                transaction,
                operation,
            )?)
        }
        _ => return Err(SettingsServiceError::CorruptJournal),
    };
    if !disclosed.embeddings_enabled && !disclosed.embeddings_dimensions {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(ReplyComparisonObservation::Original {
        invocation: Arc::clone(&qualified.invocation),
        linked: qualified.original.link.is_some(),
        close: qualified
            .original
            .close
            .as_ref()
            .map(|close| OriginalCloseFloor {
                requested_at: close.requested_at,
                reason: close.reason,
            }),
        terminal: qualified.original.terminal,
        disclosed,
        operation,
        wall: qualified.observed_wall,
        monotonic: qualified.observed_monotonic,
    })
}

impl fmt::Debug for SettingsReplyComparison {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyComparison([sealed comparison only])")
    }
}
