pub mod acknowledgements;
pub mod attempts;
pub mod clock;
pub mod controls;
pub mod delivery;
pub mod engine;
pub mod execution;
pub mod store;
pub mod tools;

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, PathBuf};
use std::str::FromStr;

use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::agent::{AgentError, ConversationConfig};
use crate::permissions::{ExecutionAuthority, ExecutionMode, PermissionError};
use crate::protocol::{CommandId, ConversationId, TaskId, UnixMillis};
use crate::storage::StorageError;

pub use store::ScheduleRepository;

pub const SCHEDULE_SCHEMA_VERSION: u16 = 1;
pub const MAX_SCHEDULE_DEFINITION_BYTES: usize = 131_072;
pub const MAX_RUN_OUTCOME_BYTES: usize = 32_768;
pub const MAX_SCHEDULE_PAGE_BYTES: usize = 4 * 1_048_576;
const MAX_TIMESTAMP: u64 = 253_402_300_799_999;

#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Configuration(#[from] AgentError),
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error("invalid schedule input: {0}")]
    Invalid(&'static str),
    #[error("scheduled {entity} {id} was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("schedule revision conflict: expected {expected}, found {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("schedule occurrence {0} conflicts with a previous claim")]
    OccurrenceConflict(OccurrenceId),
    #[error("scheduled run {0} cannot make the requested transition")]
    RunStateConflict(OccurrenceId),
    #[error("scheduled job is disabled, deleted, or no longer due")]
    NotDue,
    #[error("scheduling capacity exhausted: {0}")]
    Capacity(&'static str),
    #[error("schedule page exceeds its byte budget; request fewer rows")]
    ReadBudgetExceeded,
}

pub type ScheduleResult<T> = Result<T, ScheduleError>;

macro_rules! schedule_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "Uuid", into = "Uuid")]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = ScheduleError;

            fn try_from(value: Uuid) -> ScheduleResult<Self> {
                if value.is_nil() {
                    return Err(ScheduleError::Invalid("nil schedule identifier"));
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl FromStr for $name {
            type Err = ScheduleError;

            fn from_str(value: &str) -> ScheduleResult<Self> {
                Self::try_from(
                    Uuid::parse_str(value).map_err(|_| {
                        ScheduleError::Invalid("schedule identifier must be a UUID")
                    })?,
                )
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

schedule_id!(ScheduledJobId);
schedule_id!(OccurrenceId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleSpec {
    Once {
        at: UnixMillis,
    },
    Interval {
        anchor: UnixMillis,
        every_ms: u64,
    },
    Cron {
        expression: String,
        time_zone: String,
    },
}

impl ScheduleSpec {
    pub fn validate(&self) -> ScheduleResult<()> {
        match self {
            Self::Once { at } => {
                timestamp(*at)?;
            }
            Self::Interval { anchor, every_ms } => {
                timestamp(*anchor)?;
                if !(1_000..=366 * 24 * 60 * 60 * 1_000).contains(every_ms) {
                    return Err(ScheduleError::Invalid(
                        "interval must be between one second and 366 days",
                    ));
                }
            }
            Self::Cron {
                expression,
                time_zone,
            } => {
                parse_cron(expression)?;
                parse_time_zone(time_zone)?;
            }
        }
        Ok(())
    }

    fn validate_next(&self, next: Option<UnixMillis>) -> ScheduleResult<()> {
        let Some(next) = next else {
            return Ok(());
        };
        timestamp(next)?;
        match self {
            Self::Once { at } if *at != next => {
                return Err(ScheduleError::Invalid(
                    "one-time next run must equal its scheduled instant",
                ));
            }
            Self::Interval { anchor, every_ms }
                if next < *anchor
                    || (next.get() - anchor.get()).checked_rem(*every_ms) != Some(0) =>
            {
                return Err(ScheduleError::Invalid(
                    "interval next run must lie on its anchored timeline",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn parse_cron(expression: &str) -> ScheduleResult<Cron> {
    if expression.trim().is_empty()
        || expression.len() > 512
        || !expression.is_ascii()
        || expression.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ScheduleError::Invalid(
            "cron expression requires 1 to 512 printable ASCII bytes",
        ));
    }
    CronParser::builder()
        .seconds(Seconds::Optional)
        .year(Year::Disallowed)
        .dom_and_dow(false)
        .alternative_weekdays(false)
        .build()
        .parse(expression)
        .map_err(|_| ScheduleError::Invalid("invalid five- or six-field cron expression"))
}

pub fn parse_time_zone(value: &str) -> ScheduleResult<Tz> {
    if value.is_empty() || value.len() > 128 {
        return Err(ScheduleError::Invalid("invalid IANA timezone"));
    }
    value
        .parse()
        .map_err(|_| ScheduleError::Invalid("invalid IANA timezone"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultDelivery {
    #[default]
    Isolated,
    Conversation {
        conversation_id: ConversationId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledPermissions {
    pub mode: ExecutionMode,
    pub allowed_tools: BTreeSet<String>,
}

impl Default for ScheduledPermissions {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::ReadOnly,
            allowed_tools: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBudget {
    pub max_model_requests: u16,
    pub max_tool_calls: u16,
    pub max_output_tokens: u32,
    pub timeout_ms: u64,
}

impl Default for RunBudget {
    fn default() -> Self {
        Self {
            max_model_requests: 12,
            max_tool_calls: 0,
            max_output_tokens: 16_384,
            timeout_ms: 600_000,
        }
    }
}

impl RunBudget {
    fn validate(&self) -> ScheduleResult<()> {
        if !(1..=64).contains(&self.max_model_requests)
            || self.max_tool_calls > 1_024
            || !(1..=262_144).contains(&self.max_output_tokens)
            || !(1_000..=86_400_000).contains(&self.timeout_ms)
        {
            return Err(ScheduleError::Invalid("invalid scheduled-run budgets"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledTarget {
    pub configuration: ConversationConfig,
    pub instruction: String,
    pub workspace_root: PathBuf,
    pub permissions: ScheduledPermissions,
    pub budget: RunBudget,
    pub delivery: ResultDelivery,
}

impl ScheduledTarget {
    pub fn validate(&self) -> ScheduleResult<()> {
        self.configuration.validate()?;
        if self.instruction.trim().is_empty()
            || self.instruction.len() > 32_768
            || self.instruction.contains('\0')
        {
            return Err(ScheduleError::Invalid(
                "scheduled instruction requires 1 to 32768 bytes without NUL",
            ));
        }
        if !self.workspace_root.is_absolute()
            || self
                .workspace_root
                .to_str()
                .is_none_or(|path| path.len() > 4_096 || path.contains('\0'))
            || self
                .workspace_root
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(ScheduleError::Invalid(
                "scheduled workspace must be an absolute UTF-8 path without traversal",
            ));
        }
        if self.permissions.allowed_tools.len() > 128
            || self.permissions.allowed_tools.iter().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
                    })
            })
        {
            return Err(ScheduleError::Invalid(
                "scheduled tool allowlist exceeds its bounds",
            ));
        }
        self.budget.validate()?;
        if self.configuration.max_output_tokens > self.budget.max_output_tokens {
            return Err(ScheduleError::Invalid(
                "one model response exceeds the scheduled output budget",
            ));
        }
        Ok(())
    }

    pub fn validate_authority(&self, authority: &ExecutionAuthority) -> ScheduleResult<()> {
        self.validate()?;
        if self.workspace_root != authority.workspace_root()
            || self.permissions.mode > authority.mode()
            || matches!(self.delivery, ResultDelivery::Conversation { conversation_id } if conversation_id.to_string() != authority.conversation_id())
        {
            return Err(ScheduleError::Invalid(
                "scheduled authority cannot exceed its trusted creator",
            ));
        }
        authority
            .clone()
            .restrict_tools(self.permissions.allowed_tools.clone())?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlapPolicy {
    #[default]
    Skip,
    Allow {
        max_running: u16,
    },
}

impl OverlapPolicy {
    fn limit(&self) -> ScheduleResult<u16> {
        match self {
            Self::Skip => Ok(1),
            Self::Allow { max_running } if (1..=16).contains(max_running) => Ok(*max_running),
            _ => Err(ScheduleError::Invalid(
                "concurrent scheduled runs must be between one and sixteen",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MissedRunPolicy {
    #[default]
    Skip,
    Latest {
        within_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff_ms: 5_000,
            max_backoff_ms: 300_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RunPolicies {
    pub overlap: OverlapPolicy,
    pub missed: MissedRunPolicy,
    pub retry: RetryPolicy,
}

impl RunPolicies {
    fn validate(&self) -> ScheduleResult<()> {
        self.overlap.limit()?;
        if matches!(self.missed, MissedRunPolicy::Latest { within_ms } if !(1_000..=604_800_000).contains(&within_ms))
            || !(1..=8).contains(&self.retry.max_attempts)
            || !(1_000..=3_600_000).contains(&self.retry.initial_backoff_ms)
            || self.retry.max_backoff_ms < self.retry.initial_backoff_ms
            || self.retry.max_backoff_ms > 86_400_000
        {
            return Err(ScheduleError::Invalid("invalid missed-run or retry policy"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDefinition {
    pub schema_version: u16,
    pub schedule: ScheduleSpec,
    pub target: ScheduledTarget,
    pub policies: RunPolicies,
}

impl ScheduleDefinition {
    pub fn validate(&self) -> ScheduleResult<()> {
        if self.schema_version != SCHEDULE_SCHEMA_VERSION {
            return Err(ScheduleError::Invalid(
                "unsupported schedule definition version",
            ));
        }
        self.schedule.validate()?;
        self.target.validate()?;
        self.policies.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: ScheduledJobId,
    pub name: String,
    pub definition: ScheduleDefinition,
    pub generation: u64,
    pub revision: u64,
    pub enabled: bool,
    pub next_run_at: Option<UnixMillis>,
    pub last_run_at: Option<UnixMillis>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub deleted_at: Option<UnixMillis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewScheduledJob {
    pub id: ScheduledJobId,
    pub name: String,
    pub definition: ScheduleDefinition,
    pub enabled: bool,
    pub next_run_at: Option<UnixMillis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledJobEdit {
    pub job: NewScheduledJob,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JobFilter {
    pub after_id: Option<ScheduledJobId>,
    pub enabled: Option<bool>,
    pub include_deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Claimed,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
    Unknown,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Claimed | Self::Running)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunTrigger {
    Scheduled,
    Manual { command_id: CommandId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Overlap,
    Missed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunOutcome {
    Succeeded { summary: String },
    Failed { code: String, message: String },
    Cancelled { reason: String },
    Skipped { reason: SkipReason },
    SkippedMissedRange { through: UnixMillis },
    Unknown { reason: String },
}

impl RunOutcome {
    fn state(&self) -> RunState {
        match self {
            Self::Succeeded { .. } => RunState::Succeeded,
            Self::Failed { .. } => RunState::Failed,
            Self::Cancelled { .. } => RunState::Cancelled,
            Self::Skipped { .. } | Self::SkippedMissedRange { .. } => RunState::Skipped,
            Self::Unknown { .. } => RunState::Unknown,
        }
    }

    fn validate(&self) -> ScheduleResult<()> {
        let text = match self {
            Self::Succeeded { summary } => summary,
            Self::Failed { code, message } => {
                if code.is_empty()
                    || code.len() > 64
                    || !code
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err(ScheduleError::Invalid("invalid scheduled failure code"));
                }
                message
            }
            Self::Cancelled { reason } | Self::Unknown { reason } => reason,
            Self::Skipped { .. } => return Ok(()),
            Self::SkippedMissedRange { through } => {
                timestamp(*through)?;
                return Ok(());
            }
        };
        if text.len() > 16_384 || text.contains('\0') {
            return Err(ScheduleError::Invalid(
                "scheduled outcome text exceeds its bound",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledRun {
    pub occurrence_id: OccurrenceId,
    pub job_id: ScheduledJobId,
    pub generation: u64,
    pub definition: ScheduleDefinition,
    pub scheduled_for: UnixMillis,
    pub trigger: RunTrigger,
    pub initial_task_id: TaskId,
    pub state: RunState,
    pub revision: u64,
    pub claimed_at: UnixMillis,
    pub started_at: Option<UnixMillis>,
    pub finished_at: Option<UnixMillis>,
    pub updated_at: UnixMillis,
    pub outcome: Option<RunOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DueClaim {
    pub job_id: ScheduledJobId,
    pub expected_revision: u64,
    pub occurrence_id: OccurrenceId,
    pub scheduled_for: UnixMillis,
    pub next_run_at: Option<UnixMillis>,
    #[serde(default, skip_serializing_if = "DueDisposition::is_dispatch")]
    pub disposition: DueDisposition,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DueDisposition {
    #[default]
    Dispatch,
    SkipMissedRange {
        through: UnixMillis,
    },
}

impl DueDisposition {
    fn is_dispatch(&self) -> bool {
        matches!(self, Self::Dispatch)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualClaim {
    pub job_id: ScheduledJobId,
    pub expected_revision: u64,
    pub occurrence_id: OccurrenceId,
    pub command_id: CommandId,
}

#[derive(Debug, Clone)]
pub struct ClaimOutcome {
    pub replayed: bool,
    pub run: ScheduledRun,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RunCursor {
    pub claimed_at: UnixMillis,
    pub occurrence_id: OccurrenceId,
}

fn timestamp(value: UnixMillis) -> ScheduleResult<i64> {
    if value.get() > MAX_TIMESTAMP {
        return Err(ScheduleError::Invalid(
            "schedule timestamp exceeds year 9999",
        ));
    }
    Ok(value.get() as i64)
}
