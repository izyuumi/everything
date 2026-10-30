use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agent::{ConversationConfig, RecordedMessage, TextCommand};
use crate::host::chatgpt::{ChatGptOperation, ChatGptStatus};
use crate::host::client::{HostClient, HostClientError};
use crate::host::settings::{HostSettings, ModelCredentialReference, ModelCredentialSource};
use crate::ipc::{
    ApprovalLookup, ConversationSummary, OwnerCommand, OwnerOperation, OwnerOperationResult,
    OwnerQuery, OwnerQueryResult, RequestedRunOptions,
};
use crate::permissions::{ApprovalRequest, ApprovalStatus, ExecutionMode};
use crate::protocol::{
    Command, CommandEnvelope, CommandId, ConversationEvent, ConversationId, ConversationMessage,
    MessageContent, MessageRole, PermissionDecision, SyncCursor, SyncRequest, TaskId, TaskRecord,
    TaskStatus, UnixMillis,
};
use crate::scheduler::WaitingReason;
use crate::transport::local::{LocalRequestPayload, LocalResponsePayload};

#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Host(#[from] HostClientError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(&'static str),
    #[error(
        "cannot reach the Everything host for {}; start it with `everything service start` or `everything serve`",
        .0.display()
    )]
    Unavailable(PathBuf),
    #[error("{0}")]
    Setup(String),
    #[error("cancelled")]
    Interrupted,
    #[error("the terminal UI needs an interactive terminal; run `everything --help` for commands")]
    NoTerminal,
}

#[derive(Clone, Debug)]
pub struct Tools {
    pub workspace: PathBuf,
    pub mode: ExecutionMode,
}

pub enum TurnStart {
    Send {
        conversation_id: ConversationId,
        command_id: CommandId,
        text: String,
        tools: Option<Tools>,
    },
    Decide {
        conversation_id: ConversationId,
        command_id: CommandId,
        request: Box<ApprovalRequest>,
        allow: bool,
    },
    Watch {
        conversation_id: ConversationId,
        command_id: CommandId,
    },
}

pub enum TurnEvent {
    Live(String),
    Note(String),
    Approvals(Vec<ApprovalRequest>),
    Done(String),
    Stopped(String),
    Failed(String),
}

pub struct HostEvent {
    pub generation: u64,
    pub event: TurnEvent,
}

pub enum Readiness {
    Ready,
    Busy(CommandId),
    Waiting(CommandId),
    Stopped,
}

#[derive(Clone)]
pub struct Host {
    client: Arc<HostClient>,
    cancellation: CancellationToken,
}

impl Host {
    pub fn new(state: PathBuf, cancellation: CancellationToken) -> Result<Self, TuiError> {
        Ok(Self {
            client: Arc::new(HostClient::new(state)?),
            cancellation,
        })
    }

    async fn query(&self, query: OwnerQuery) -> Result<OwnerQueryResult, TuiError> {
        Ok(self.client.query(query, self.cancellation.clone()).await?)
    }

    pub async fn conversations(&self, limit: usize) -> Result<Vec<ConversationSummary>, TuiError> {
        match self.query(OwnerQuery::Conversations { limit }).await? {
            OwnerQueryResult::Conversations { conversations } => Ok(conversations),
            _ => Err(TuiError::Invalid("unexpected conversations response")),
        }
    }

    pub async fn conversation(
        &self,
        conversation_id: ConversationId,
    ) -> Result<(ConversationSummary, ConversationConfig), TuiError> {
        match self
            .query(OwnerQuery::Conversation { conversation_id })
            .await?
        {
            OwnerQueryResult::Conversation {
                information,
                configuration,
            } => Ok((information, configuration)),
            _ => Err(TuiError::Invalid("unexpected conversation response")),
        }
    }

    pub async fn initialize(
        &self,
        title: String,
        configuration: ConversationConfig,
    ) -> Result<ConversationSummary, TuiError> {
        let conversation_id = ConversationId::new();
        self.client
            .submit(
                OwnerCommand::Initialize {
                    conversation_id,
                    command_id: CommandId::new(),
                    title,
                    configuration,
                },
                self.cancellation.clone(),
            )
            .await?;
        Ok(self.conversation(conversation_id).await?.0)
    }

    pub async fn messages(
        &self,
        conversation_id: ConversationId,
        through: u64,
    ) -> Result<Vec<RecordedMessage>, TuiError> {
        let mut cursor = SyncCursor {
            conversation_id,
            after_sequence: 0,
        };
        let mut messages = Vec::new();
        let mut count = 0_usize;
        while cursor.after_sequence < through {
            let request = SyncRequest::new(vec![cursor], 256)
                .map_err(|_| TuiError::Invalid("invalid history request"))?;
            let LocalResponsePayload::Synchronized { batch } = self
                .client
                .request(
                    LocalRequestPayload::Synchronize { request },
                    self.cancellation.clone(),
                )
                .await?
            else {
                return Err(TuiError::Invalid("unexpected history response"));
            };
            if batch.events.is_empty() {
                break;
            }
            cursor = batch.next_cursor();
            for event in batch
                .events
                .into_iter()
                .take_while(|event| event.sequence.get() <= through)
            {
                count += 1;
                if count > 65_536 {
                    return Err(TuiError::Invalid("conversation history is too large"));
                }
                if let ConversationEvent::MessageAppended { message } = event.payload {
                    messages.push(RecordedMessage {
                        event_id: event.event_id,
                        sequence: event.sequence,
                        causation_id: event.causation_id,
                        message,
                    });
                }
            }
        }
        Ok(messages)
    }

    pub async fn tasks(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<TaskRecord>, TuiError> {
        match self
            .query(OwnerQuery::Tasks {
                conversation_id: Some(conversation_id),
                limit: 100,
            })
            .await?
        {
            OwnerQueryResult::Tasks { tasks } => Ok(tasks),
            _ => Err(TuiError::Invalid("unexpected tasks response")),
        }
    }

    pub async fn readiness(
        &self,
        information: &ConversationSummary,
    ) -> Result<Readiness, TuiError> {
        let Some(pending) = information.pending_command_id else {
            return Ok(Readiness::Ready);
        };
        let status = self
            .tasks(information.id)
            .await?
            .into_iter()
            .find(|task| task.id.as_uuid() == pending.as_uuid())
            .map(|task| task.status);
        Ok(match status {
            Some(status) if status.is_terminal() => Readiness::Stopped,
            Some(TaskStatus::AwaitingPermission) => Readiness::Waiting(pending),
            _ => Readiness::Busy(pending),
        })
    }

    pub async fn pending_approvals(
        &self,
        conversation_id: ConversationId,
        task_id: Option<TaskId>,
    ) -> Result<Vec<ApprovalRequest>, TuiError> {
        let mut requests = Vec::new();
        let mut cursor = None;
        loop {
            let OwnerQueryResult::Approvals {
                approvals,
                next_cursor,
            } = self
                .query(OwnerQuery::Approvals {
                    conversation_id,
                    limit: 100,
                    cursor,
                })
                .await?
            else {
                return Err(TuiError::Invalid("unexpected approvals response"));
            };
            for summary in approvals.into_iter().filter(|summary| {
                summary.status == ApprovalStatus::Pending
                    && task_id.is_none_or(|task| summary.task_id == task.to_string())
            }) {
                if let OwnerQueryResult::Approval {
                    approval: Some(approval),
                } = self
                    .query(OwnerQuery::Approval {
                        conversation_id,
                        lookup: ApprovalLookup::Approval(summary.id),
                    })
                    .await?
                    && approval.status == ApprovalStatus::Pending
                {
                    requests.push(approval.request);
                }
            }
            match next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(requests),
            }
        }
    }

    pub async fn cancel(
        &self,
        conversation_id: ConversationId,
        task_id: TaskId,
    ) -> Result<(), TuiError> {
        let issued_at = UnixMillis::now().map_err(|_| TuiError::Invalid("system clock"))?;
        self.client
            .submit_domain(
                CommandEnvelope::new(
                    conversation_id,
                    None,
                    issued_at,
                    Command::CancelTask { task_id },
                ),
                self.cancellation.clone(),
            )
            .await?;
        Ok(())
    }

    pub async fn settings(&self) -> Result<HostSettings, TuiError> {
        match self.query(OwnerQuery::Settings).await? {
            OwnerQueryResult::Settings { settings } => Ok(*settings),
            _ => Err(TuiError::Invalid("unexpected settings response")),
        }
    }

    pub async fn operate(
        &self,
        operation: OwnerOperation,
    ) -> Result<OwnerOperationResult, TuiError> {
        Ok(self
            .client
            .operate(operation, CommandId::new(), self.cancellation.clone())
            .await?)
    }

    pub async fn chatgpt(&self, operation: ChatGptOperation) -> Result<ChatGptStatus, TuiError> {
        match self.operate(OwnerOperation::ChatGpt { operation }).await? {
            OwnerOperationResult::ChatGpt { status } => Ok(*status),
            _ => Err(TuiError::Invalid("unexpected ChatGPT response")),
        }
    }

    pub async fn chatgpt_models(&self) -> Result<Vec<(String, String)>, TuiError> {
        let binding = self
            .settings()
            .await?
            .model_credentials
            .into_iter()
            .find(|binding| binding.source == ModelCredentialSource::ChatGpt)
            .ok_or(TuiError::Invalid(
                "no ChatGPT sign-in is selected; type /model MODEL_ID instead",
            ))?;
        self.plan_models(binding).await
    }

    pub async fn plan_models(
        &self,
        binding: ModelCredentialReference,
    ) -> Result<Vec<(String, String)>, TuiError> {
        let OwnerOperationResult::ChatGptModels { models, .. } = self
            .operate(OwnerOperation::ChatGptModels { binding })
            .await?
        else {
            return Err(TuiError::Invalid("unexpected models response"));
        };
        Ok(models
            .into_iter()
            .filter(|model| model.visibility.as_deref() == Some("list"))
            .map(|model| {
                let name = model.display_name.unwrap_or_default();
                (model.slug, name)
            })
            .collect())
    }

    pub async fn run_turn(
        self,
        generation: u64,
        start: TurnStart,
        events: UnboundedSender<HostEvent>,
    ) {
        let send = |event| {
            let _ = events.send(HostEvent { generation, event });
        };
        if let Err(error) = self.drive(start, &send).await {
            send(TurnEvent::Failed(error.to_string()));
        }
    }

    async fn drive(&self, start: TurnStart, send: &impl Fn(TurnEvent)) -> Result<(), TuiError> {
        let (conversation_id, command_id) = match start {
            TurnStart::Send {
                conversation_id,
                command_id,
                text,
                tools,
            } => {
                let options = match tools {
                    Some(tools) => RequestedRunOptions {
                        workspace: Some(tools.workspace),
                        execution_mode: Some(tools.mode),
                        tools: Some(true),
                        ..RequestedRunOptions::default()
                    },
                    None => RequestedRunOptions {
                        tools: Some(false),
                        ..RequestedRunOptions::default()
                    },
                };
                let receipt = self
                    .client
                    .submit(
                        OwnerCommand::Text {
                            command: TextCommand {
                                conversation_id,
                                command_id,
                                text,
                            },
                            options,
                        },
                        self.cancellation.clone(),
                    )
                    .await?;
                if receipt
                    .applied
                    .as_ref()
                    .is_none_or(|applied| applied.task_id.is_none())
                {
                    send(TurnEvent::Done(
                        self.final_text(conversation_id, command_id).await?,
                    ));
                    return Ok(());
                }
                (conversation_id, command_id)
            }
            TurnStart::Decide {
                conversation_id,
                command_id,
                request,
                allow,
            } => {
                let operation_id = request
                    .scope
                    .operation_id
                    .parse()
                    .map_err(|_| TuiError::Invalid("invalid approval operation"))?;
                let decision = if allow {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny
                };
                let issued_at = UnixMillis::now().map_err(|_| TuiError::Invalid("system clock"))?;
                self.client
                    .submit_domain(
                        CommandEnvelope::new(
                            conversation_id,
                            None,
                            issued_at,
                            Command::ResolvePermission {
                                operation_id,
                                decision,
                            },
                        ),
                        self.cancellation.clone(),
                    )
                    .await?;
                (conversation_id, command_id)
            }
            TurnStart::Watch {
                conversation_id,
                command_id,
            } => (conversation_id, command_id),
        };
        let task_id = TaskId::try_from(*command_id.as_uuid())
            .map_err(|_| TuiError::Invalid("invalid task identity"))?;
        let mut streamed = String::new();
        let mut reported = None;
        loop {
            let OwnerQueryResult::Task { task, execution } =
                self.query(OwnerQuery::Task { task_id }).await?
            else {
                return Err(TuiError::Invalid("unexpected task response"));
            };
            let children = task.status == TaskStatus::WaitingForChildren
                && !execution.cancel_requested
                && matches!(execution.waiting, Some(WaitingReason::Children { .. }));
            match task.status {
                TaskStatus::Completed => {
                    send(TurnEvent::Done(
                        self.final_text(conversation_id, command_id).await?,
                    ));
                    return Ok(());
                }
                TaskStatus::AwaitingPermission => {
                    send(TurnEvent::Approvals(
                        self.pending_approvals(conversation_id, Some(task_id))
                            .await?,
                    ));
                    return Ok(());
                }
                TaskStatus::Queued | TaskStatus::Running => {}
                _ if children => {}
                status => {
                    let reason = match (&execution.waiting, &execution.diagnostic) {
                        (_, Some(diagnostic)) => diagnostic.clone(),
                        (Some(waiting), None) => format!("stopped at {waiting:?}"),
                        (None, None) => format!("task {status:?}"),
                    };
                    send(TurnEvent::Stopped(reason));
                    return Ok(());
                }
            }
            if execution.diagnostic != reported {
                if let Some(diagnostic) = &execution.diagnostic {
                    send(TurnEvent::Note(diagnostic.clone()));
                }
                reported = execution.diagnostic;
            }
            if let Some(live) = execution.live_text
                && live.text != streamed
            {
                streamed = live.text;
                send(TurnEvent::Live(streamed.clone()));
            }
            let (information, _) = self.conversation(conversation_id).await?;
            self.client
                .request(
                    LocalRequestPayload::Wait {
                        request: crate::ipc::ConversationWait {
                            conversation_id,
                            after_sequence: information.last_sequence,
                            timeout_ms: 1000,
                        },
                    },
                    self.cancellation.clone(),
                )
                .await?;
        }
    }

    async fn final_text(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
    ) -> Result<String, TuiError> {
        let (information, _) = self.conversation(conversation_id).await?;
        let messages = self
            .messages(conversation_id, information.last_sequence)
            .await?;
        Ok(messages
            .into_iter()
            .rev()
            .find(|message| {
                message.causation_id == Some(command_id)
                    && message.message.role == MessageRole::Assistant
                    && !message
                        .message
                        .content
                        .iter()
                        .any(|content| matches!(content, MessageContent::ToolCall(_)))
            })
            .map(|message| message_text(&message.message))
            .unwrap_or_default())
    }
}

pub fn message_text(message: &ConversationMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
