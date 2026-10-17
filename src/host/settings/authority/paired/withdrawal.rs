use std::any::Any;
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::agent::Agent;
use crate::devices::{DeviceEndpoint, OwnedCommittedSettingsBind, SettingsReceiveError};

use super::{Authority, BindingHistoryState, PairedSettingsParent, ParentEntry};

pub(crate) struct OwnedWithdrawnSettingsParent {
    payload: Option<WithdrawnParentPayload>,
}

struct WithdrawnParentPayload {
    removed: Option<ParentEntry>,
    parent: PairedSettingsParent,
    request: Box<OwnedCommittedSettingsBind>,
}

pub(crate) struct PairedParentWithdrawalFailure {
    payload: Option<FailedParentWithdrawal>,
}

struct FailedParentWithdrawal {
    cause: PairedParentWithdrawalCause,
    owner: OwnedWithdrawnSettingsParent,
}

#[derive(thiserror::Error)]
enum PairedParentWithdrawalCause {
    #[error("Settings parent withdrawal requires the root service")]
    Invocation,
    #[error("Settings parent withdrawal requires the original Host mutex")]
    MutationScope,
    #[error("Settings parent withdrawal source does not match")]
    SourceScope,
    #[error("Settings parent withdrawal requires the retired original connection")]
    ConnectionNotRetired,
    #[error("Settings parent withdrawal could not inspect original history")]
    History(#[source] SettingsReceiveError),
    #[error("Settings parent withdrawal history does not match")]
    HistoryState,
    #[error("Settings parent withdrawal registration does not match")]
    RegistrationScope,
    #[error("Settings parent withdrawal registration lock is poisoned")]
    Poisoned,
    #[error("Settings parent withdrawal panicked")]
    Panicked(Box<dyn Any + Send>),
}

struct AbandonedParentWithdrawal {
    _cause: ParentWithdrawalAbandoned,
    payload: WithdrawnParentPayload,
}

struct ParentWithdrawalAbandoned;

impl OwnedWithdrawnSettingsParent {
    pub(in crate::host::settings) fn new(
        parent: PairedSettingsParent,
        request: Box<OwnedCommittedSettingsBind>,
    ) -> Self {
        Self {
            payload: Some(WithdrawnParentPayload {
                removed: None,
                parent,
                request,
            }),
        }
    }

    pub(in crate::host::settings) fn reject_invocation(self) -> Box<PairedParentWithdrawalFailure> {
        self.fail(PairedParentWithdrawalCause::Invocation)
    }

    pub(in crate::host::settings) fn withdraw_on(
        mut self,
        authority: &Arc<Authority>,
        agent: &Agent,
        endpoint: &DeviceEndpoint,
        mutation: &Arc<Mutex<()>>,
        guard: &OwnedMutexGuard<()>,
    ) -> Result<Self, Box<PairedParentWithdrawalFailure>> {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            self.withdraw_registration_on(authority, agent, endpoint, mutation, guard)
        }));
        match outcome {
            Ok(Ok(())) => Ok(self),
            Ok(Err(cause)) => Err(self.fail(cause)),
            Err(payload) => Err(self.fail(PairedParentWithdrawalCause::Panicked(payload))),
        }
    }

    fn withdraw_registration_on(
        &mut self,
        authority: &Arc<Authority>,
        agent: &Agent,
        endpoint: &DeviceEndpoint,
        mutation: &Arc<Mutex<()>>,
        guard: &OwnedMutexGuard<()>,
    ) -> Result<(), PairedParentWithdrawalCause> {
        use PairedParentWithdrawalCause as Cause;

        let payload = self.payload.as_mut().ok_or(Cause::RegistrationScope)?;
        if payload.removed.is_some() {
            return Err(Cause::RegistrationScope);
        }
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(guard), mutation) {
            return Err(Cause::MutationScope);
        }
        let request = &payload.request;
        let parent = &payload.parent;
        let committed = request.committed_source();
        let source = committed.authority_source();
        let connection = source.connection();
        let host = connection.host_identity();
        let intent = request.binding_intent();
        let lifetime = committed.lifetime();
        if !Arc::ptr_eq(authority, &parent.authority)
            || !source.matches_context(agent, endpoint, mutation)
            || !Arc::ptr_eq(&parent.record.connection, connection)
            || !Arc::ptr_eq(&parent.record.creation, committed.creation_identity())
            || parent.record.intent != *intent
            || parent.record.peer != *source.verified_peer_keys()
            || parent.record.lifetime.expires_at != lifetime.expires_at
            || parent.record.lifetime.deadline != lifetime.deadline
            || intent.host_id != authority.host_id
            || host.host_id() != authority.host_id
            || intent.generation.host_epoch != host.host_epoch()
            || intent.generation.connection_epoch != connection.connection_epoch()
        {
            return Err(Cause::SourceScope);
        }
        let snapshot = request.binding_snapshot().map_err(Cause::History)?;
        if snapshot.intent != *intent
            || snapshot.state != BindingHistoryState::Committed
            || snapshot.lifetime.is_none_or(|original| {
                original.expires_at != lifetime.expires_at || original.deadline != lifetime.deadline
            })
        {
            return Err(Cause::HistoryState);
        }
        let mut sessions = authority.sessions.lock().map_err(|_| Cause::Poisoned)?;
        if !connection.is_retired() {
            return Err(Cause::ConnectionNotRetired);
        }
        if sessions
            .paired
            .host
            .as_ref()
            .is_none_or(|original| !Arc::ptr_eq(original, host))
        {
            return Err(Cause::SourceScope);
        }
        let key = (
            connection.connection_epoch(),
            source.verified_peer_keys().device_id,
            intent.binding_request_id,
        );
        if sessions
            .paired
            .entries
            .get(&key)
            .is_none_or(|entry| !Arc::ptr_eq(&entry.record, &parent.record))
        {
            return Err(Cause::RegistrationScope);
        }
        let removed = sessions
            .paired
            .entries
            .remove(&key)
            .ok_or(Cause::RegistrationScope)?;
        payload.removed = Some(removed);
        drop(sessions);
        payload.parent.record.cancellation.cancel();
        Ok(())
    }

    fn fail(self, cause: PairedParentWithdrawalCause) -> Box<PairedParentWithdrawalFailure> {
        Box::new(PairedParentWithdrawalFailure {
            payload: Some(FailedParentWithdrawal { cause, owner: self }),
        })
    }

    fn quarantine(&self) {
        if let Some(payload) = &self.payload {
            payload.request.quarantine_retirement();
        }
    }
}

impl Drop for OwnedWithdrawnSettingsParent {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let retained = Box::leak(Box::new(AbandonedParentWithdrawal {
                _cause: ParentWithdrawalAbandoned,
                payload,
            }));
            retained.payload.request.quarantine_retirement();
        }
    }
}

impl Drop for PairedParentWithdrawalFailure {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let retained = Box::leak(Box::new(payload));
            retained.owner.quarantine();
        }
    }
}

impl fmt::Debug for OwnedWithdrawnSettingsParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedWithdrawnSettingsParent([retained registration])")
    }
}

impl fmt::Debug for PairedParentWithdrawalFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedParentWithdrawalFailure([retained owner and cause])")
    }
}

impl fmt::Display for PairedParentWithdrawalFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Settings parent withdrawal failed")
    }
}

impl Error for PairedParentWithdrawalFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.payload
            .as_ref()
            .map(|payload| &payload.cause as &(dyn Error + 'static))
    }
}

impl fmt::Debug for PairedParentWithdrawalCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedParentWithdrawalCause([retained cause])")
    }
}
