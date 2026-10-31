use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::permissions::ToolScopeRule;
use crate::protocol::{CommandId, ConversationId, TaskId};
use crate::scheduler::{ScheduleCallerContext, Scheduler};
use crate::tools::{
    Tool, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext, ToolOutput,
    ToolRegistry,
};

use super::controls::{
    ControlError, SCHEDULE_CONTROL_NAMES, ScheduleControls, ScheduleJobDraft, ScheduleMutation,
    ScheduleMutationResult, ScheduleQuery,
};
use super::{
    ManualClaim, OccurrenceId, ResultDelivery, RunBudget, RunCursor, RunPolicies,
    SCHEDULE_SCHEMA_VERSION, ScheduleDefinition, ScheduleSpec, ScheduledJobId,
    ScheduledPermissions, ScheduledTarget,
};

const MAX_ARGUMENT_BYTES: usize = 65_536;
const MAX_TOOL_REPLY_BYTES: usize = 28 * 1_024;

pub struct ScheduleTools {
    binding: Mutex<Option<Binding>>,
}

struct Binding {
    id: Uuid,
    controls: Weak<ScheduleControls>,
    scheduler: Scheduler,
}

pub struct ScheduleToolBinding {
    owner: Arc<ScheduleTools>,
    id: Uuid,
}

impl ScheduleTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            binding: Mutex::new(None),
        })
    }

    pub fn register_into(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
    ) -> Result<BTreeMap<String, ToolScopeRule>, ToolError> {
        for name in SCHEDULE_CONTROL_NAMES {
            if registry.descriptor(name).is_ok() {
                return Err(ToolError::DuplicateTool {
                    name: (*name).into(),
                });
            }
        }
        let mut scopes = BTreeMap::new();
        for operation in [
            Operation::Create,
            Operation::List,
            Operation::Show,
            Operation::Update,
            Operation::Disable,
            Operation::Delete,
            Operation::Run,
            Operation::History,
        ] {
            let descriptor = descriptor(operation);
            let name = descriptor.name.clone();
            registry.register(Arc::new(ScheduleTool {
                descriptor,
                operation,
                owner: Arc::downgrade(self),
            }))?;
            scopes.insert(
                name,
                ToolScopeRule::Host {
                    service: "scheduled_jobs".into(),
                },
            );
        }
        Ok(scopes)
    }

    pub fn bind(
        self: &Arc<Self>,
        controls: &Arc<ScheduleControls>,
        scheduler: &Scheduler,
    ) -> Result<ScheduleToolBinding, ControlError> {
        let mut saved = self.binding.lock().map_err(|_| ControlError::Worker)?;
        if saved
            .as_ref()
            .is_some_and(|binding| binding.controls.upgrade().is_some())
        {
            return Err(ControlError::Invalid("schedule tools are already bound"));
        }
        let id = Uuid::new_v4();
        *saved = Some(Binding {
            id,
            controls: Arc::downgrade(controls),
            scheduler: scheduler.clone(),
        });
        Ok(ScheduleToolBinding {
            owner: Arc::clone(self),
            id,
        })
    }

    fn services(&self) -> Result<(Arc<ScheduleControls>, Scheduler), ControlError> {
        let saved = self.binding.lock().map_err(|_| ControlError::Worker)?;
        let binding = saved.as_ref().ok_or(ControlError::Stopped)?;
        let controls = binding.controls.upgrade().ok_or(ControlError::Stopped)?;
        Ok((controls, binding.scheduler.clone()))
    }
}

impl Drop for ScheduleToolBinding {
    fn drop(&mut self) {
        let mut binding = match self.owner.binding.lock() {
            Ok(binding) => binding,
            Err(poisoned) => poisoned.into_inner(),
        };
        if binding
            .as_ref()
            .is_some_and(|current| current.id == self.id)
        {
            *binding = None;
        }
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Create,
    List,
    Show,
    Update,
    Disable,
    Delete,
    Run,
    History,
}

struct ScheduleTool {
    descriptor: ToolDescriptor,
    operation: Operation,
    owner: Weak<ScheduleTools>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobArguments {
    name: String,
    schedule: ScheduleSpec,
    instruction: String,
    #[serde(default)]
    permissions: ScheduledPermissions,
    #[serde(default)]
    budget: RunBudget,
    #[serde(default)]
    policies: RunPolicies,
    #[serde(default)]
    deliver_to_conversation: bool,
    #[serde(default = "enabled")]
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArguments {
    job_id: ScheduledJobId,
    expected_revision: u64,
    job: JobArguments,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionArguments {
    job_id: ScheduledJobId,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowArguments {
    job_id: ScheduledJobId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    after_id: Option<ScheduledJobId>,
    enabled: Option<bool>,
    #[serde(default)]
    include_deleted: bool,
    #[serde(default = "page_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryArguments {
    job_id: ScheduledJobId,
    before: Option<RunCursor>,
    #[serde(default = "page_limit")]
    limit: usize,
}

enum ParsedRequest {
    Create(JobArguments),
    Update(UpdateArguments),
    Disable(RevisionArguments),
    Delete(RevisionArguments),
    Run(RevisionArguments),
    Query(ScheduleQuery),
}

#[async_trait]
impl Tool for ScheduleTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let name = &self.descriptor.name;
        let parsed = match parse_request(self.operation, arguments) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(error_output("invalid_arguments", &error.to_string())),
        };
        if context.cancellation().is_cancelled() {
            return Err(no_effect(
                name,
                "schedule control cancelled before caller admission",
            ));
        }
        let caller: TaskId = match context.task_id().parse() {
            Ok(caller) => caller,
            Err(_) => {
                return Ok(error_output(
                    "invalid_context",
                    "schedule control requires a native task UUID",
                ));
            }
        };
        let operation: CommandId = match context.request_id().parse() {
            Ok(operation) => operation,
            Err(_) => {
                return Ok(error_output(
                    "invalid_context",
                    "schedule control requires a native operation UUID",
                ));
            }
        };
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| no_effect(name, "schedule tool registration is unavailable"))?;
        let (controls, scheduler) = owner
            .services()
            .map_err(|_| no_effect(name, "schedule controls are not bound to this host"))?;
        let caller_context = tokio::select! {
            biased;
            _ = context.cancellation().cancelled() => return Err(no_effect(name, "schedule control cancelled before caller admission")),
            result = scheduler.schedule_context(caller, context.workspace_root().into(), operation, name.clone()) => match result {
                Ok(context) => context,
                Err(error) => return Ok(error_output("outside_scope", &error.to_string())),
            },
        };
        let request = match parsed {
            ParsedRequest::Query(query) => {
                return match controls
                    .query_as_agent(query, caller_context, context.cancellation().child_token())
                    .await
                {
                    Ok(result) => query_output(name, &result),
                    Err(error) => control_error(name, error),
                };
            }
            ParsedRequest::Create(job) => {
                let id = ScheduledJobId::try_from(*operation.as_uuid())
                    .map_err(|_| no_effect(name, "invalid schedule creation identity"))?;
                ScheduleMutation::Create {
                    job: Box::new(
                        job_draft(id, job, &caller_context)
                            .map_err(|_| no_effect(name, "invalid captured schedule target"))?,
                    ),
                }
            }
            ParsedRequest::Update(update) => ScheduleMutation::Update {
                job: Box::new(
                    job_draft(update.job_id, update.job, &caller_context)
                        .map_err(|_| no_effect(name, "invalid captured schedule target"))?,
                ),
                expected_revision: update.expected_revision,
            },
            ParsedRequest::Disable(target) => ScheduleMutation::Disable {
                job_id: target.job_id,
                expected_revision: target.expected_revision,
            },
            ParsedRequest::Delete(target) => ScheduleMutation::Delete {
                job_id: target.job_id,
                expected_revision: target.expected_revision,
            },
            ParsedRequest::Run(target) => ScheduleMutation::Run {
                claim: ManualClaim {
                    job_id: target.job_id,
                    expected_revision: target.expected_revision,
                    occurrence_id: OccurrenceId::try_from(*operation.as_uuid())
                        .map_err(|_| no_effect(name, "invalid manual occurrence identity"))?,
                    command_id: operation,
                },
            },
        };
        match controls
            .mutate_as_agent(
                request,
                caller_context,
                context.cancellation().child_token(),
            )
            .await
        {
            Ok(result) => mutation_output(name, &result),
            Err(error) => control_error(name, error),
        }
    }
}

fn parse_request(operation: Operation, arguments: Value) -> Result<ParsedRequest, ControlError> {
    if serde_json::to_vec(&arguments)
        .map_err(|_| ControlError::Invalid("arguments could not be encoded"))?
        .len()
        > MAX_ARGUMENT_BYTES
    {
        return Err(ControlError::Invalid(
            "schedule arguments exceed 65536 bytes",
        ));
    }
    let invalid = |_| ControlError::Invalid("arguments do not match the scheduling tool schema");
    Ok(match operation {
        Operation::Create => {
            ParsedRequest::Create(serde_json::from_value(arguments).map_err(invalid)?)
        }
        Operation::Update => {
            ParsedRequest::Update(serde_json::from_value(arguments).map_err(invalid)?)
        }
        Operation::Disable => {
            ParsedRequest::Disable(serde_json::from_value(arguments).map_err(invalid)?)
        }
        Operation::Delete => {
            ParsedRequest::Delete(serde_json::from_value(arguments).map_err(invalid)?)
        }
        Operation::Run => ParsedRequest::Run(serde_json::from_value(arguments).map_err(invalid)?),
        Operation::Show => {
            let arguments: ShowArguments = serde_json::from_value(arguments).map_err(invalid)?;
            ParsedRequest::Query(ScheduleQuery::Show {
                job_id: arguments.job_id,
            })
        }
        Operation::List => {
            let arguments: ListArguments = serde_json::from_value(arguments).map_err(invalid)?;
            ParsedRequest::Query(ScheduleQuery::List {
                after_id: arguments.after_id,
                enabled: arguments.enabled,
                include_deleted: arguments.include_deleted,
                limit: arguments.limit,
            })
        }
        Operation::History => {
            let arguments: HistoryArguments = serde_json::from_value(arguments).map_err(invalid)?;
            ParsedRequest::Query(ScheduleQuery::History {
                job_id: arguments.job_id,
                before: arguments.before,
                limit: arguments.limit,
            })
        }
    })
}

fn job_draft(
    id: ScheduledJobId,
    arguments: JobArguments,
    caller: &ScheduleCallerContext,
) -> Result<ScheduleJobDraft, ControlError> {
    let delivery = if arguments.deliver_to_conversation {
        ResultDelivery::Conversation {
            conversation_id: caller
                .authority()
                .conversation_id()
                .parse::<ConversationId>()
                .map_err(|_| ControlError::OutsideScope)?,
        }
    } else {
        ResultDelivery::Isolated
    };
    Ok(ScheduleJobDraft {
        id,
        name: arguments.name,
        enabled: arguments.enabled,
        definition: ScheduleDefinition {
            schema_version: SCHEDULE_SCHEMA_VERSION,
            schedule: arguments.schedule,
            target: ScheduledTarget {
                configuration: caller.configuration().clone(),
                instruction: arguments.instruction,
                workspace_root: caller.authority().workspace_root().into(),
                permissions: arguments.permissions,
                budget: arguments.budget,
                delivery,
                agent: None,
            },
            policies: arguments.policies,
        },
    })
}

fn descriptor(operation: Operation) -> ToolDescriptor {
    let id = json!({"type":"string","format":"uuid","minLength":36,"maxLength":36});
    let revision = json!({"type":"integer","minimum":1,"maximum":9223372036854775807_u64});
    let job = job_schema();
    let (name, description, schema, read_only) = match operation {
        Operation::Create => (
            "schedule_create",
            "Persist a bounded recurring or one-time scheduled native job with the caller's exact provider and workspace; requires exact host approval",
            job,
            false,
        ),
        Operation::Update => (
            "schedule_update",
            "Replace an authorized job definition using its exact revision; existing occurrence history remains immutable",
            json!({"type":"object","properties":{"job_id":id,"expected_revision":revision,"job":job},"required":["job_id","expected_revision","job"],"additionalProperties":false}),
            false,
        ),
        Operation::List => (
            "schedule_list",
            "List only schedule definitions within the caller's actual configuration and authority; returned cursor advances through bounded storage pages",
            json!({"type":"object","properties":{"after_id":id,"enabled":{"type":"boolean"},"include_deleted":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":100}},"additionalProperties":false}),
            true,
        ),
        Operation::Show => (
            "schedule_show",
            "Inspect the exact saved job definition within the caller's authority; private instructions require host approval",
            json!({"type":"object","properties":{"job_id":id},"required":["job_id"],"additionalProperties":false}),
            true,
        ),
        Operation::History => (
            "schedule_history",
            "Read bounded immutable run and attempt history, accounting and outcome digests within the caller's authority; Unknown never authorizes retry",
            json!({"type":"object","properties":{"job_id":id.clone(),"before":{"type":"object","properties":{"claimed_at":{"type":"integer","minimum":0,"maximum":253402300799999_u64},"occurrence_id":id},"required":["claimed_at","occurrence_id"],"additionalProperties":false},"limit":{"type":"integer","minimum":1,"maximum":100}},"required":["job_id"],"additionalProperties":false}),
            true,
        ),
        Operation::Disable => (
            "schedule_disable",
            "Disable future automatic occurrences at an exact revision; this does not cancel existing runs",
            revision_schema(id, revision),
            false,
        ),
        Operation::Delete => (
            "schedule_delete",
            "Soft-delete an authorized schedule at an exact revision while retaining immutable history and current runs",
            revision_schema(id, revision),
            false,
        ),
        Operation::Run => (
            "schedule_run",
            "Durably claim one manual occurrence at an exact job revision, including a disabled job; acceptance is not successful execution",
            revision_schema(id, revision),
            false,
        ),
    };
    ToolDescriptor {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        capabilities: Vec::new(),
        read_only,
        concurrency: ToolConcurrency::Concurrent,
    }
}

fn job_schema() -> Value {
    let schedule = json!({"oneOf":[
        {"type":"object","properties":{"kind":{"const":"once"},"at":{"type":"integer","minimum":0,"maximum":253402300799999_u64}},"required":["kind","at"],"additionalProperties":false},
        {"type":"object","properties":{"kind":{"const":"interval"},"anchor":{"type":"integer","minimum":0,"maximum":253402300799999_u64},"every_ms":{"type":"integer","minimum":1000,"maximum":31622400000_u64}},"required":["kind","anchor","every_ms"],"additionalProperties":false},
        {"type":"object","properties":{"kind":{"const":"cron"},"expression":{"type":"string","minLength":1,"maxLength":512},"time_zone":{"type":"string","minLength":1,"maxLength":128}},"required":["kind","expression","time_zone"],"additionalProperties":false}
    ]});
    let permissions = json!({"type":"object","properties":{"mode":{"type":"string","enum":["read_only","trusted_workspace","full_host"]},"allowed_tools":{"type":"array","items":{"type":"string","pattern":"^[A-Za-z0-9_.-]{1,128}$"},"uniqueItems":true,"maxItems":128}},"required":["mode","allowed_tools"],"additionalProperties":false});
    let budget = json!({"type":"object","properties":{"max_model_requests":{"type":"integer","minimum":1,"maximum":64},"max_tool_calls":{"type":"integer","minimum":0,"maximum":1024},"max_output_tokens":{"type":"integer","minimum":1,"maximum":262144},"timeout_ms":{"type":"integer","minimum":1000,"maximum":86400000}},"required":["max_model_requests","max_tool_calls","max_output_tokens","timeout_ms"],"additionalProperties":false});
    let overlap = json!({"oneOf":[{"type":"object","properties":{"kind":{"const":"skip"}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"allow"},"max_running":{"type":"integer","minimum":1,"maximum":16}},"required":["kind","max_running"],"additionalProperties":false}]});
    let missed = json!({"oneOf":[{"type":"object","properties":{"kind":{"const":"skip"}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"latest"},"within_ms":{"type":"integer","minimum":1000,"maximum":604800000}},"required":["kind","within_ms"],"additionalProperties":false}]});
    let retry = json!({"type":"object","properties":{"max_attempts":{"type":"integer","minimum":1,"maximum":8},"initial_backoff_ms":{"type":"integer","minimum":1000,"maximum":3600000},"max_backoff_ms":{"type":"integer","minimum":1000,"maximum":86400000}},"required":["max_attempts","initial_backoff_ms","max_backoff_ms"],"additionalProperties":false});
    let policies = json!({"type":"object","properties":{"overlap":overlap,"missed":missed,"retry":retry},"required":["overlap","missed","retry"],"additionalProperties":false});
    json!({"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":256},"schedule":schedule,"instruction":{"type":"string","minLength":1,"maxLength":32768},"permissions":permissions,"budget":budget,"policies":policies,"deliver_to_conversation":{"type":"boolean"},"enabled":{"type":"boolean"}},"required":["name","schedule","instruction"],"additionalProperties":false})
}

fn control_error(name: &str, error: ControlError) -> Result<ToolOutput, ToolError> {
    if let ControlError::OutcomeUnknown(message) = error {
        return Err(ToolError::EffectsUnknown {
            name: name.into(),
            message,
            cleanup_completed: false,
        });
    }
    let code = match &error {
        ControlError::OutsideScope => "outside_scope",
        ControlError::Cancelled => "cancelled",
        ControlError::Stopped => "stopped",
        ControlError::Busy => "busy",
        ControlError::ReplyTooLarge => "reply_too_large",
        ControlError::Schedule(super::ScheduleError::RevisionConflict { .. }) => {
            "revision_conflict"
        }
        ControlError::Schedule(super::ScheduleError::Capacity(_)) => "capacity",
        _ => "schedule_control_failed",
    };
    Ok(error_output(code, &error.to_string()))
}

fn mutation_output(name: &str, result: &ScheduleMutationResult) -> Result<ToolOutput, ToolError> {
    let content = serde_json::to_value(result).map_err(|_| ToolError::EffectsUnknown {
        name: name.into(),
        message: "mutation committed but reply could not be encoded".into(),
        cleanup_completed: false,
    })?;
    if serde_json::to_vec(&content)
        .map_err(|_| ToolError::EffectsUnknown {
            name: name.into(),
            message: "mutation committed but reply could not be encoded".into(),
            cleanup_completed: false,
        })?
        .len()
        <= MAX_TOOL_REPLY_BYTES
    {
        return Ok(ToolOutput {
            content,
            is_error: false,
        });
    }
    let receipt = match result {
        ScheduleMutationResult::Job { job } => {
            json!({"kind":"job","job_id":job.id,"revision":job.revision,"generation":job.generation,"enabled":job.enabled,"next_run_at":job.next_run_at,"deleted_at":job.deleted_at})
        }
        ScheduleMutationResult::Run { run, replayed } => {
            json!({"kind":"run","occurrence_id":run.occurrence_id,"job_id":run.job_id,"generation":run.generation,"state":run.state,"replayed":replayed})
        }
    };
    Ok(ToolOutput {
        content: json!({"applied":true,"receipt":receipt,"details_omitted":true,"notice":"Full saved definition is available through owner schedule inspection"}),
        is_error: false,
    })
}

fn query_output(name: &str, result: &impl serde::Serialize) -> Result<ToolOutput, ToolError> {
    let content = serde_json::to_value(result)
        .map_err(|_| no_effect(name, "query reply could not be encoded"))?;
    if serde_json::to_vec(&content)
        .map_err(|_| no_effect(name, "query reply could not be encoded"))?
        .len()
        > MAX_TOOL_REPLY_BYTES
    {
        return Ok(error_output(
            "reply_too_large",
            "Reduce the page size or use owner schedule inspection for a large definition",
        ));
    }
    Ok(ToolOutput {
        content,
        is_error: false,
    })
}

fn error_output(code: &str, message: &str) -> ToolOutput {
    let mut end = message.len().min(2048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    ToolOutput {
        content: json!({"error":{"code":code,"message":&message[..end]}}),
        is_error: true,
    }
}

fn no_effect(name: &str, message: &str) -> ToolError {
    ToolError::FailedWithoutSideEffects {
        name: name.into(),
        message: message.into(),
    }
}
fn enabled() -> bool {
    true
}
fn page_limit() -> usize {
    10
}

fn revision_schema(id: Value, revision: Value) -> Value {
    json!({"type":"object","properties":{"job_id":id,"expected_revision":revision},"required":["job_id","expected_revision"],"additionalProperties":false})
}
