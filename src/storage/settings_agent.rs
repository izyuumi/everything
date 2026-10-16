use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{CommandId, ConversationId, DeviceId, TaskId, UnixMillis};

pub(crate) const MAX_SETTINGS_SESSIONS: usize = 2;
pub(crate) const MAX_SETTINGS_TURNS: usize = 8;
pub(crate) const MAX_SETTINGS_MUTATIONS: usize = 8;
pub(crate) const MAX_SETTINGS_PAGES: u64 = 8192;
pub(crate) const SETTINGS_PAGE_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsSessionReservation {
    pub version: u16,
    pub session_id: Uuid,
    pub host_id: DeviceId,
    pub principal_id: Uuid,
    pub native_session_id: Uuid,
    pub conversation_id: ConversationId,
    pub create_command_id: CommandId,
    pub provider_binding_sha256: String,
    pub tools_sha256: String,
    pub created_at: UnixMillis,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsFencePhase {
    Armed,
    Unknown,
    Settled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsTurnFence {
    pub version: u16,
    pub session_id: Uuid,
    pub command_id: CommandId,
    pub task_id: TaskId,
    pub phase: SettingsFencePhase,
    pub attempt_id: Option<CommandId>,
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reserved_output_tokens: u64,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
    pub started_at: UnixMillis,
    pub deadline: UnixMillis,
    pub last_observed_wall: UnixMillis,
    pub mutation_ids: Vec<CommandId>,
    pub provider_complete: bool,
    pub usage_complete: bool,
}
