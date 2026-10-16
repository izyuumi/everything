use serde::{Deserialize, Serialize};

use crate::protocol::{CommandId, UnixMillis};

use super::super::contract::{
    SETTINGS_CONTRACT_VERSION, SettingKey, SettingsOperationState, SettingsScope, SettingsToolName,
};
use super::super::operations::{
    OperationRecord, SettingsApplicationState, SettingsOperationPhase, SettingsWriteResolution,
};
use super::{ServiceCore, SettingsCaller, SettingsService, SettingsServiceError};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChangePage {
    pub contract_version: u16,
    pub observed_at: UnixMillis,
    pub changes: Vec<SettingsChangeHint>,
    pub next_sequence: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChangeHint {
    pub sequence: u64,
    pub scope: SettingsScope,
    pub request_id: Option<CommandId>,
    pub revision: Option<u64>,
    pub changed_keys: Vec<SettingKey>,
    pub state: SettingsChangeState,
    pub application: Option<SettingsApplicationState>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsChangeState {
    KeysChanged,
    AwaitingConfirmation,
    AwaitingAction,
    InFlight,
    Applying,
    Applied,
    Completed,
    PendingApplication,
    HostActionRequired,
    RestartRequired,
    Conflict,
    Unavailable,
    Failed,
    Unknown,
    Cancelled,
}

impl SettingsService {
    pub fn settings_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.core.repository.changes()
    }

    pub async fn changes(
        &self,
        caller: SettingsCaller,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<SettingsChangePage, SettingsServiceError> {
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::Get)?;
        self.blocking(move |core| {
            core.authority
                .require_tool(&caller, SettingsToolName::Get)?;
            let records = core
                .repository
                .change_records(after_sequence.unwrap_or(0), limit)?;
            let mut page = SettingsChangePage {
                contract_version: SETTINGS_CONTRACT_VERSION,
                observed_at: UnixMillis::now()?,
                changes: Vec::new(),
                next_sequence: None,
            };
            for (sequence, record) in records {
                let Some(hint) = core.change_hint(&caller, sequence, &record)? else {
                    page.next_sequence = Some(sequence);
                    continue;
                };
                if encoded_size(&hint)? > 1024 {
                    return Err(SettingsServiceError::Capacity);
                }
                let previous_cursor = page.next_sequence;
                page.changes.push(hint);
                page.next_sequence = Some(sequence);
                if encoded_size(&page)? > 32 * 1024 {
                    page.changes.pop();
                    page.next_sequence = previous_cursor;
                    break;
                }
            }
            core.authority
                .require_tool(&caller, SettingsToolName::Get)?;
            Ok(page)
        })
        .await
    }
}

impl ServiceCore {
    fn change_hint(
        &self,
        caller: &SettingsCaller,
        sequence: u64,
        record: &OperationRecord,
    ) -> Result<Option<SettingsChangeHint>, SettingsServiceError> {
        let own_operation = match self.check_principal(caller, &record.identity) {
            Ok(()) => true,
            Err(SettingsServiceError::OutsideScope) => false,
            Err(error) => return Err(error),
        };
        let write = record.write();
        let keys = write.map_or_else(Vec::new, |write| write.intent.changed_keys.clone());
        let applied =
            write.is_some_and(|write| write.resolution == Some(SettingsWriteResolution::Applied));
        if !own_operation {
            if !applied
                || keys.is_empty()
                || !matches!(record.identity.binding.scope, SettingsScope::Host{host_id} if host_id==self.authority.host_id)
            {
                return Ok(None);
            }
            return Ok(Some(SettingsChangeHint {
                sequence,
                scope: record.identity.binding.scope.clone(),
                request_id: None,
                revision: write.map(|write| write.intent.after.revision),
                changed_keys: keys,
                state: SettingsChangeState::KeysChanged,
                application: record.application().cloned(),
            }));
        }
        let (state, revision) = match record.phase() {
            SettingsOperationPhase::AwaitingConfirmation { .. } => {
                (SettingsChangeState::AwaitingConfirmation, None)
            }
            SettingsOperationPhase::AwaitingAction { .. } => {
                (SettingsChangeState::AwaitingAction, None)
            }
            SettingsOperationPhase::InFlight { .. } => (SettingsChangeState::InFlight, None),
            SettingsOperationPhase::Applying { .. } => (SettingsChangeState::Applying, None),
            SettingsOperationPhase::Finished { result } => match &result.state {
                SettingsOperationState::Applied { revision } => {
                    (SettingsChangeState::Applied, Some(*revision))
                }
                SettingsOperationState::Completed { .. } => (SettingsChangeState::Completed, None),
                SettingsOperationState::PendingApplication { revision } => {
                    (SettingsChangeState::PendingApplication, Some(*revision))
                }
                SettingsOperationState::HostActionRequired => {
                    (SettingsChangeState::HostActionRequired, None)
                }
                SettingsOperationState::RestartRequired { revision } => {
                    (SettingsChangeState::RestartRequired, Some(*revision))
                }
                SettingsOperationState::Conflict { observed_revision } => {
                    (SettingsChangeState::Conflict, Some(*observed_revision))
                }
                SettingsOperationState::Unavailable { .. } => {
                    (SettingsChangeState::Unavailable, None)
                }
                SettingsOperationState::Failed { .. } => (SettingsChangeState::Failed, None),
                SettingsOperationState::Unknown => (SettingsChangeState::Unknown, None),
                SettingsOperationState::Cancelled => (SettingsChangeState::Cancelled, None),
                _ => return Err(SettingsServiceError::CorruptJournal),
            },
        };
        Ok(Some(SettingsChangeHint {
            sequence,
            scope: record.identity.binding.scope.clone(),
            request_id: Some(record.identity.binding.request_id),
            revision,
            changed_keys: keys,
            state,
            application: record.application().cloned(),
        }))
    }
}

fn encoded_size(value: &impl Serialize) -> Result<usize, SettingsServiceError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| SettingsServiceError::Encoding)
}
