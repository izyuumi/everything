use tokio_util::sync::CancellationToken;

use crate::agent::ConversationConfig;
use crate::permissions::ExecutionAuthority;
use crate::protocol::CommandId;

use super::SchedulerError;

pub struct ScheduleCallerContext {
    configuration: ConversationConfig,
    authority: ExecutionAuthority,
    command_id: CommandId,
    operation_id: CommandId,
    tool_name: String,
    cancellation: CancellationToken,
}

impl ScheduleCallerContext {
    pub(super) fn new(
        configuration: ConversationConfig,
        authority: ExecutionAuthority,
        command_id: CommandId,
        operation_id: CommandId,
        tool_name: String,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            configuration,
            authority,
            command_id,
            operation_id,
            tool_name,
            cancellation,
        }
    }

    pub fn configuration(&self) -> &ConversationConfig {
        &self.configuration
    }

    pub fn authority(&self) -> &ExecutionAuthority {
        &self.authority
    }

    pub fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub fn operation_id(&self) -> CommandId {
        self.operation_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.child_token()
    }

    pub fn check_cancelled(&self) -> Result<(), SchedulerError> {
        if self.cancellation.is_cancelled() {
            Err(SchedulerError::OutsideScope)
        } else {
            Ok(())
        }
    }
}
