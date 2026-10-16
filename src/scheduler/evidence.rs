use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::{Conversation, ConversationConfig, LoopStatus, PauseReason};
use crate::protocol::{CommandId, ConversationId, TaskId};

use super::types::{Definition, ExecutionPolicy, SchedulerError};

pub(super) const MAX_TERMINAL_EVIDENCE_BYTES: usize = 1024;
const EVIDENCE_FIELD_BYTES: usize = b",\"terminal_evidence\":".len();

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalTaskEvidence {
    pub(crate) command_id: CommandId,
    binding_digest: String,
    pub(crate) native: TerminalNativeEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalNativeEvidence {
    NoAttempt,
    Observed {
        model_requests: u32,
        tool_calls: u32,
        reserved_output_tokens: Option<u64>,
        deadline_ms: u64,
        effects_unknown: bool,
    },
}

impl TerminalTaskEvidence {
    pub(super) fn capture(
        definition: &Definition,
        command_id: CommandId,
        conversation: &Conversation,
    ) -> Result<Self, SchedulerError> {
        if conversation.info.id != definition.initial.conversation_id
            || conversation.config != definition.configuration
        {
            return Err(SchedulerError::Conflict(definition.initial.id));
        }
        let native = match conversation.native_run() {
            Some(run) if run.task_id == definition.initial.id && run.command_id == command_id => {
                let policy = &definition.execution;
                if run.workspace_root != policy.workspace_root
                    || run.mode != policy.mode
                    || run
                        .allowed_tools
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                        != *policy
                            .allowed_tools
                            .as_ref()
                            .ok_or(SchedulerError::Invalid(
                                "terminal evidence requires captured tools",
                            ))?
                    || policy
                        .limits
                        .absolute_deadline
                        .is_some_and(|deadline| run.deadline_ms > deadline.get())
                {
                    return Err(SchedulerError::Conflict(definition.initial.id));
                }
                TerminalNativeEvidence::Observed {
                    model_requests: run.model_attempts,
                    tool_calls: run.tool_calls,
                    reserved_output_tokens: run.reserved_output_tokens,
                    deadline_ms: run.deadline_ms,
                    effects_unknown: matches!(
                        run.status,
                        LoopStatus::ProviderInFlight { .. }
                            | LoopStatus::ExecutingTools
                            | LoopStatus::Paused {
                                reason: PauseReason::ReconciliationRequired { .. }
                                    | PauseReason::BrokerRejected { .. }
                                    | PauseReason::OperationInFlight { .. }
                                    | PauseReason::ChildOutcomeUnknown { .. }
                            }
                    ),
                }
            }
            None if conversation
                .info
                .pending_command_id
                .is_none_or(|pending| pending == command_id) =>
            {
                TerminalNativeEvidence::NoAttempt
            }
            Some(run)
                if run.command_id != command_id
                    && run.status == LoopStatus::Completed
                    && conversation.info.pending_command_id == Some(command_id) =>
            {
                TerminalNativeEvidence::NoAttempt
            }
            _ => return Err(SchedulerError::Conflict(definition.initial.id)),
        };
        let evidence = Self {
            command_id,
            binding_digest: binding_digest(
                definition.initial.id,
                definition.initial.conversation_id,
                command_id,
                &definition.configuration,
                &definition.execution,
            )?,
            native,
        };
        evidence.validate(definition, command_id)?;
        Ok(evidence)
    }

    pub(super) fn no_attempt(
        definition: &Definition,
        command_id: CommandId,
    ) -> Result<Self, SchedulerError> {
        let evidence = Self {
            command_id,
            binding_digest: binding_digest(
                definition.initial.id,
                definition.initial.conversation_id,
                command_id,
                &definition.configuration,
                &definition.execution,
            )?,
            native: TerminalNativeEvidence::NoAttempt,
        };
        evidence.validate(definition, command_id)?;
        Ok(evidence)
    }

    pub(crate) fn matches(
        &self,
        task_id: TaskId,
        conversation_id: ConversationId,
        command_id: CommandId,
        configuration: &ConversationConfig,
        execution: &ExecutionPolicy,
    ) -> Result<bool, SchedulerError> {
        Ok(self.command_id == command_id
            && self.binding_digest
                == binding_digest(
                    task_id,
                    conversation_id,
                    command_id,
                    configuration,
                    execution,
                )?)
    }

    pub(super) fn validate(
        &self,
        definition: &Definition,
        command_id: CommandId,
    ) -> Result<(), SchedulerError> {
        if self.binding_digest.len() != 64
            || !self
                .binding_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !self.matches(
                definition.initial.id,
                definition.initial.conversation_id,
                command_id,
                &definition.configuration,
                &definition.execution,
            )?
        {
            return Err(SchedulerError::Conflict(definition.initial.id));
        }
        if let TerminalNativeEvidence::Observed {
            model_requests,
            tool_calls,
            reserved_output_tokens,
            deadline_ms,
            ..
        } = &self.native
        {
            let limits = &definition.execution.limits;
            let maximum_output = limits.max_reserved_output_tokens.unwrap_or(
                u64::from(limits.max_model_attempts)
                    * u64::from(definition.execution.budget.reserved_output_tokens),
            );
            if *model_requests > limits.max_model_attempts
                || *tool_calls > limits.max_tool_calls
                || reserved_output_tokens.is_some_and(|tokens| tokens > maximum_output)
                || *deadline_ms > i64::MAX as u64
                || limits
                    .absolute_deadline
                    .is_some_and(|deadline| *deadline_ms > deadline.get())
            {
                return Err(SchedulerError::Conflict(definition.initial.id));
            }
        }
        self.validate_size()
    }

    pub(super) fn validate_size(&self) -> Result<(), SchedulerError> {
        if serde_json::to_vec(self)?
            .len()
            .checked_add(EVIDENCE_FIELD_BYTES)
            .is_none_or(|bytes| bytes > MAX_TERMINAL_EVIDENCE_BYTES)
        {
            return Err(SchedulerError::Capacity("terminal task evidence bytes"));
        }
        Ok(())
    }
}

fn binding_digest(
    task_id: TaskId,
    conversation_id: ConversationId,
    command_id: CommandId,
    configuration: &ConversationConfig,
    execution: &ExecutionPolicy,
) -> Result<String, SchedulerError> {
    let mut digest = Sha256::new();
    digest.update(b"everything.scheduler.terminal-evidence.v1\0");
    digest.update(serde_json::to_vec(&(
        task_id,
        conversation_id,
        command_id,
        configuration,
        execution,
    ))?);
    Ok(format!("{:x}", digest.finalize()))
}
