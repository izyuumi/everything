use super::credentials::SecretTransfer;
use super::runtime::{HostError, HostInner, timestamp};
use crate::devices::PairingApproval;
use crate::ipc::{
    CommandReceipt, OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult,
    RelayConnectionState, RelayStatus,
};
use crate::protocol::{CommandId, TaskId, UnixMillis};
use crate::scheduler::SchedulerError;
use crate::storage::StorageError;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl HostInner {
    async fn task_snapshot(
        self: &Arc<Self>,
        task_id: TaskId,
    ) -> Result<crate::scheduler::TaskSnapshot, SchedulerError> {
        match self.scheduler.status(task_id).await {
            Err(SchedulerError::Stopped) => {
                let host = self.clone();
                tokio::task::spawn_blocking(move || {
                    crate::scheduler::task_snapshot(&host.agent.store(), task_id)
                })
                .await
                .map_err(|_| SchedulerError::Worker)?
            }
            result => result,
        }
    }
    pub(super) async fn query(
        self: &Arc<Self>,
        query: OwnerQuery,
    ) -> Result<OwnerQueryResult, HostError> {
        match query {
            OwnerQuery::Conversations { limit } => Ok(OwnerQueryResult::Conversations {
                conversations: self
                    .agent
                    .conversations(limit)
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            }),
            OwnerQuery::Conversation { conversation_id } => {
                let conversation = self.agent.conversation(conversation_id).await?;
                Ok(OwnerQueryResult::Conversation {
                    information: conversation.info.into(),
                    configuration: conversation.config,
                })
            }
            OwnerQuery::Tasks {
                conversation_id,
                limit,
            } => {
                let rows = self
                    .blocking(move |host| {
                        Ok(host.agent.store().tasks(
                            conversation_id.map(|id| id.to_string()).as_deref(),
                            None,
                            limit,
                        )?)
                    })
                    .await?;
                let mut tasks = Vec::with_capacity(rows.len());
                for row in rows {
                    let id: TaskId = row.id.parse()?;
                    match self.task_snapshot(id).await {
                        Ok(snapshot) => tasks.push(snapshot.task),
                        Err(SchedulerError::NotOwned(_)) => tasks.push(legacy_task(row)?),
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(OwnerQueryResult::Tasks { tasks })
            }
            OwnerQuery::Task { task_id } => match self.task_snapshot(task_id).await {
                Ok(snapshot) => Ok(OwnerQueryResult::Task {
                    task: snapshot.task,
                    execution: crate::ipc::TaskExecutionMetadata {
                        active_command_id: snapshot.active_command_id,
                        queued_messages: snapshot.queued_messages,
                        cancel_requested: snapshot.cancel_requested,
                        waiting: snapshot.waiting,
                        diagnostic: snapshot.diagnostic,
                    },
                }),
                Err(SchedulerError::NotOwned(_)) => {
                    let row = self
                        .blocking(move |host| {
                            host.agent
                                .store()
                                .task(&task_id.to_string())?
                                .ok_or(HostError::Invalid("unknown task"))
                        })
                        .await?;
                    Ok(OwnerQueryResult::LegacyForeground {
                        task: legacy_task(row)?,
                    })
                }
                Err(error) => Err(error.into()),
            },
            OwnerQuery::Approvals {
                conversation_id,
                limit,
                cursor,
            } => {
                self.blocking(move |host| {
                    let page = host.authorizer.pending_approval_page(
                        &conversation_id.to_string(),
                        cursor.as_ref(),
                        limit,
                        timestamp(UnixMillis::now()?)?,
                    )?;
                    Ok(OwnerQueryResult::Approvals {
                        approvals: page.approvals,
                        next_cursor: page.next_cursor,
                    })
                })
                .await
            }
            OwnerQuery::Approval {
                conversation_id,
                lookup,
            } => {
                self.blocking(move |host| {
                    let now = timestamp(UnixMillis::now()?)?;
                    let approval = match lookup {
                        crate::ipc::ApprovalLookup::Approval(id) => host
                            .authorizer
                            .approval_for_id(&conversation_id.to_string(), &id, now)?,
                        crate::ipc::ApprovalLookup::Operation(id) => host
                            .authorizer
                            .approval_for_operation(&conversation_id.to_string(), &id, now)?,
                    };
                    Ok(OwnerQueryResult::Approval { approval })
                })
                .await
            }
            OwnerQuery::Settings => {
                self.blocking(|host| {
                    Ok(OwnerQueryResult::Settings {
                        settings: host.settings.snapshot()?,
                    })
                })
                .await
            }
            OwnerQuery::Identity => {
                let keys = self.devices.public_keys();
                Ok(OwnerQueryResult::Identity {
                    fingerprint: keys.fingerprint(),
                    keys,
                })
            }
            OwnerQuery::Transport => {
                use crate::devices::RelaySessionStatus;
                let connection = if self.relay_origin.is_none() {
                    RelayConnectionState::Disabled
                } else {
                    match self.relay_status.borrow().clone() {
                        RelaySessionStatus::Stopped => RelayConnectionState::Stopped,
                        RelaySessionStatus::Connecting { .. } => RelayConnectionState::Connecting,
                        RelaySessionStatus::Connected | RelaySessionStatus::Maintenance { .. } => {
                            RelayConnectionState::Connected
                        }
                        RelaySessionStatus::Retrying { .. } => RelayConnectionState::Retrying,
                        RelaySessionStatus::Failed { .. } => RelayConnectionState::Failed,
                    }
                };
                Ok(OwnerQueryResult::Transport {
                    relay: RelayStatus {
                        origin: self.relay_origin.clone(),
                        connection,
                    },
                })
            }
            OwnerQuery::Runtime => Ok(OwnerQueryResult::Runtime {
                state: crate::ipc::HostRuntimeState {
                    draining: self.draining.load(Ordering::Acquire),
                    workers_drained: self.workers_drained.load(Ordering::Acquire),
                    broker_quarantined: self.broker.is_quarantined(),
                },
            }),
            OwnerQuery::ToolRecovery => {
                let _recovery = self.recovery.lock().await;
                let report = self.broker.recovery_report().await?;
                let (running_operations,more_running) = self.blocking(|host| {
                    let store = host.agent.store();
                    let connection = store.connection()?;
                    let mut statement = connection.prepare("SELECT id FROM tool_calls WHERE state='running' ORDER BY id LIMIT 129").map_err(StorageError::from)?;
                    let mut ids = statement.query_map([],|row|row.get::<_,String>(0)).map_err(StorageError::from)?
                        .collect::<Result<Vec<_>,_>>().map_err(StorageError::from)?;
                    let more = ids.len()>128;
                    ids.truncate(128);
                    Ok((ids,more))
                }).await?;
                Ok(OwnerQueryResult::ToolRecovery {
                    report,
                    running_operations,
                    more_running,
                })
            }
            OwnerQuery::Devices { limit } => {
                self.blocking(move |host| {
                    Ok(OwnerQueryResult::Devices {
                        devices: host.agent.store().devices(limit)?,
                    })
                })
                .await
            }
            OwnerQuery::Command { command_id } => {
                self.blocking(move |host| {
                    Ok(OwnerQueryResult::Command {
                        receipt: host.inbox.record(command_id)?.map(|record| CommandReceipt {
                            command_id: record.command_id,
                            conversation_id: record.conversation_id,
                            phase: record.phase,
                            accepted_at: record.accepted_at,
                            applied: record.applied_receipt,
                            result: record.terminal_result,
                            archived: record.original.is_none(),
                        }),
                    })
                })
                .await
            }
        }
    }

    pub(super) async fn operate(
        self: &Arc<Self>,
        operation: OwnerOperation,
        request_id: CommandId,
    ) -> Result<OwnerOperationResult, HostError> {
        if !matches!(
            operation,
            OwnerOperation::AcknowledgeCleanup { .. } | OwnerOperation::ReconcileDrained
        ) {
            self.ensure_accepting()?;
        }
        match operation {
            OwnerOperation::DiscardUnapplied {
                command_id,
                expected_accepted_at,
                operator,
                reason,
            } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                let discard = super::inbox::UnappliedDiscard {
                    command_id,
                    expected_accepted_at,
                    request_id,
                    operator,
                    reason,
                };
                let record = self
                    .blocking(move |host| {
                        Ok(host.inbox.discard_unapplied(&discard, UnixMillis::now()?)?)
                    })
                    .await?;
                self.changed.notify_one();
                self.receipt_changed();
                Ok(OwnerOperationResult::Command {
                    receipt: super::runtime::command_receipt(record),
                })
            }
            OwnerOperation::AcknowledgeCleanup {
                operation_id,
                original_result_digest,
                operator,
                note,
            } => {
                let _recovery = self.recovery.lock().await;
                let report = self
                    .broker
                    .acknowledge_cleanup(&operation_id, &original_result_digest, &operator, &note)
                    .await?;
                self.receipt_changed();
                Ok(OwnerOperationResult::ToolRecovery { report })
            }
            OwnerOperation::ReconcileDrained => {
                let _recovery = self.recovery.lock().await;
                if !self.draining.load(Ordering::Acquire)
                    || !self.workers_drained.load(Ordering::Acquire)
                {
                    return Err(HostError::Invalid(
                        "stranded running operations can be reconciled only after owned scheduler workers drain",
                    ));
                }
                let report = self.broker.recover_after_drain().await?;
                self.receipt_changed();
                Ok(OwnerOperationResult::ToolRecovery { report })
            }
            OwnerOperation::Credential { operation } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.credentials.apply(operation).await?;
                Ok(OwnerOperationResult::Applied)
            }
            OwnerOperation::ReplaceSettings {
                expected_revision,
                settings,
            } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                let old = self.blocking(|host| Ok(host.settings.snapshot()?)).await?;
                let restart_required = old.relay_origin != settings.relay_origin;
                self.blocking(move |host| {
                    Ok(OwnerOperationResult::Settings {
                        settings: host.settings.replace(settings, expected_revision)?,
                        restart_required,
                    })
                })
                .await
            }
            OwnerOperation::BeginPairing { lifetime_ms } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.blocking(move |host| {
                    let display = host
                        .devices
                        .begin_pairing(UnixMillis::now()?, lifetime_ms)?;
                    Ok(OwnerOperationResult::PairingInvitation {
                        pairing_id: display.pairing_id,
                        expires_at_ms: display.expires_at_ms,
                        fingerprint: display.fingerprint,
                        invitation: SecretTransfer::new(display.invitation_json.to_string())?,
                    })
                })
                .await
            }
            OwnerOperation::ApprovePairing {
                request,
                label,
                expected_fingerprint,
                access,
            } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.blocking(move |host| {
                    Ok(OwnerOperationResult::PairingApproved {
                        acceptance: host.devices.approve_pairing(
                            &request,
                            PairingApproval::new(label, expected_fingerprint, access)?,
                            UnixMillis::now()?,
                        )?,
                    })
                })
                .await
            }
            OwnerOperation::RevokeDevice { device_id } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.blocking(move |host| {
                    Ok(host.devices.revoke_device(device_id, UnixMillis::now()?)?)
                })
                .await?;
                self.changed.notify_one();
                Ok(OwnerOperationResult::Applied)
            }
            OwnerOperation::WakeTask { task_id } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.scheduler.wake(task_id).await?;
                Ok(OwnerOperationResult::Applied)
            }
            OwnerOperation::RetryProvider {
                task_id,
                expected_attempt,
            } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.scheduler
                    .resume_provider_once(task_id, expected_attempt, request_id)
                    .await?;
                Ok(OwnerOperationResult::Applied)
            }
            OwnerOperation::ResumeInterrupted {
                task_id,
                expected_command,
                expected_revision,
                expected_status,
            } => {
                let _mutation = self.mutation.lock().await;
                self.ensure_accepting()?;
                self.scheduler
                    .resume_interrupted_once(
                        task_id,
                        expected_command,
                        expected_revision,
                        expected_status,
                        request_id,
                    )
                    .await?;
                Ok(OwnerOperationResult::Applied)
            }
        }
    }
}

fn legacy_task(row: crate::storage::TaskRow) -> Result<crate::protocol::TaskRecord, HostError> {
    let initial: crate::protocol::TaskRecord = serde_json::from_value(row.input)?;
    let current: crate::protocol::TaskRecord = serde_json::from_value(row.checkpoint)?;
    let state_matches = matches!(
        (row.state, current.status),
        (
            crate::storage::TaskState::Queued,
            crate::protocol::TaskStatus::Queued
        ) | (
            crate::storage::TaskState::Running,
            crate::protocol::TaskStatus::Running
        ) | (
            crate::storage::TaskState::Waiting,
            crate::protocol::TaskStatus::AwaitingPermission
                | crate::protocol::TaskStatus::WaitingForChildren
        ) | (
            crate::storage::TaskState::Completed,
            crate::protocol::TaskStatus::Completed
        ) | (
            crate::storage::TaskState::Failed,
            crate::protocol::TaskStatus::Failed
        ) | (
            crate::storage::TaskState::Cancelled,
            crate::protocol::TaskStatus::Cancelled
        )
    );
    if initial.id != current.id
        || initial.conversation_id != current.conversation_id
        || initial.parent_task_id != current.parent_task_id
        || initial.originating_command_id != current.originating_command_id
        || initial.created_at != current.created_at
        || initial.instruction != current.instruction
        || row.id != current.id.to_string()
        || row.conversation_id != current.conversation_id.to_string()
        || row.parent_task_id != current.parent_task_id.map(|id| id.to_string())
        || row.created_at != timestamp(initial.created_at)?
        || row.updated_at != timestamp(current.updated_at)?
        || !state_matches
    {
        return Err(HostError::Invalid(
            "legacy foreground task metadata is inconsistent",
        ));
    }
    Ok(current)
}
