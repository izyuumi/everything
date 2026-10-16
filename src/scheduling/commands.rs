use std::io::{self, Read};
use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::host::client::{HostClient, HostClientError};
use crate::ipc::{OwnerOperation, OwnerQuery};
use crate::protocol::{CommandId, UnixMillis};

use super::acknowledgements::UnknownAcknowledgementRequest;
use super::controls::{ScheduleJobDraft, ScheduleMutation, ScheduleQuery};
use super::{ManualClaim, OccurrenceId, RunCursor, ScheduleError, ScheduledJobId};

const MAX_DRAFT_BYTES: u64 = 256 * 1024;
const MAX_TIMESTAMP: u64 = 253_402_300_799_999;

#[derive(Args)]
pub struct ScheduleArguments {
    #[command(subcommand)]
    command: ScheduleCommand,
}

#[derive(Subcommand)]
enum ScheduleCommand {
    Create {
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        #[command(flatten)]
        request: RequestArguments,
    },
    List {
        #[arg(long)]
        after_id: Option<ScheduledJobId>,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: Option<bool>,
        #[arg(long)]
        include_deleted: bool,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
    Show {
        job_id: ScheduledJobId,
    },
    Update {
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        #[command(flatten)]
        revision: RevisionArguments,
        #[command(flatten)]
        request: RequestArguments,
    },
    Disable {
        job_id: ScheduledJobId,
        #[command(flatten)]
        revision: RevisionArguments,
        #[command(flatten)]
        request: RequestArguments,
    },
    Delete {
        job_id: ScheduledJobId,
        #[command(flatten)]
        revision: RevisionArguments,
        #[command(flatten)]
        request: RequestArguments,
    },
    Run {
        job_id: ScheduledJobId,
        #[command(flatten)]
        revision: RevisionArguments,
        #[arg(long)]
        occurrence_id: Option<OccurrenceId>,
        #[arg(long)]
        command_id: Option<CommandId>,
        #[command(flatten)]
        request: RequestArguments,
    },
    History {
        job_id: ScheduledJobId,
        #[arg(long, requires = "before_occurrence_id", value_parser = clap::value_parser!(u64).range(0..=MAX_TIMESTAMP))]
        before_claimed_at: Option<u64>,
        #[arg(long, requires = "before_claimed_at")]
        before_occurrence_id: Option<OccurrenceId>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
    Status {
        #[arg(long)]
        occurrence_id: Option<OccurrenceId>,
    },
    AcknowledgeUnknown {
        occurrence_id: OccurrenceId,
        #[arg(long, value_parser = parse_digest)]
        outcome_digest: String,
        #[arg(long, value_parser = parse_note)]
        note: String,
        #[arg(long)]
        command_id: Option<CommandId>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=MAX_TIMESTAMP))]
        accepted_at_ms: Option<u64>,
        #[command(flatten)]
        request: RequestArguments,
    },
}

#[derive(Args)]
struct RequestArguments {
    #[arg(long)]
    request_id: Option<CommandId>,
}

#[derive(Args)]
struct RevisionArguments {
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    expected_revision: u64,
}

#[derive(Debug, Error)]
pub enum CommandError {
    #[error(transparent)]
    Host(#[from] HostClientError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid schedule input: {0}")]
    Invalid(&'static str),
    #[error("schedule input worker failed")]
    Worker,
    #[error("schedule request cancelled before dispatch")]
    Cancelled,
}

pub async fn run(
    arguments: ScheduleArguments,
    state: PathBuf,
    cancellation: CancellationToken,
) -> Result<Value, CommandError> {
    if cancellation.is_cancelled() {
        return Err(CommandError::Cancelled);
    }
    let client = HostClient::new(state)?;
    let (mutation, request) = match arguments.command {
        ScheduleCommand::Create { file, request } => (
            ScheduleMutation::Create {
                job: Box::new(read_draft(file, cancellation.clone()).await?),
            },
            request,
        ),
        ScheduleCommand::Update {
            file,
            revision,
            request,
        } => (
            ScheduleMutation::Update {
                job: Box::new(read_draft(file, cancellation.clone()).await?),
                expected_revision: revision.expected_revision,
            },
            request,
        ),
        ScheduleCommand::Disable {
            job_id,
            revision,
            request,
        } => (
            ScheduleMutation::Disable {
                job_id,
                expected_revision: revision.expected_revision,
            },
            request,
        ),
        ScheduleCommand::Delete {
            job_id,
            revision,
            request,
        } => (
            ScheduleMutation::Delete {
                job_id,
                expected_revision: revision.expected_revision,
            },
            request,
        ),
        ScheduleCommand::Run {
            job_id,
            revision,
            occurrence_id,
            command_id,
            request,
        } => (
            ScheduleMutation::Run {
                claim: ManualClaim {
                    job_id,
                    expected_revision: revision.expected_revision,
                    occurrence_id: occurrence_id.unwrap_or_default(),
                    command_id: command_id.unwrap_or_default(),
                },
            },
            request,
        ),
        ScheduleCommand::List {
            after_id,
            enabled,
            include_deleted,
            limit,
        } => {
            return query(
                &client,
                ScheduleQuery::List {
                    after_id,
                    enabled,
                    include_deleted,
                    limit: usize::from(limit),
                },
                cancellation,
            )
            .await;
        }
        ScheduleCommand::Show { job_id } => {
            return query(&client, ScheduleQuery::Show { job_id }, cancellation).await;
        }
        ScheduleCommand::History {
            job_id,
            before_claimed_at,
            before_occurrence_id,
            limit,
        } => {
            let before = match (before_claimed_at, before_occurrence_id) {
                (Some(at), Some(occurrence_id)) => Some(RunCursor {
                    claimed_at: UnixMillis::try_from(at)?,
                    occurrence_id,
                }),
                (None, None) => None,
                _ => return Err(CommandError::Invalid("history cursor requires both fields")),
            };
            return query(
                &client,
                ScheduleQuery::History {
                    job_id,
                    before,
                    limit: usize::from(limit),
                },
                cancellation,
            )
            .await;
        }
        ScheduleCommand::Status { occurrence_id } => {
            return match occurrence_id {
                Some(occurrence_id) => {
                    query(&client, ScheduleQuery::Run { occurrence_id }, cancellation).await
                }
                None => Ok(serde_json::to_value(
                    client
                        .query(OwnerQuery::ScheduleEngine, cancellation)
                        .await?,
                )?),
            };
        }
        ScheduleCommand::AcknowledgeUnknown {
            occurrence_id,
            outcome_digest,
            note,
            command_id,
            accepted_at_ms,
            request,
        } => {
            let request_id = request.request_id.unwrap_or_default();
            let acknowledgement = UnknownAcknowledgementRequest {
                occurrence_id,
                command_id: command_id.unwrap_or_default(),
                outcome_digest,
                note,
                accepted_at: match accepted_at_ms {
                    Some(at) => UnixMillis::try_from(at)?,
                    None => UnixMillis::now()?,
                },
            };
            eprintln!(
                "schedule acknowledgement {}; preserve every field for an exact retry",
                json!({"request_id": request_id, "request": acknowledgement})
            );
            return Ok(serde_json::to_value(
                client
                    .operate(
                        OwnerOperation::AcknowledgeScheduledUnknown {
                            request: acknowledgement,
                        },
                        request_id,
                        cancellation,
                    )
                    .await?,
            )?);
        }
    };
    if cancellation.is_cancelled() {
        return Err(CommandError::Cancelled);
    }
    let request_id = request.request_id.unwrap_or_default();
    let identity = match &mutation {
        ScheduleMutation::Create { job } => json!({"kind": "create", "job_id": job.id}),
        ScheduleMutation::Update {
            job,
            expected_revision,
        } => json!({"kind": "update", "job_id": job.id, "expected_revision": expected_revision}),
        ScheduleMutation::Disable {
            job_id,
            expected_revision,
        } => json!({"kind": "disable", "job_id": job_id, "expected_revision": expected_revision}),
        ScheduleMutation::Delete {
            job_id,
            expected_revision,
        } => json!({"kind": "delete", "job_id": job_id, "expected_revision": expected_revision}),
        ScheduleMutation::Run { claim } => json!({"kind": "run", "claim": claim}),
    };
    eprintln!(
        "schedule request {}; inspect state after an ambiguous error; manual replay requires the exact original claim",
        json!({"request_id": request_id, "mutation": identity})
    );
    Ok(serde_json::to_value(
        client
            .operate(
                OwnerOperation::Schedule { mutation },
                request_id,
                cancellation,
            )
            .await?,
    )?)
}

async fn query(
    client: &HostClient,
    query: ScheduleQuery,
    cancellation: CancellationToken,
) -> Result<Value, CommandError> {
    Ok(serde_json::to_value(
        client
            .query(OwnerQuery::Schedule { query }, cancellation)
            .await?,
    )?)
}

async fn read_draft(
    path: PathBuf,
    cancellation: CancellationToken,
) -> Result<ScheduleJobDraft, CommandError> {
    let draft = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::from(
            rustix::fs::open(
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_DRAFT_BYTES {
            return Err(CommandError::Invalid(
                "draft must be a regular file of at most 256 KiB",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_DRAFT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_DRAFT_BYTES {
            return Err(CommandError::Invalid("draft exceeds 256 KiB"));
        }
        let draft: ScheduleJobDraft = serde_json::from_slice(&bytes)
            .map_err(|_| CommandError::Invalid("draft must be valid ScheduleJobDraft JSON"))?;
        draft.definition.validate()?;
        Ok(draft)
    })
    .await
    .map_err(|_| CommandError::Worker)??;
    if cancellation.is_cancelled() {
        return Err(CommandError::Cancelled);
    }
    Ok(draft)
}

fn parse_digest(value: &str) -> Result<String, &'static str> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("outcome digest must contain 64 lowercase hexadecimal characters");
    }
    Ok(value.to_owned())
}

fn parse_note(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > 1024 || value.contains('\0') {
        return Err("note requires 1 to 1024 bytes without NUL");
    }
    Ok(value.to_owned())
}
