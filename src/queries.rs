use std::time::Duration;

use clap::Args;
use everything::agent::AgentError;
use everything::host::client::HostClient;
use everything::ipc::{OwnerQuery, OwnerQueryResult};
use everything::protocol::{ConversationId, TaskId, TaskStatus};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::{CliError, write_json};

#[derive(Args)]
pub struct CancelArguments {
    task_id: TaskId,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct TasksArguments {
    #[arg(long)]
    conversation: Option<ConversationId>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct ApprovalsArguments {
    #[arg(long)]
    conversation: Option<ConversationId>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct ListArguments {
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct MemoryArguments {
    question: String,
    #[arg(long)]
    json: bool,
}

pub async fn cancel(
    client: &HostClient,
    arguments: CancelArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let result = cancel_task(client, arguments.task_id, &cancellation).await?;
    if arguments.json {
        return write_json(result).await;
    }
    print_lines(vec![describe_cancellation(&result)]).await
}

pub async fn cancel_task(
    client: &HostClient,
    task_id: TaskId,
    cancellation: &CancellationToken,
) -> Result<OwnerQueryResult, CliError> {
    let query = || client.query(OwnerQuery::Task { task_id }, cancellation.clone());
    let task = match query().await? {
        OwnerQueryResult::Task { task, .. } => task,
        OwnerQueryResult::LegacyForeground { .. } => {
            return Err(CliError::ToolOptions(
                "stop the host and explicitly settle this legacy foreground task",
            ));
        }
        _ => return Err(CliError::OwnerInput),
    };
    if !task.status.is_terminal() {
        client
            .submit_domain(
                everything::protocol::CommandEnvelope::new(
                    task.conversation_id,
                    None,
                    everything::protocol::UnixMillis::now().map_err(AgentError::from)?,
                    everything::protocol::Command::CancelTask { task_id },
                ),
                cancellation.clone(),
            )
            .await?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = query().await?;
        match &result {
            OwnerQueryResult::Task { task, .. }
                if task.status.is_terminal() || tokio::time::Instant::now() >= deadline =>
            {
                return Ok(result);
            }
            OwnerQueryResult::Task { .. } => {
                tokio::select! {
                    () = cancellation.cancelled() => return Err(AgentError::Cancelled.into()),
                    () = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
            }
            _ => return Err(CliError::OwnerInput),
        }
    }
}

pub fn describe_cancellation(result: &OwnerQueryResult) -> String {
    let OwnerQueryResult::Task { task, execution } = result else {
        return "unexpected owner task response".into();
    };
    let status = label(task.status);
    let mut line = match task.status {
        TaskStatus::Cancelled => format!("host task {} cancelled", task.id),
        status_value if status_value.is_terminal() => {
            format!("host task {} ended {status}", task.id)
        }
        _ if execution.cancel_requested => format!(
            "host task {} cancellation requested; still {status}, the host stops it at its next boundary",
            task.id
        ),
        _ => format!("host task {} is still {status}", task.id),
    };
    if let Some(failure) = &task.failure {
        line.push_str(&format!(": {} ({})", failure.message, failure.code));
    } else if let Some(diagnostic) = &execution.diagnostic {
        line.push_str(&format!(": {diagnostic}"));
    }
    line
}

pub async fn tasks(
    client: &HostClient,
    arguments: TasksArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let result = client
        .query(
            OwnerQuery::Tasks {
                conversation_id: arguments.conversation,
                limit: arguments.limit,
            },
            cancellation,
        )
        .await?;
    if arguments.json {
        return write_json(result).await;
    }
    let OwnerQueryResult::Tasks { tasks } = result else {
        return Err(CliError::OwnerInput);
    };
    if tasks.is_empty() {
        eprintln!("no tasks");
    }
    print_lines(
        tasks
            .iter()
            .map(|task| {
                format!(
                    "{}\t{}\t{}\t{}\t{}",
                    task.id,
                    label(task.status),
                    time(task.updated_at.get()),
                    task.conversation_id,
                    snippet(&task.instruction)
                )
            })
            .collect(),
    )
    .await
}

pub async fn approvals(
    client: &HostClient,
    arguments: ApprovalsArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let (approvals, more) = match arguments.conversation {
        Some(conversation_id) => {
            let result = client
                .query(
                    OwnerQuery::Approvals {
                        conversation_id,
                        limit: arguments.limit,
                        cursor: None,
                    },
                    cancellation,
                )
                .await?;
            if arguments.json {
                return write_json(result).await;
            }
            let OwnerQueryResult::Approvals {
                approvals,
                next_cursor,
            } = result
            else {
                return Err(CliError::OwnerInput);
            };
            (approvals, next_cursor.is_some())
        }
        None => {
            let OwnerQueryResult::Conversations { conversations } = client
                .query(
                    OwnerQuery::Conversations { limit: 100 },
                    cancellation.clone(),
                )
                .await?
            else {
                return Err(CliError::OwnerInput);
            };
            let mut pending = Vec::new();
            let mut more = false;
            for conversation_id in conversations
                .iter()
                .filter(|conversation| conversation.pending_command_id.is_some())
                .map(|conversation| conversation.id)
            {
                let OwnerQueryResult::Approvals {
                    approvals,
                    next_cursor,
                } = client
                    .query(
                        OwnerQuery::Approvals {
                            conversation_id,
                            limit: arguments.limit,
                            cursor: None,
                        },
                        cancellation.clone(),
                    )
                    .await?
                else {
                    return Err(CliError::OwnerInput);
                };
                more |= next_cursor.is_some();
                pending.extend(approvals);
            }
            if arguments.json {
                return write_json(pending).await;
            }
            (pending, more)
        }
    };
    if approvals.is_empty() {
        eprintln!("no pending approvals");
    }
    if more {
        eprintln!("more approvals exist; pass --conversation and a larger --limit (up to 100)");
    }
    print_lines(
        approvals
            .iter()
            .map(|approval| {
                format!(
                    "{}\t{}\t{}\t{}\t{}",
                    approval.id,
                    approval.tool_name,
                    time(u64::try_from(approval.requested_at).unwrap_or_default()),
                    approval.conversation_id,
                    approval.task_id
                )
            })
            .collect(),
    )
    .await
}

pub async fn conversations(
    client: &HostClient,
    arguments: ListArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let result = client
        .query(
            OwnerQuery::Conversations {
                limit: arguments.limit,
            },
            cancellation,
        )
        .await?;
    if arguments.json {
        return write_json(result).await;
    }
    let OwnerQueryResult::Conversations { conversations } = result else {
        return Err(CliError::OwnerInput);
    };
    if conversations.is_empty() {
        eprintln!("no conversations");
    }
    print_lines(
        conversations
            .iter()
            .map(|conversation| {
                format!(
                    "{}\t{}\t{} messages\t{}/{}\t{}",
                    conversation.id,
                    if conversation.pending_command_id.is_some() {
                        "pending"
                    } else {
                        "idle"
                    },
                    conversation.message_count,
                    label(conversation.provider),
                    conversation.model,
                    snippet(&conversation.title)
                )
            })
            .collect(),
    )
    .await
}

pub async fn schedules(
    client: &HostClient,
    arguments: ListArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    use everything::scheduling::controls::{ScheduleQuery, ScheduleQueryResult};
    let result = client
        .query(
            OwnerQuery::Schedule {
                query: ScheduleQuery::List {
                    after_id: None,
                    enabled: None,
                    include_deleted: false,
                    limit: arguments.limit,
                },
            },
            cancellation,
        )
        .await?;
    if arguments.json {
        return write_json(result).await;
    }
    let OwnerQueryResult::Schedule {
        result: ScheduleQueryResult::Jobs { jobs, next_after },
    } = result
    else {
        return Err(CliError::OwnerInput);
    };
    if jobs.is_empty() {
        eprintln!("no schedules");
    }
    if next_after.is_some() {
        eprintln!("more schedules exist; use `everything schedule list --after-id` to page");
    }
    print_lines(
        jobs.iter()
            .map(|job| {
                format!(
                    "{}\t{}\tnext {}\t{}",
                    job.id,
                    if job.enabled { "enabled" } else { "disabled" },
                    job.next_run_at
                        .map_or_else(|| "-".into(), |at| time(at.get())),
                    snippet(&job.name)
                )
            })
            .collect(),
    )
    .await
}

pub async fn memory(
    client: &HostClient,
    arguments: MemoryArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let result = client
        .query(
            OwnerQuery::RetrieveMemory {
                question: arguments.question,
            },
            cancellation,
        )
        .await?;
    if arguments.json {
        return write_json(result).await;
    }
    let OwnerQueryResult::RetrievedMemory {
        excerpts,
        omitted_hits,
        ..
    } = result
    else {
        return Err(CliError::OwnerInput);
    };
    if excerpts.is_empty() {
        eprintln!("no memory matched");
    }
    if omitted_hits > 0 {
        eprintln!("{omitted_hits} more matches omitted");
    }
    print_lines(
        excerpts
            .iter()
            .map(|excerpt| {
                format!(
                    "{} › {}\n{}\n",
                    excerpt.title,
                    excerpt.heading,
                    excerpt.text.trim_end()
                )
            })
            .collect(),
    )
    .await
}

fn label(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".into())
}

fn time(milliseconds: u64) -> String {
    i64::try_from(milliseconds)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "-".into(),
            |at| {
                at.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            },
        )
}

fn snippet(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(60) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line,
    }
}

async fn print_lines(lines: Vec<String>) -> Result<(), CliError> {
    let mut output = tokio::io::stdout();
    for line in lines {
        output.write_all(line.as_bytes()).await?;
        output.write_all(b"\n").await?;
    }
    output.flush().await?;
    Ok(())
}
