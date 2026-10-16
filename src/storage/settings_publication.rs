use std::any::Any;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use rusqlite::{DropBehavior, Transaction, TransactionBehavior};
use thiserror::Error;

use crate::crypto::EnvelopeHeader;
use crate::devices::{
    SealedSettingsReply, SettingsReplyBuildRequest, SettingsReplyPublicationError,
    SettingsReplySealContext,
};
use crate::host::settings::service::{
    OriginalReplySource, SettingsReceiptAuthority, SettingsReplyComparison,
    SettingsReplyProjectionError,
};

use super::{Store, consume_settings_reply_in};

pub(crate) struct CommittedSettingsReply {
    header: EnvelopeHeader,
    signature: [u8; 64],
    encrypted_sha256: [u8; 32],
    envelope_bytes: usize,
    comparison: SettingsReplyComparison,
}

#[derive(Error)]
pub(crate) enum SettingsReplyPublishFailure {
    #[error("Settings reply failed before opening its publication transaction")]
    BeforeTransaction {
        #[source]
        cause: SettingsReplyPublicationError,
    },
    #[error("Settings reply publication failed and was rolled back")]
    RolledBack {
        #[source]
        cause: SettingsReplyPublicationError,
    },
    #[error("Settings reply publication failed and rollback failed")]
    RollbackFailed {
        #[source]
        cause: SettingsReplyPublicationError,
        rollback: rusqlite::Error,
    },
    #[error("Settings reply publication commit outcome is uncertain")]
    CommitUncertain {
        #[source]
        commit: rusqlite::Error,
        rollback: Result<(), rusqlite::Error>,
    },
    #[error("Settings reply publication panicked and its outcome is uncertain")]
    Panicked {
        payload: Box<dyn Any + Send>,
        rollback: Result<(), rusqlite::Error>,
    },
}

enum PublicationAttemptFailure {
    Publication(Box<SettingsReplyPublicationError>),
    Commit(rusqlite::Error),
}

impl Store {
    pub(crate) fn publish_settings_reply(
        &self,
        context: SettingsReplySealContext<'_>,
        request: &SettingsReplyBuildRequest,
    ) -> Result<CommittedSettingsReply, Box<SettingsReplyPublishFailure>> {
        if !self.shares_connection(context.store) {
            return Err(Box::new(SettingsReplyPublishFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::Conflict,
            }));
        }
        let mut main = self.connection().map_err(|cause| {
            Box::new(SettingsReplyPublishFailure::BeforeTransaction {
                cause: cause.into(),
            })
        })?;
        let transaction = main
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|cause| {
                Box::new(SettingsReplyPublishFailure::BeforeTransaction {
                    cause: cause.into(),
                })
            })?;
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            publish_on(self, &transaction, context, request)
        }));
        match attempt {
            Ok(Ok(committed)) => {
                drop(transaction);
                Ok(committed)
            }
            Ok(Err(PublicationAttemptFailure::Publication(cause))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(match rollback {
                    Ok(()) => SettingsReplyPublishFailure::RolledBack { cause: *cause },
                    Err(rollback) => SettingsReplyPublishFailure::RollbackFailed {
                        cause: *cause,
                        rollback,
                    },
                }))
            }
            Ok(Err(PublicationAttemptFailure::Commit(commit))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsReplyPublishFailure::CommitUncertain {
                    commit,
                    rollback,
                }))
            }
            Err(payload) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsReplyPublishFailure::Panicked {
                    payload,
                    rollback,
                }))
            }
        }
    }
}

fn publish_on(
    store: &Store,
    transaction: &Transaction<'_>,
    context: SettingsReplySealContext<'_>,
    request: &SettingsReplyBuildRequest,
) -> Result<CommittedSettingsReply, PublicationAttemptFailure> {
    match request {
        SettingsReplyBuildRequest::Fresh(request) => {
            let source = request.committed_source();
            let authority = source.receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_original_reply_on(
                    store,
                    transaction,
                    context.mutation,
                    OriginalReplySource::Fresh(request.committed_source()),
                )
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed = SealedSettingsReply::original(
                context,
                transaction,
                &qualified,
                OriginalReplySource::Fresh(request.committed_source()),
            )?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Historical(request) => {
            let authority = request.receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_original_reply_on(
                    store,
                    transaction,
                    context.mutation,
                    OriginalReplySource::Historical(request),
                )
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed = SealedSettingsReply::original(
                context,
                transaction,
                &qualified,
                OriginalReplySource::Historical(request),
            )?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Bind { parent, request } => {
            let authority = context.authority;
            let qualified = authority.qualify_bind_reply_on(
                store,
                transaction,
                context.mutation,
                request,
                parent,
            )?;
            let sealed = SealedSettingsReply::bind(context, transaction, &qualified, request)?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
    }
}

fn commit_sealed_on(
    store: &Store,
    transaction: &Transaction<'_>,
    authority: &SettingsReceiptAuthority,
    sealed: &SealedSettingsReply<'_, '_, '_>,
) -> Result<CommittedSettingsReply, PublicationAttemptFailure> {
    let consumed = consume_settings_reply_in(transaction, sealed)?;
    let comparison = authority.capture_reply_comparison_on(store, transaction, &consumed)?;
    let header = sealed.header().clone();
    let signature = *sealed.signature();
    let encrypted_sha256 = *sealed.encrypted_sha256();
    let envelope_bytes = sealed.item().message.envelope.len();
    authority.recheck_consumed_reply_on(store, transaction, &consumed)?;
    transaction
        .execute_batch("COMMIT")
        .map_err(PublicationAttemptFailure::Commit)?;
    Ok(CommittedSettingsReply {
        header,
        signature,
        encrypted_sha256,
        envelope_bytes,
        comparison,
    })
}

fn rollback_publication(
    store: &Store,
    mut transaction: Transaction<'_>,
) -> Result<(), rusqlite::Error> {
    transaction.set_drop_behavior(DropBehavior::Ignore);
    let rollback = transaction.rollback();
    if rollback.is_err() {
        store.quarantine_connection();
    }
    rollback
}

impl From<SettingsReplyPublicationError> for PublicationAttemptFailure {
    fn from(cause: SettingsReplyPublicationError) -> Self {
        Self::Publication(Box::new(cause))
    }
}

impl From<SettingsReplyProjectionError> for PublicationAttemptFailure {
    fn from(cause: SettingsReplyProjectionError) -> Self {
        SettingsReplyPublicationError::from(cause).into()
    }
}

impl fmt::Debug for CommittedSettingsReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CommittedSettingsReply([observed Main commit only])")
    }
}

impl fmt::Debug for SettingsReplyPublishFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyPublishFailure([retained cause])")
    }
}
