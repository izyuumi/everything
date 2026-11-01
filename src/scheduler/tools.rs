use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::permissions::{ExecutionMode, ToolScopeRule};
use crate::protocol::{CommandId, TaskId, TaskStatus, UnixMillis};
use crate::tools::{
    Tool, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext, ToolOutput,
    ToolRegistry,
};

use super::actor::{HandleInner, Scheduler};
use super::{SchedulerError, TaskSnapshot, WaitingReason};

pub const SCHEDULER_TOOL_NAMES: &[&str] = &[
    "spawn_agent",
    "send_agent",
    "agent_status",
    "cancel_agent",
    "wait_agents",
    MESSAGE_AGENT,
];

pub const MESSAGE_AGENT: &str = "message_agent";

const MAX_INSTRUCTION_BYTES: usize = 32_768;
const MAX_PREVIEW_BYTES: usize = 8_192;
const MAX_DIAGNOSTIC_BYTES: usize = 1_024;
const MAX_RESPONSE_BYTES: usize = 65_536;

pub struct SchedulerTools {
    binding: Mutex<Option<Weak<HandleInner>>>,
}

impl SchedulerTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            binding: Mutex::new(None),
        })
    }

    pub fn register_into(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
    ) -> Result<BTreeMap<String, ToolScopeRule>, SchedulerError> {
        let registrations = [
            ControlOperation::Spawn,
            ControlOperation::Send,
            ControlOperation::Status,
            ControlOperation::Cancel,
            ControlOperation::Wait,
            ControlOperation::Message,
        ]
        .map(|operation| {
            Arc::new(SchedulerTool {
                descriptor: descriptor(operation),
                operation,
                owner: Arc::downgrade(self),
            })
        });
        for tool in &registrations {
            if registry.descriptor(&tool.descriptor.name).is_ok() {
                return Err(ToolError::DuplicateTool {
                    name: tool.descriptor.name.clone(),
                }
                .into());
            }
        }
        let mut scopes = BTreeMap::new();
        for tool in registrations {
            let name = tool.descriptor.name.clone();
            registry.register(tool)?;
            scopes.insert(name, ToolScopeRule::RuntimeTask);
        }
        Ok(scopes)
    }

    pub fn bind(
        self: &Arc<Self>,
        scheduler: &Scheduler,
    ) -> Result<SchedulerToolBinding, SchedulerError> {
        let mut binding = self
            .binding
            .lock()
            .map_err(|_| SchedulerError::Invalid("scheduler tool binding is unavailable"))?;
        if binding.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(SchedulerError::Invalid("scheduler tools are already bound"));
        }
        let scheduler = Arc::downgrade(&scheduler.inner);
        *binding = Some(scheduler.clone());
        Ok(SchedulerToolBinding {
            owner: Arc::clone(self),
            scheduler,
        })
    }

    fn scheduler(&self) -> Result<Scheduler, &'static str> {
        self.binding
            .lock()
            .map_err(|_| "scheduler tool binding is unavailable")?
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|inner| Scheduler { inner })
            .ok_or("scheduler tools are not bound to a running host")
    }
}

pub struct SchedulerToolBinding {
    owner: Arc<SchedulerTools>,
    scheduler: Weak<HandleInner>,
}

impl Drop for SchedulerToolBinding {
    fn drop(&mut self) {
        let mut binding = match self.owner.binding.lock() {
            Ok(binding) => binding,
            Err(poisoned) => poisoned.into_inner(),
        };
        if binding
            .as_ref()
            .is_some_and(|current| current.ptr_eq(&self.scheduler))
        {
            *binding = None;
        }
    }
}

#[derive(Clone, Copy)]
enum ControlOperation {
    Spawn,
    Send,
    Status,
    Cancel,
    Wait,
    Message,
}

struct SchedulerTool {
    descriptor: ToolDescriptor,
    operation: ControlOperation,
    owner: Weak<SchedulerTools>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArguments {
    instruction: String,
    mode: Option<ExecutionMode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArguments {
    task_id: String,
    instruction: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArguments {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArguments {
    task_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArguments {
    to: String,
    text: String,
    #[serde(default = "default_wait")]
    wait: bool,
}

fn default_wait() -> bool {
    true
}

enum ControlRequest {
    Wait {
        children: Vec<TaskId>,
    },
    Spawn {
        instruction: String,
        mode: Option<ExecutionMode>,
    },
    Send {
        target: TaskId,
        instruction: String,
    },
    Status {
        target: TaskId,
    },
    Cancel {
        target: TaskId,
    },
    Message {
        to: String,
        text: String,
        wait: bool,
    },
}

#[async_trait]
impl Tool for SchedulerTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let name = &self.descriptor.name;
        let request = match parse_request(self.operation, arguments) {
            Ok(request) => request,
            Err(message) => return Ok(error_output(message)),
        };
        let caller = match task_id(context.task_id()) {
            Ok(caller) => caller,
            Err(message) => return Ok(error_output(message)),
        };
        let operation: CommandId = match context.request_id().parse() {
            Ok(operation) => operation,
            Err(_) => {
                return Ok(error_output(
                    "task control requires a non-nil operation UUID",
                ));
            }
        };
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| no_effect(name, "scheduler tool registration is unavailable"))?;
        let scheduler = owner
            .scheduler()
            .map_err(|message| no_effect(name, message))?;
        if context.cancellation().is_cancelled() {
            return Err(no_effect(
                name,
                "task control was cancelled before actor admission",
            ));
        }
        let workspace = context.workspace_root().to_owned();
        let result = match request {
            ControlRequest::Wait { children } => scheduler
                .tool_wait(caller, workspace, operation, children)
                .await
                .map(snapshot_output),
            ControlRequest::Spawn { instruction, mode } => scheduler
                .tool_spawn(caller, workspace, operation, instruction, mode)
                .await
                .map(snapshot_output),
            ControlRequest::Send {
                target,
                instruction,
            } => scheduler
                .tool_send(caller, workspace, operation, target, instruction)
                .await
                .map(snapshot_output),
            ControlRequest::Status { target } => scheduler
                .tool_status(caller, workspace, target)
                .await
                .map(snapshot_output),
            ControlRequest::Cancel { target } => scheduler
                .tool_cancel(caller, workspace, target)
                .await
                .map(snapshot_output),
            ControlRequest::Message { to, text, wait } => scheduler
                .tool_message(caller, workspace, operation, to.clone(), text, wait)
                .await
                .map(|(snapshot, wait_error)| message_output(snapshot, &to, wait, wait_error)),
        };
        match result {
            Ok(output) => Ok(output),
            Err(error)
                if !matches!(self.operation, ControlOperation::Status)
                    && matches!(error, SchedulerError::MutationUncertain(_)) =>
            {
                Err(ToolError::EffectsUnknown {
                    name: name.clone(),
                    message: bounded(&error.to_string(), 2048),
                    cleanup_completed: false,
                })
            }
            Err(SchedulerError::Delegation { code, message }) => Ok(ToolOutput {
                content: json!({"error":{"code":code,"message":message}}),
                is_error: true,
            }),
            Err(error) => Ok(error_output(&error.to_string())),
        }
    }
}

fn message_output(
    snapshot: TaskSnapshot,
    to: &str,
    wait: bool,
    wait_error: Option<String>,
) -> ToolOutput {
    let mut output = snapshot_output(snapshot);
    if let Value::Object(fields) = &mut output.content {
        fields.insert("to".into(), Value::String(to.to_owned()));
        let reply = match (wait, wait_error) {
            (true, None) => {
                "the final report arrives as an agent_reply input after this tool round".to_owned()
            }
            (true, Some(error)) => format!(
                "the message was admitted but no wait was registered ({}); use wait_agents or agent_status with task_id",
                bounded(&error, MAX_DIAGNOSTIC_BYTES)
            ),
            (false, _) => "not waiting; use wait_agents or agent_status with task_id".to_owned(),
        };
        fields.insert("reply".into(), Value::String(reply));
    }
    output
}

fn descriptor(operation: ControlOperation) -> ToolDescriptor {
    let instruction = json!({"type":"string","minLength":1,"maxLength":MAX_INSTRUCTION_BYTES});
    let target = json!({"type":"string","minLength":36,"maxLength":36,"format":"uuid"});
    let (name, description, input_schema) = match operation {
        ControlOperation::Wait => (
            "wait_agents",
            "Register an explicit wait for up to 32 descendant tasks and return immediately; native execution yields after this known tool round until child results arrive",
            json!({"type":"object","properties":{"task_ids":{"type":"array","items":target,"minItems":1,"maxItems":32,"uniqueItems":true}},"required":["task_ids"],"additionalProperties":false}),
        ),
        ControlOperation::Spawn => (
            "spawn_agent",
            "Admit a native child task with an isolated workspace and inherited or narrower authority; return its current durable state without waiting for completion",
            json!({"type":"object","properties":{"instruction":instruction,"mode":{"type":"string","enum":["read_only","trusted_workspace","full_host"]}},"required":["instruction"],"additionalProperties":false}),
        ),
        ControlOperation::Send => (
            "send_agent",
            "Send an instruction to a task within the caller's permitted descendant scope; return its current durable state without waiting for completion",
            json!({"type":"object","properties":{"task_id":target,"instruction":instruction},"required":["task_id","instruction"],"additionalProperties":false}),
        ),
        ControlOperation::Status => (
            "agent_status",
            "Read bounded durable task state and output within the caller's permitted task scope",
            json!({"type":"object","properties":{"task_id":target},"required":["task_id"],"additionalProperties":false}),
        ),
        ControlOperation::Message => (
            MESSAGE_AGENT,
            "Send a task to another named agent the owner allows this agent to message; it runs with its own tools inside this turn's workspace bounds and returns only its final report as data. wait defaults to true",
            json!({"type":"object","properties":{"to":{"type":"string","minLength":1,"maxLength":32,"pattern":"^[a-z0-9]+(-[a-z0-9]+)*$"},"text":instruction,"wait":{"type":"boolean"}},"required":["to","text"],"additionalProperties":false}),
        ),
        ControlOperation::Cancel => (
            "cancel_agent",
            "Request cancellation within the caller's permitted task scope and return its current durable state; cleanup and uncertain effects can keep the task waiting",
            json!({"type":"object","properties":{"task_id":target},"required":["task_id"],"additionalProperties":false}),
        ),
    };
    ToolDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        capabilities: Vec::new(),
        read_only: matches!(operation, ControlOperation::Status),
        concurrency: ToolConcurrency::Concurrent,
    }
}

fn parse_request(
    operation: ControlOperation,
    arguments: Value,
) -> Result<ControlRequest, &'static str> {
    match operation {
        ControlOperation::Wait => {
            let arguments: WaitArguments = serde_json::from_value(arguments)
                .map_err(|_| "invalid native child wait arguments")?;
            let children = arguments
                .task_ids
                .iter()
                .map(|value| task_id(value))
                .collect::<Result<Vec<_>, _>>()?;
            if children.is_empty()
                || children.len() > 32
                || children
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != children.len()
            {
                return Err("wait requires 1 to 32 distinct descendant task IDs");
            }
            Ok(ControlRequest::Wait { children })
        }
        ControlOperation::Spawn => {
            let arguments: SpawnArguments = serde_json::from_value(arguments)
                .map_err(|_| "invalid native task spawn arguments")?;
            validate_instruction(&arguments.instruction)?;
            Ok(ControlRequest::Spawn {
                instruction: arguments.instruction,
                mode: arguments.mode,
            })
        }
        ControlOperation::Send => {
            let arguments: SendArguments = serde_json::from_value(arguments)
                .map_err(|_| "invalid native task message arguments")?;
            validate_instruction(&arguments.instruction)?;
            Ok(ControlRequest::Send {
                target: task_id(&arguments.task_id)?,
                instruction: arguments.instruction,
            })
        }
        ControlOperation::Message => {
            let arguments: MessageArguments =
                serde_json::from_value(arguments).map_err(|_| "invalid agent message arguments")?;
            validate_instruction(&arguments.text)?;
            crate::host::agents::AgentName::try_from(arguments.to.clone())
                .map_err(|_| "to must be an agent name")?;
            Ok(ControlRequest::Message {
                to: arguments.to,
                text: arguments.text,
                wait: arguments.wait,
            })
        }
        ControlOperation::Status | ControlOperation::Cancel => {
            let arguments: TargetArguments = serde_json::from_value(arguments)
                .map_err(|_| "invalid native task target arguments")?;
            let target = task_id(&arguments.task_id)?;
            Ok(if matches!(operation, ControlOperation::Status) {
                ControlRequest::Status { target }
            } else {
                ControlRequest::Cancel { target }
            })
        }
    }
}

fn validate_instruction(instruction: &str) -> Result<(), &'static str> {
    if instruction.trim().is_empty()
        || instruction.len() > MAX_INSTRUCTION_BYTES
        || instruction.contains('\0')
    {
        return Err("task instruction requires 1 to 32768 UTF-8 bytes without NUL");
    }
    Ok(())
}

fn task_id(value: &str) -> Result<TaskId, &'static str> {
    let id: TaskId = value
        .parse()
        .map_err(|_| "task_id must be a non-nil UUID")?;
    if id.to_string() != value {
        return Err("task_id must use canonical lowercase hyphenated UUID notation");
    }
    Ok(id)
}

#[derive(Serialize)]
struct CompactSnapshot<'a> {
    task_id: TaskId,
    conversation_id: crate::protocol::ConversationId,
    parent_task_id: Option<TaskId>,
    status: TaskStatus,
    updated_at: UnixMillis,
    active_command_id: CommandId,
    queued_messages: usize,
    cancel_requested: bool,
    waiting: Option<&'a WaitingReason>,
    last_output: Option<String>,
    diagnostic: Option<String>,
}

fn snapshot_output(snapshot: TaskSnapshot) -> ToolOutput {
    let compact = CompactSnapshot {
        task_id: snapshot.task.id,
        conversation_id: snapshot.task.conversation_id,
        parent_task_id: snapshot.task.parent_task_id,
        status: snapshot.task.status,
        updated_at: snapshot.task.updated_at,
        active_command_id: snapshot.active_command_id,
        queued_messages: snapshot.queued_messages,
        cancel_requested: snapshot.cancel_requested,
        waiting: snapshot.waiting.as_ref(),
        last_output: snapshot
            .last_output
            .as_ref()
            .map(|output| bounded(output, MAX_PREVIEW_BYTES)),
        diagnostic: snapshot
            .diagnostic
            .as_ref()
            .map(|message| bounded(message, MAX_DIAGNOSTIC_BYTES)),
    };
    let mut writer = ResponseWriter { bytes: Vec::new() };
    if serde_json::to_writer(&mut writer, &compact).is_err() {
        return applied_receipt(&compact);
    }
    match serde_json::from_slice(&writer.bytes) {
        Ok(content) => ToolOutput {
            content,
            is_error: false,
        },
        Err(_) => applied_receipt(&compact),
    }
}

fn applied_receipt(snapshot: &CompactSnapshot<'_>) -> ToolOutput {
    ToolOutput {
        content: json!({"task_id":snapshot.task_id,"status":snapshot.status,"active_command_id":snapshot.active_command_id,"snapshot_truncated":true}),
        is_error: false,
    }
}

struct ResponseWriter {
    bytes: Vec<u8>,
}

impl Write for ResponseWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|size| size > MAX_RESPONSE_BYTES - 128)
        {
            return Err(io::Error::other("native task response size limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn no_effect(name: &str, message: &str) -> ToolError {
    ToolError::FailedWithoutSideEffects {
        name: name.to_owned(),
        message: message.to_owned(),
    }
}

fn error_output(message: &str) -> ToolOutput {
    ToolOutput {
        content: json!({"error":{"code":"scheduler_error","message":bounded(message, MAX_DIAGNOSTIC_BYTES)}}),
        is_error: true,
    }
}

fn bounded(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
