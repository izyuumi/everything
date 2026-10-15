use rusqlite::{OptionalExtension,params};
use serde::Serialize;
use serde_json::Value;

use crate::permissions::ExecutionAuthority;
use crate::protocol::{TaskId,TaskStatus,UnixMillis};
use crate::storage::{Store,TaskRow,TaskState};

use super::types::*;

const MAX_TASK_BYTES:usize=786_432;
const MAX_RETAINED_TASKS:i64=4096;

pub(super) fn encode(value:&impl Serialize)->Result<Value,SchedulerError>{
    let encoded=serde_json::to_vec(value)?;
    if encoded.len()>MAX_TASK_BYTES{return Err(SchedulerError::Capacity("task checkpoint bytes"));}
    Ok(serde_json::from_slice(&encoded)?)
}

pub(super) fn load(store:&Store,id:TaskId)->Result<StoredTask,SchedulerError>{
    let row=store.task(&id.to_string())?.ok_or(SchedulerError::NotFound(id))?;
    decode(row)
}

fn decode(row:TaskRow)->Result<StoredTask,SchedulerError>{
    let definition:Definition=serde_json::from_value(row.input)?;
    let checkpoint:TaskCheckpoint=serde_json::from_value(row.checkpoint)?;
    definition.configuration.validate()?;
    definition.execution.validate()?;
    let initial=&definition.initial;
    let current=&checkpoint.task;
    if definition.scheduler_version!=1 || checkpoint.scheduler_version!=1 || initial.id.to_string()!=row.id || initial.conversation_id.to_string()!=row.conversation_id || initial.parent_task_id.map(|id|id.to_string())!=row.parent_task_id || initial.status!=TaskStatus::Queued || current.id!=initial.id || current.conversation_id!=initial.conversation_id || current.parent_task_id!=initial.parent_task_id || current.originating_command_id!=initial.originating_command_id || current.instruction!=initial.instruction || current.created_at!=initial.created_at || timestamp(current.updated_at)?!=row.updated_at || timestamp(initial.created_at)?!=row.created_at || state(current.status)!=row.state || checkpoint.queued.len()>32 || checkpoint.resumes.len()>32 || definition.execution.allowed_tools.is_none(){return Err(SchedulerError::Invalid("task checkpoint identity or bounds are invalid"));}
    Ok(StoredTask{definition,checkpoint,state:row.state})
}

pub(super) fn insert(store:&Store,task:&StoredTask,max_pending:usize)->Result<bool,SchedulerError>{
    if let Some(existing)=store.task(&task.definition.initial.id.to_string())?{
        if existing.input!=encode(&task.definition)? {return Err(SchedulerError::Conflict(task.definition.initial.id));}
        decode(existing)?;
        return Ok(false);
    }
    let connection=store.connection()?;
    let (retained,pending):(i64,i64)=connection.query_row("SELECT count(*),coalesce(sum(state IN ('queued','running','waiting')),0) FROM tasks WHERE json_extract(input,'$.scheduler_version')=1",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    drop(connection);
    if retained>=MAX_RETAINED_TASKS || pending>=max_pending as i64{return Err(SchedulerError::Capacity("retained or pending tasks"));}
    let initial=&task.definition.initial;
    store.create_task(&TaskRow{id:initial.id.to_string(),conversation_id:initial.conversation_id.to_string(),parent_task_id:initial.parent_task_id.map(|id|id.to_string()),state:TaskState::Queued,input:encode(&task.definition)?,checkpoint:encode(&task.checkpoint)?,created_at:timestamp(initial.created_at)?,updated_at:timestamp(initial.created_at)?})?;
    Ok(true)
}

pub(super) fn save(store:&Store,task:&mut StoredTask,status:TaskStatus)->Result<(),SchedulerError>{
    let now=UnixMillis::now()?;
    if status!=task.checkpoint.task.status {
        task.checkpoint.task.transition(status,now,if status==TaskStatus::Failed {Some(crate::protocol::TaskFailure {code:"native_task_failed".into(),message:task.checkpoint.diagnostic.clone().unwrap_or_else(||"Native task failed".into()),retryable:false})}else{None})?;
    } else {
        task.checkpoint.task.updated_at=now;
    }
    let next=state(status);
    let payload=encode(&task.checkpoint)?;
    if task.state==next {store.checkpoint_task(&task.definition.initial.id.to_string(),task.state,&payload,timestamp(now)?)?;} else {store.transition_task(&task.definition.initial.id.to_string(),task.state,next,&payload,timestamp(now)?)?;}
    task.state=next;
    Ok(())
}

pub(super) fn state(status:TaskStatus)->TaskState{match status{TaskStatus::Queued=>TaskState::Queued,TaskStatus::Running=>TaskState::Running,TaskStatus::AwaitingPermission|TaskStatus::WaitingForChildren=>TaskState::Waiting,TaskStatus::Completed=>TaskState::Completed,TaskStatus::Failed=>TaskState::Failed,TaskStatus::Cancelled=>TaskState::Cancelled}}

pub(super) fn unfinished(store:&Store,maximum:usize)->Result<Vec<StoredTask>,SchedulerError>{
    let connection=store.connection()?;
    let mut statement=connection.prepare("SELECT id FROM tasks WHERE json_extract(input,'$.scheduler_version')=1 AND state IN ('queued','running','waiting') ORDER BY created_at,id LIMIT ?1")?;
    let ids=statement.query_map([maximum as i64+1],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>().map_err(crate::storage::StorageError::from)?;
    drop(statement);drop(connection);
    if ids.len()>maximum{return Err(SchedulerError::Capacity("recovered pending tasks"));}
    ids.into_iter().map(|id|load(store,id.parse()?)).collect()
}

pub(super) fn descendants(store:&Store,parent:TaskId,maximum:usize)->Result<Vec<TaskId>,SchedulerError>{
    let connection=store.connection()?;
    let mut statement=connection.prepare("WITH RECURSIVE children(id) AS (SELECT id FROM tasks WHERE parent_task_id=?1 UNION SELECT tasks.id FROM tasks JOIN children ON tasks.parent_task_id=children.id) SELECT id FROM children LIMIT ?2")?;
    let ids=statement.query_map(params![parent.to_string(),maximum as i64+1],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>().map_err(crate::storage::StorageError::from)?;
    if ids.len()>maximum{return Err(SchedulerError::Capacity("descendant task count"));}
    ids.into_iter().map(|id|id.parse().map_err(SchedulerError::from)).collect()
}

pub(super) fn authorize_target(store:&Store,caller:TaskId,target:TaskId,allow_self:bool)->Result<(),SchedulerError>{
    if caller==target {return if allow_self{Ok(())}else{Err(SchedulerError::OutsideScope)};}
    let mut current=load(store,target)?;
    for _ in 0..16 {
        match current.definition.initial.parent_task_id {
            Some(parent) if parent==caller=>return Ok(()),
            Some(parent)=>current=load(store,parent)?,
            None=>break,
        }
    }
    Err(SchedulerError::OutsideScope)
}

pub(super) fn authority(store:&Store,id:TaskId)->Result<ExecutionAuthority,SchedulerError>{
    let mut chain=vec![load(store,id)?];
    while let Some(parent)=chain.last().and_then(|task|task.definition.initial.parent_task_id){
        if chain.len()>=16{return Err(SchedulerError::Capacity("subagent nesting"));}
        if chain.iter().any(|task|task.definition.initial.id==parent){return Err(SchedulerError::Invalid("cyclic task ancestry"));}
        chain.push(load(store,parent)?);
    }
    chain.reverse();
    let root=chain.first().ok_or(SchedulerError::NotFound(id))?;
    let mut authority=root.definition.execution.authority(root.definition.initial.conversation_id,root.definition.initial.id)?;
    for child in chain.iter().skip(1){
        let policy=&child.definition.execution;
        authority=authority.child_in_workspace(child.definition.initial.conversation_id.to_string(),child.definition.initial.id.to_string(),policy.workspace_root.clone(),policy.mode)?.restrict_tools(policy.allowed_tools.clone().ok_or(SchedulerError::Invalid("child policy was not captured"))?)?;
    }
    Ok(authority)
}

pub(super) fn command_exists(store:&Store,id:crate::protocol::CommandId)->Result<bool,SchedulerError>{
    let connection=store.connection()?;
    Ok(connection.query_row("SELECT 1 FROM commands WHERE id=?1",[id.to_string()],|row|row.get::<_,i64>(0)).optional()?.is_some())
}
