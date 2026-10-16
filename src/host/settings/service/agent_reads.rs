use uuid::Uuid;

use crate::protocol::CommandId;

use super::super::actions::{NativeActionDetails, NativeActionReference, SettingsActionTarget};
use super::super::agent::SettingsOperationLookup;
use super::super::authority::{CallerSource, SettingsCaller, SettingsOperationOrigin};
use super::super::contract::{NativeSettingsAction, SettingsScope};
use super::super::operations::{OperationIdentity, SettingsOperationPhase};
use super::{ServiceCore, SettingsService, SettingsServiceError};

impl SettingsService {
    pub(crate) async fn inspect_agent_operation(
        &self,
        caller: SettingsCaller,
        chat_id: Uuid,
        turn_id: CommandId,
        operation_id: CommandId,
    ) -> Result<SettingsOperationLookup, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        self.blocking(move |core| {
            core.check_agent_operation_link(&caller, chat_id, turn_id, operation_id, true)?;
            let lookup = match core.repository.get(operation_id) {
                Ok(record) => {
                    core.check_agent_operation_identity(
                        &caller,
                        chat_id,
                        operation_id,
                        &record.identity,
                    )?;
                    let operation = record.inspection();
                    operation.validate()?;
                    SettingsOperationLookup::Inspected {
                        operation: Box::new(operation),
                    }
                }
                Err(SettingsServiceError::NotFound) => SettingsOperationLookup::NotRecorded,
                Err(error) => return Err(error),
            };
            core.authority.validate(&caller, true)?;
            Ok(lookup)
        })
        .await
    }

    pub(crate) async fn agent_action_details(
        &self,
        caller: SettingsCaller,
        chat_id: Uuid,
        turn_id: CommandId,
        operation_id: CommandId,
        reference: NativeActionReference,
    ) -> Result<NativeActionDetails, SettingsServiceError> {
        self.core.authority.validate(&caller, false)?;
        self.blocking(move |core| {
            core.check_agent_operation_link(&caller, chat_id, turn_id, operation_id, false)?;
            let record = core.repository.get(operation_id)?;
            core.check_agent_operation_identity(&caller, chat_id, operation_id, &record.identity)?;
            match &record.identity.target {
                SettingsActionTarget::Patch(_) | SettingsActionTarget::Device(_) => {}
                SettingsActionTarget::Native(request)
                    if matches!(request.action, NativeSettingsAction::OpenSection { .. }) => {}
                _ => return Err(SettingsServiceError::OutsideScope),
            }
            let expected = match record.phase() {
                SettingsOperationPhase::AwaitingAction { reference } => {
                    NativeActionReference::Action(reference.clone())
                }
                SettingsOperationPhase::AwaitingConfirmation { reference } => {
                    NativeActionReference::Confirmation(reference.clone())
                }
                _ => return Err(SettingsServiceError::Conflict),
            };
            if expected != reference {
                return Err(SettingsServiceError::Conflict);
            }
            core.check_current(&record.identity)?;
            core.authority.validate(&caller, false)?;
            Ok(NativeActionDetails {
                operation: record.inspection(),
                target: record.identity.target,
            })
        })
        .await
    }
}

impl ServiceCore {
    fn check_agent_operation_link(
        &self,
        caller: &SettingsCaller,
        chat_id: Uuid,
        turn_id: CommandId,
        operation_id: CommandId,
        cleanup: bool,
    ) -> Result<(), SettingsServiceError> {
        self.authority.validate(caller, cleanup)?;
        let CallerSource::NativeUi(parent) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let store = self.agent.store();
        let reservation = store
            .settings_session(chat_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        let fence = store
            .settings_turn(turn_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if reservation.session_id != chat_id
            || reservation.host_id != self.authority.host_id
            || reservation.principal_id != self.authority.principal()
            || reservation.native_session_id != parent
            || fence.command_id != turn_id
            || fence.session_id != chat_id
            || !fence.mutation_ids.contains(&operation_id)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.check_scope(&SettingsScope::Host {
            host_id: reservation.host_id,
        })
    }

    fn check_agent_operation_identity(
        &self,
        caller: &SettingsCaller,
        chat_id: Uuid,
        operation_id: CommandId,
        identity: &OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        let CallerSource::NativeUi(parent) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if identity.binding.request_id != operation_id
            || identity.origin
                != (SettingsOperationOrigin::SettingsAgent {
                    session_id: chat_id,
                    native_session_id: parent,
                })
            || identity.binding.scope
                != (SettingsScope::Host {
                    host_id: self.authority.host_id,
                })
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.check_principal(caller, identity)
    }
}
