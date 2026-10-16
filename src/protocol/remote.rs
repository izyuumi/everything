use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CommandId, DeviceId, MessageId, ProtocolError, TaskId, UnixMillis};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTaskOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteResultSource {
    Result { result_digest: String },
    Rejected { reason: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTaskUsage {
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reserved_output_tokens: Option<u64>,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RemoteTaskReportWire")]
pub struct RemoteTaskReport {
    pub task_id: TaskId,
    pub command_id: CommandId,
    pub delegation_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: String,
    pub executor: DeviceId,
    pub executor_fingerprint: String,
    pub source_message_id: MessageId,
    pub envelope_digest: String,
    pub packet_digest: String,
    pub source: RemoteResultSource,
    pub outcome: RemoteTaskOutcome,
    pub summary: String,
    pub usage: RemoteTaskUsage,
    pub artifact_count: u16,
    pub quarantine_digest: Option<String>,
    pub finished_at: Option<UnixMillis>,
    pub accepted_at: UnixMillis,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteTaskReportWire {
    task_id: TaskId,
    command_id: CommandId,
    delegation_id: Uuid,
    attempt_id: Uuid,
    manifest_digest: String,
    executor: DeviceId,
    executor_fingerprint: String,
    source_message_id: MessageId,
    envelope_digest: String,
    packet_digest: String,
    source: RemoteResultSource,
    outcome: RemoteTaskOutcome,
    summary: String,
    usage: RemoteTaskUsage,
    artifact_count: u16,
    quarantine_digest: Option<String>,
    finished_at: Option<UnixMillis>,
    accepted_at: UnixMillis,
}

impl RemoteTaskReport {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let valid_source = match &self.source {
            RemoteResultSource::Result { result_digest } => {
                digest(result_digest)
                    && self
                        .quarantine_digest
                        .as_deref()
                        .is_none_or(|value| value == result_digest.as_str())
                    && self.outcome != RemoteTaskOutcome::Rejected
                    && self.finished_at.is_some()
            }
            RemoteResultSource::Rejected { reason } => {
                !reason.is_empty()
                    && reason.len() <= 256
                    && !reason.chars().any(char::is_control)
                    && self.outcome == RemoteTaskOutcome::Rejected
                    && self.finished_at.is_none()
                    && self.usage == RemoteTaskUsage::default()
            }
        };
        if self.delegation_id.is_nil()
            || self.attempt_id.is_nil()
            || !digest(&self.manifest_digest)
            || !digest(&self.envelope_digest)
            || !digest(&self.packet_digest)
            || !fingerprint(&self.executor_fingerprint)
            || self.summary.len() > 8192
            || self.summary.contains('\0')
            || self.artifact_count > 64
            || self
                .quarantine_digest
                .as_deref()
                .is_some_and(|value| !digest(value))
            || self.artifact_count > 0 && self.quarantine_digest.is_none()
            || matches!(
                self.outcome,
                RemoteTaskOutcome::Unknown | RemoteTaskOutcome::Rejected
            ) && (self.artifact_count != 0 || self.quarantine_digest.is_some())
            || !valid_source
            || serde_json::to_vec(self)
                .map_err(|_| ProtocolError::InvalidRemoteResult("report serialization"))?
                .len()
                > 64 * 1024
        {
            return Err(ProtocolError::InvalidRemoteResult(
                "invalid remote result report",
            ));
        }
        Ok(())
    }
}

impl TryFrom<RemoteTaskReportWire> for RemoteTaskReport {
    type Error = ProtocolError;

    fn try_from(value: RemoteTaskReportWire) -> Result<Self, Self::Error> {
        let report = Self {
            task_id: value.task_id,
            command_id: value.command_id,
            delegation_id: value.delegation_id,
            attempt_id: value.attempt_id,
            manifest_digest: value.manifest_digest,
            executor: value.executor,
            executor_fingerprint: value.executor_fingerprint,
            source_message_id: value.source_message_id,
            envelope_digest: value.envelope_digest,
            packet_digest: value.packet_digest,
            source: value.source,
            outcome: value.outcome,
            summary: value.summary,
            usage: value.usage,
            artifact_count: value.artifact_count,
            quarantine_digest: value.quarantine_digest,
            finished_at: value.finished_at,
            accepted_at: value.accepted_at,
        };
        report.validate()?;
        Ok(report)
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn fingerprint(value: &str) -> bool {
    value.len() == 71
        && value.bytes().enumerate().all(|(index, byte)| {
            if index % 9 == 8 {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}
