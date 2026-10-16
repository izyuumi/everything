use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use clap::{ArgGroup, Args, Subcommand};
use rustix::fs::{FileType, OFlags};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::host::client::{HostClient, HostClientError};
use crate::ipc::{OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult};
use crate::protocol::{CommandId, DeviceId, TaskId};
use crate::worker::{WorkerError, WorkerJobBinding};

use super::worker::{
    MAX_WORKER_COORDINATOR_REQUEST_BYTES, WorkerCoordinatorOperation, WorkerCoordinatorQuery,
    WorkerPreparationRequest,
};

const MAX_PUBLIC_KEYS_BYTES: usize = 4096;

#[derive(Args)]
pub struct WorkerCoordinatorArguments {
    #[command(subcommand)]
    command: WorkerCoordinatorCommand,
}

#[derive(Subcommand)]
enum WorkerCoordinatorCommand {
    Status,
    Identity,
    Peers,
    PinPeer {
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        request_id: Option<CommandId>,
    },
    Prepare {
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        #[arg(long)]
        request_id: Option<CommandId>,
    },
    Delegate {
        #[arg(long)]
        ticket: Uuid,
        #[arg(long)]
        manifest_digest: String,
        #[arg(long)]
        executor_fingerprint: String,
        #[arg(long)]
        request_id: CommandId,
    },
    Discard {
        #[arg(long)]
        ticket: Uuid,
        #[arg(long)]
        request_id: Option<CommandId>,
    },
    Jobs {
        #[arg(long)]
        after_delegation_id: Option<Uuid>,
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u8).range(1..=32))]
        limit: u8,
    },
    Inspect(InspectionArguments),
    Cancel {
        #[arg(long)]
        task: TaskId,
        #[arg(long)]
        delegation_id: Uuid,
        #[arg(long)]
        attempt_id: Uuid,
        #[arg(long)]
        manifest_digest: String,
        #[arg(long)]
        request_id: Option<CommandId>,
    },
    Revoke {
        #[arg(long)]
        peer: DeviceId,
        #[arg(long)]
        request_id: Option<CommandId>,
    },
}

#[derive(Args)]
#[command(group(ArgGroup::new("inspection").required(true).args(["task", "delegation"])))]
struct InspectionArguments {
    #[arg(long)]
    task: Option<TaskId>,
    #[arg(long)]
    delegation: Option<Uuid>,
}

#[derive(Debug, Error)]
pub enum WorkerCoordinatorCommandError {
    #[error("invalid worker coordinator command: {0}")]
    Invalid(&'static str),
    #[error("worker coordinator request cancelled; an admitted mutation may still be durable")]
    Cancelled,
    #[error("worker coordinator input worker failed")]
    Worker,
    #[error("worker coordinator response did not match the requested operation")]
    Protocol,
    #[error(transparent)]
    Client(#[from] HostClientError),
    #[error(transparent)]
    WorkerOperation(#[from] WorkerError),
    #[error("worker coordinator response encoding failed")]
    Encoding,
}

pub async fn run(
    arguments: WorkerCoordinatorArguments,
    state_directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCoordinatorCommandError> {
    check_cancelled(&cancellation)?;
    let client = HostClient::new(state_directory)?;
    let (operation, request_id) = match arguments.command {
        WorkerCoordinatorCommand::Status => {
            return query(&client, WorkerCoordinatorQuery::Status, cancellation).await;
        }
        WorkerCoordinatorCommand::Identity => {
            return query(&client, WorkerCoordinatorQuery::Identity, cancellation).await;
        }
        WorkerCoordinatorCommand::Peers => {
            return query(&client, WorkerCoordinatorQuery::Peers, cancellation).await;
        }
        WorkerCoordinatorCommand::Jobs {
            after_delegation_id,
            limit,
        } => {
            return query(
                &client,
                WorkerCoordinatorQuery::Jobs {
                    after_delegation_id,
                    limit: usize::from(limit),
                },
                cancellation,
            )
            .await;
        }
        WorkerCoordinatorCommand::Inspect(arguments) => {
            let request = match (arguments.task, arguments.delegation) {
                (Some(task_id), None) => WorkerCoordinatorQuery::Task { task_id },
                (None, Some(delegation_id)) => WorkerCoordinatorQuery::Delegation { delegation_id },
                _ => {
                    return Err(WorkerCoordinatorCommandError::Invalid(
                        "select one task or delegation inspection",
                    ));
                }
            };
            return query(&client, request, cancellation).await;
        }
        WorkerCoordinatorCommand::PinPeer {
            file,
            fingerprint,
            request_id,
        } => {
            let keys: DevicePublicKeys =
                read_owner_file(file, MAX_PUBLIC_KEYS_BYTES, cancellation.clone()).await?;
            (
                WorkerCoordinatorOperation::PinPeer {
                    keys,
                    expected_fingerprint: fingerprint,
                },
                request_id,
            )
        }
        WorkerCoordinatorCommand::Prepare { file, request_id } => {
            let request: WorkerPreparationRequest = read_owner_file(
                file,
                MAX_WORKER_COORDINATOR_REQUEST_BYTES,
                cancellation.clone(),
            )
            .await?;
            (
                WorkerCoordinatorOperation::Prepare {
                    request: Box::new(request),
                },
                request_id,
            )
        }
        WorkerCoordinatorCommand::Delegate {
            ticket,
            manifest_digest,
            executor_fingerprint,
            request_id,
        } => (
            WorkerCoordinatorOperation::Delegate {
                ticket,
                expected_manifest_digest: manifest_digest,
                expected_executor_fingerprint: executor_fingerprint,
            },
            Some(request_id),
        ),
        WorkerCoordinatorCommand::Discard { ticket, request_id } => {
            (WorkerCoordinatorOperation::Discard { ticket }, request_id)
        }
        WorkerCoordinatorCommand::Cancel {
            task,
            delegation_id,
            attempt_id,
            manifest_digest,
            request_id,
        } => (
            WorkerCoordinatorOperation::Cancel {
                task_id: task,
                binding: WorkerJobBinding {
                    delegation_id,
                    attempt_id,
                    manifest_digest,
                },
            },
            request_id,
        ),
        WorkerCoordinatorCommand::Revoke { peer, request_id } => (
            WorkerCoordinatorOperation::RevokePeer { device_id: peer },
            request_id,
        ),
    };
    operate(
        &client,
        operation,
        request_id.unwrap_or_default(),
        cancellation,
    )
    .await
}

async fn query(
    client: &HostClient,
    query: WorkerCoordinatorQuery,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCoordinatorCommandError> {
    query.validate()?;
    check_cancelled(&cancellation)?;
    let reply = client
        .query(
            OwnerQuery::WorkerCoordinator {
                query: query.clone(),
            },
            cancellation,
        )
        .await?;
    if let OwnerQueryResult::WorkerCoordinator { result } = &reply {
        result.validate_for(&query)?;
    } else {
        return Err(WorkerCoordinatorCommandError::Protocol);
    }
    serde_json::to_value(reply).map_err(|_| WorkerCoordinatorCommandError::Encoding)
}

async fn operate(
    client: &HostClient,
    operation: WorkerCoordinatorOperation,
    request_id: CommandId,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCoordinatorCommandError> {
    operation.validate()?;
    check_cancelled(&cancellation)?;
    eprintln!(
        "worker coordinator request {}; preserve the original identity and payload after an ambiguous response",
        json!({"request_id": request_id, "operation": mutation_identity(&operation)})
    );
    let reply = client
        .operate(
            OwnerOperation::WorkerCoordinator {
                operation: Box::new(operation.clone()),
            },
            request_id,
            cancellation,
        )
        .await?;
    if let OwnerOperationResult::WorkerCoordinator { result } = &reply {
        result.validate_for(&operation, request_id)?;
    } else {
        return Err(WorkerCoordinatorCommandError::Protocol);
    }
    serde_json::to_value(reply).map_err(|_| WorkerCoordinatorCommandError::Encoding)
}

fn mutation_identity(operation: &WorkerCoordinatorOperation) -> Value {
    match operation {
        WorkerCoordinatorOperation::PinPeer {
            keys,
            expected_fingerprint,
        } => {
            json!({"kind":"pin_peer","device_id":keys.device_id,"expected_fingerprint":expected_fingerprint})
        }
        WorkerCoordinatorOperation::Prepare { request } => {
            json!({"kind":"prepare","executor":request.executor})
        }
        WorkerCoordinatorOperation::Discard { ticket } => json!({"kind":"discard","ticket":ticket}),
        WorkerCoordinatorOperation::Delegate {
            ticket,
            expected_manifest_digest,
            expected_executor_fingerprint,
        } => {
            json!({"kind":"delegate","ticket":ticket,"expected_manifest_digest":expected_manifest_digest,"expected_executor_fingerprint":expected_executor_fingerprint})
        }
        WorkerCoordinatorOperation::Cancel { task_id, binding } => {
            json!({"kind":"cancel","task_id":task_id,"binding":binding})
        }
        WorkerCoordinatorOperation::RevokePeer { device_id } => {
            json!({"kind":"revoke_peer","device_id":device_id})
        }
    }
}

async fn read_owner_file<T: DeserializeOwned + Send + 'static>(
    path: PathBuf,
    limit: usize,
    cancellation: CancellationToken,
) -> Result<T, WorkerCoordinatorCommandError> {
    check_cancelled(&cancellation)?;
    tokio::task::spawn_blocking(move || {
        check_cancelled(&cancellation)?;
        if path.to_str().is_none_or(|path| path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control)) {
            return Err(WorkerCoordinatorCommandError::Invalid("invalid owner input path"));
        }
        let descriptor = rustix::fs::open(path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, rustix::fs::Mode::empty())
            .map_err(|_| WorkerCoordinatorCommandError::Invalid("owner input cannot be opened safely"))?;
        let metadata = rustix::fs::fstat(&descriptor).map_err(|_| WorkerCoordinatorCommandError::Invalid("owner input metadata unavailable"))?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile || metadata.st_uid != rustix::process::geteuid().as_raw() || metadata.st_nlink != 1 || metadata.st_mode & 0o022 != 0 || metadata.st_size < 0 || metadata.st_size > limit as i64 {
            return Err(WorkerCoordinatorCommandError::Invalid("owner input must be an owner-controlled single-link regular file within its byte limit"));
        }
        let mut bytes = Vec::new();
        File::from(descriptor).take(limit as u64 + 1).read_to_end(&mut bytes).map_err(|_| WorkerCoordinatorCommandError::Invalid("owner input cannot be read"))?;
        if bytes.len() > limit { return Err(WorkerCoordinatorCommandError::Invalid("owner input exceeds its byte limit")); }
        let value = serde_json::from_slice(&bytes).map_err(|_| WorkerCoordinatorCommandError::Invalid("owner input JSON or fields are invalid"))?;
        check_cancelled(&cancellation)?;
        Ok(value)
    }).await.map_err(|_| WorkerCoordinatorCommandError::Worker)?
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), WorkerCoordinatorCommandError> {
    if cancellation.is_cancelled() {
        Err(WorkerCoordinatorCommandError::Cancelled)
    } else {
        Ok(())
    }
}
