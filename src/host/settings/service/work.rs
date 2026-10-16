use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::protocol::CommandId;

use super::super::authority::{CallerSource, SettingsAgentBinding, SettingsCaller};
use super::super::contract::SettingsToolName;
use super::{SettingsService, SettingsServiceError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsInvocationDisposition {
    ReadOnly,
    RejectedBeforeAdmission,
    Recorded { operation_id: CommandId },
    Unknown,
}

pub(super) struct InvocationWork {
    binding: SettingsAgentBinding,
    tool: SettingsToolName,
    operation_id: CommandId,
    state: Mutex<InvocationState>,
    changed: Notify,
}

struct InvocationState {
    closed: bool,
    started: bool,
    workers: usize,
    admission_attempted: bool,
    uncertain: bool,
    disposition: SettingsInvocationDisposition,
}

#[derive(Clone)]
pub(crate) struct SettingsInvocationCompletion {
    work: Arc<InvocationWork>,
}

pub(super) struct InvocationGuard {
    work: Arc<InvocationWork>,
}

impl InvocationWork {
    pub(super) fn new(
        binding: SettingsAgentBinding,
        tool: SettingsToolName,
        operation_id: CommandId,
    ) -> (Arc<Self>, SettingsInvocationCompletion) {
        let work = Arc::new(Self {
            binding,
            tool,
            operation_id,
            state: Mutex::new(InvocationState {
                closed: false,
                started: false,
                workers: 0,
                admission_attempted: false,
                uncertain: false,
                disposition: SettingsInvocationDisposition::Unknown,
            }),
            changed: Notify::new(),
        });
        let completion = SettingsInvocationCompletion {
            work: Arc::clone(&work),
        };
        (work, completion)
    }

    fn begin(
        self: &Arc<Self>,
        caller: &SettingsCaller,
        tool: SettingsToolName,
        operation_id: Option<CommandId>,
    ) -> Result<InvocationGuard, SettingsServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if state.closed || state.started || self.binding.cancellation.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        if self.tool != tool
            || operation_id.is_some_and(|id| id != self.operation_id)
            || !matches!(caller.source, CallerSource::SettingsAgent { session_id, native_session_id, tool: actual }
                if session_id == self.binding.session_id
                    && native_session_id == self.binding.native_session_id && actual == tool)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        state.started = true;
        state.workers = 1;
        if matches!(
            tool,
            SettingsToolName::List | SettingsToolName::Get | SettingsToolName::DevicesList
        ) {
            state.disposition = SettingsInvocationDisposition::ReadOnly;
        }
        Ok(InvocationGuard {
            work: Arc::clone(self),
        })
    }

    pub(super) fn fork(self: &Arc<Self>) -> Result<InvocationGuard, SettingsServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if state.workers == 0 || state.workers >= 16 {
            return Err(SettingsServiceError::Stopped);
        }
        state.workers += 1;
        Ok(InvocationGuard {
            work: Arc::clone(self),
        })
    }

    pub(super) fn reject(&self) -> Result<(), SettingsServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if !state.admission_attempted && !state.uncertain {
            state.disposition = SettingsInvocationDisposition::RejectedBeforeAdmission;
        }
        Ok(())
    }

    pub(super) fn attempting(&self) -> Result<(), SettingsServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        state.admission_attempted = true;
        state.disposition = SettingsInvocationDisposition::Unknown;
        Ok(())
    }

    pub(super) fn recorded(&self, operation_id: CommandId) -> Result<(), SettingsServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if operation_id != self.operation_id {
            state.uncertain = true;
            state.disposition = SettingsInvocationDisposition::Unknown;
            return Err(SettingsServiceError::Conflict);
        }
        state.admission_attempted = true;
        if !state.uncertain {
            state.disposition = SettingsInvocationDisposition::Recorded { operation_id };
        }
        Ok(())
    }
}

impl SettingsInvocationCompletion {
    pub(crate) async fn close_and_wait(
        &self,
    ) -> Result<SettingsInvocationDisposition, SettingsServiceError> {
        loop {
            let changed = self.work.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let (mut state, poisoned) = match self.work.state.lock() {
                    Ok(state) => (state, false),
                    Err(error) => (error.into_inner(), true),
                };
                state.closed = true;
                if poisoned {
                    state.uncertain = true;
                    state.disposition = SettingsInvocationDisposition::Unknown;
                }
                if state.workers == 0 {
                    return if poisoned {
                        Err(SettingsServiceError::Poisoned)
                    } else {
                        Ok(state.disposition)
                    };
                }
            }
            changed.await;
        }
    }
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        let (mut state, poisoned) = match self.work.state.lock() {
            Ok(state) => (state, false),
            Err(error) => (error.into_inner(), true),
        };
        if poisoned || std::thread::panicking() || state.workers == 0 {
            state.uncertain = true;
            state.disposition = SettingsInvocationDisposition::Unknown;
        }
        state.workers = state.workers.saturating_sub(1);
        drop(state);
        self.work.changed.notify_waiters();
    }
}

impl SettingsService {
    pub(super) fn invocation_guard(
        &self,
        caller: &SettingsCaller,
        tool: SettingsToolName,
        operation_id: Option<CommandId>,
    ) -> Result<Option<InvocationGuard>, SettingsServiceError> {
        self.invocation
            .as_ref()
            .map(|work| work.begin(caller, tool, operation_id))
            .transpose()
    }

    pub(super) fn reject_invocation(&self) -> Result<(), SettingsServiceError> {
        if let Some(work) = &self.invocation {
            work.reject()?;
        }
        Ok(())
    }
}
