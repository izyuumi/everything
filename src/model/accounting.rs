use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_core::Stream;
use tokio::sync::Notify;

use crate::protocol::CommandId;

use super::{ModelError, ProviderStream, TokenUsage};

const MAX_OBSERVED_ATTEMPTS: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct ProviderObservation {
    pub attempt_id: CommandId,
    pub usage: TokenUsage,
    pub terminal_observed: bool,
    pub response_valid: bool,
    pub invalid: bool,
}

#[derive(Debug, Default)]
struct Observations {
    entries: [Option<ProviderObservation>; MAX_OBSERVED_ATTEMPTS],
    current: Option<usize>,
    work: [AttemptWork; MAX_OBSERVED_ATTEMPTS],
    lifetime_failed: bool,
}

#[derive(Debug, Default)]
struct AttemptWork {
    closed: bool,
    stream_started: bool,
    preparation_started: bool,
    local_preparation_failed: bool,
    roots: usize,
    preparations: usize,
}

#[derive(Clone, Copy)]
enum WorkKind {
    Stream,
    Preparation,
}

pub(super) struct ProviderWorkGuard {
    accounting: Arc<ProviderAccounting>,
    index: usize,
    attempt_id: CommandId,
    kind: WorkKind,
}

pub(super) struct ProviderWork<T> {
    pub(super) value: T,
    pub(super) root: Option<ProviderWorkGuard>,
    pub(super) preparation: Option<ProviderWorkGuard>,
}

impl<T> ProviderWork<T> {
    pub(super) fn new(
        value: T,
        accounting: Option<&Arc<ProviderAccounting>>,
    ) -> Result<Self, ModelError> {
        let root = accounting
            .map(ProviderAccounting::begin_stream)
            .transpose()?;
        Ok(Self {
            value,
            root,
            preparation: None,
        })
    }

    pub(super) fn fork_preparation(&mut self) -> Result<(), ModelError> {
        if self.preparation.is_some() {
            if let Some(root) = &self.root {
                root.fail_witness();
            }
            return Err(accounting_error());
        }
        self.preparation = self
            .root
            .as_ref()
            .map(ProviderWorkGuard::fork_preparation)
            .transpose()?;
        Ok(())
    }

    pub(super) fn prepare<U>(
        self,
        operation: impl FnOnce(T) -> Result<U, ModelError>,
    ) -> ProviderWork<Result<U, ModelError>> {
        let Self {
            value,
            root,
            preparation,
        } = self;
        let value = operation(value);
        if value.is_err()
            && let Some(guard) = &preparation
        {
            guard.local_preparation_failed();
        }
        ProviderWork {
            value,
            root,
            preparation,
        }
    }
}

impl ProviderWork<ProviderStream> {
    pub(super) fn into_stream(self) -> ProviderStream {
        if self.root.is_none() {
            self.value
        } else {
            Box::pin(self)
        }
    }
}

impl Stream for ProviderWork<ProviderStream> {
    type Item = <ProviderStream as Stream>::Item;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().value.as_mut().poll_next(context)
    }
}

impl ProviderWorkGuard {
    fn fail_witness(&self) {
        if let Ok(mut state) = self.accounting.state.lock() {
            state.lifetime_failed = true;
        }
        self.accounting.changed.notify_waiters();
    }

    fn local_preparation_failed(&self) {
        if let Ok(mut state) = self.accounting.state.lock() {
            if state.entries[self.index]
                .as_ref()
                .is_none_or(|entry| entry.attempt_id != self.attempt_id)
            {
                state.lifetime_failed = true;
            } else {
                state.work[self.index].local_preparation_failed = true;
            }
        }
        self.accounting.changed.notify_waiters();
    }

    fn fork_preparation(&self) -> Result<Self, ModelError> {
        let mut state = self
            .accounting
            .state
            .lock()
            .map_err(|_| accounting_error())?;
        if state.lifetime_failed
            || state.current != Some(self.index)
            || !matches!(self.kind, WorkKind::Stream)
            || state.entries[self.index]
                .as_ref()
                .is_none_or(|entry| entry.attempt_id != self.attempt_id)
        {
            state.lifetime_failed = true;
            return Err(accounting_error());
        }
        let work = &mut state.work[self.index];
        if work.closed || work.preparation_started || work.roots != 1 || work.preparations != 0 {
            state.lifetime_failed = true;
            return Err(accounting_error());
        }
        work.preparations = 1;
        work.preparation_started = true;
        Ok(Self {
            accounting: Arc::clone(&self.accounting),
            index: self.index,
            attempt_id: self.attempt_id,
            kind: WorkKind::Preparation,
        })
    }
}

impl Drop for ProviderWorkGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.accounting.state.lock() {
            if std::thread::panicking()
                || state.entries[self.index]
                    .as_ref()
                    .is_none_or(|entry| entry.attempt_id != self.attempt_id)
            {
                state.lifetime_failed = true;
            }
            let work = &mut state.work[self.index];
            let count = match self.kind {
                WorkKind::Stream => &mut work.roots,
                WorkKind::Preparation => &mut work.preparations,
            };
            if let Some(remaining) = count.checked_sub(1) {
                *count = remaining;
            } else {
                state.lifetime_failed = true;
            }
        }
        self.accounting.changed.notify_waiters();
    }
}

#[derive(Debug, Default)]
pub(crate) struct ProviderAccounting {
    state: Mutex<Observations>,
    changed: Notify,
}

impl ProviderAccounting {
    pub(crate) fn arm(&self, attempt_id: CommandId) -> Result<(), ModelError> {
        let mut state = self.state.lock().map_err(|_| accounting_error())?;
        let result = (|| {
            if state.lifetime_failed {
                return Err(accounting_error());
            }
            if let Some(index) = state.current {
                let entry = state.entries[index].as_ref().ok_or_else(accounting_error)?;
                let work = &state.work[index];
                if !entry.terminal_observed
                    || !entry.response_valid
                    || entry.invalid
                    || work.roots != 0
                    || work.preparations != 0
                {
                    return Err(accounting_error());
                }
                state.work[index].closed = true;
            }
            if state
                .entries
                .iter()
                .flatten()
                .any(|entry| entry.attempt_id == attempt_id)
            {
                return Err(accounting_error());
            }
            let index = state
                .entries
                .iter()
                .position(Option::is_none)
                .ok_or_else(accounting_error)?;
            state.entries[index] = Some(ProviderObservation {
                attempt_id,
                usage: TokenUsage::default(),
                terminal_observed: false,
                response_valid: false,
                invalid: false,
            });
            state.current = Some(index);
            Ok(())
        })();
        if result.is_err() {
            state.lifetime_failed = true;
            self.changed.notify_waiters();
        }
        result
    }

    fn begin_stream(self: &Arc<Self>) -> Result<ProviderWorkGuard, ModelError> {
        let mut state = self.state.lock().map_err(|_| accounting_error())?;
        if state.lifetime_failed {
            return Err(accounting_error());
        }
        let Some(index) = state.current else {
            state.lifetime_failed = true;
            return Err(accounting_error());
        };
        let Some(entry) = state.entries[index].as_ref() else {
            state.lifetime_failed = true;
            return Err(accounting_error());
        };
        let attempt_id = entry.attempt_id;
        let work = &mut state.work[index];
        if work.closed || work.stream_started || work.roots != 0 || work.preparations != 0 {
            state.lifetime_failed = true;
            return Err(accounting_error());
        }
        work.roots = 1;
        work.stream_started = true;
        Ok(ProviderWorkGuard {
            accounting: Arc::clone(self),
            index,
            attempt_id,
            kind: WorkKind::Stream,
        })
    }

    pub(crate) async fn close_and_wait(&self) -> Result<(), ModelError> {
        let index = {
            let mut state = self.state.lock().map_err(|_| accounting_error())?;
            let Some(index) = state.current else {
                return if state.lifetime_failed {
                    Err(accounting_error())
                } else {
                    Ok(())
                };
            };
            state.work[index].closed = true;
            index
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().map_err(|_| accounting_error())?;
                let work = &state.work[index];
                if work.roots == 0 && work.preparations == 0 {
                    return if state.lifetime_failed {
                        Err(accounting_error())
                    } else {
                        Ok(())
                    };
                }
            }
            changed.await;
        }
    }

    pub(crate) fn local_preparation_failed(&self) -> Result<bool, ModelError> {
        let state = self.state.lock().map_err(|_| accounting_error())?;
        if state.lifetime_failed {
            return Err(accounting_error());
        }
        Ok(state
            .current
            .is_some_and(|index| state.work[index].local_preparation_failed))
    }

    pub(crate) fn observe(
        &self,
        usage: &TokenUsage,
        terminal_observed: bool,
        response_valid: bool,
    ) -> Result<(), ModelError> {
        let mut state = self.state.lock().map_err(|_| accounting_error())?;
        let index = state.current.ok_or_else(accounting_error)?;
        let entry = state.entries[index].as_mut().ok_or_else(accounting_error)?;
        let invalid = usage.validate().is_err()
            || usage.input_tokens < entry.usage.input_tokens
            || usage.output_tokens < entry.usage.output_tokens
            || entry.usage.cached_input_tokens.is_some_and(|previous| {
                usage
                    .cached_input_tokens
                    .is_none_or(|current| current < previous)
            })
            || entry.usage.reasoning_tokens.is_some_and(|previous| {
                usage
                    .reasoning_tokens
                    .is_none_or(|current| current < previous)
            })
            || (entry.response_valid && !response_valid);
        entry.usage.input_tokens = entry.usage.input_tokens.max(usage.input_tokens);
        entry.usage.output_tokens = entry.usage.output_tokens.max(usage.output_tokens);
        if let Some(cached) = usage.cached_input_tokens
            && cached <= entry.usage.input_tokens
        {
            entry.usage.cached_input_tokens =
                Some(entry.usage.cached_input_tokens.unwrap_or(0).max(cached));
        }
        if let Some(reasoning) = usage.reasoning_tokens
            && reasoning <= entry.usage.output_tokens
        {
            entry.usage.reasoning_tokens =
                Some(entry.usage.reasoning_tokens.unwrap_or(0).max(reasoning));
        }
        entry.terminal_observed |= terminal_observed;
        if invalid || entry.invalid {
            entry.invalid = true;
            return Err(accounting_error());
        }
        entry.response_valid |= response_valid;
        Ok(())
    }

    pub(crate) fn invalidate(&self, terminal_observed: bool) -> Result<(), ModelError> {
        let mut state = self.state.lock().map_err(|_| accounting_error())?;
        let index = state.current.ok_or_else(accounting_error)?;
        let entry = state.entries[index].as_mut().ok_or_else(accounting_error)?;
        entry.terminal_observed |= terminal_observed;
        entry.invalid = true;
        Ok(())
    }

    pub(crate) fn snapshot(
        &self,
        attempt_id: CommandId,
    ) -> Result<ProviderObservation, ModelError> {
        self.state
            .lock()
            .map_err(|_| accounting_error())?
            .entries
            .iter()
            .flatten()
            .find(|entry| entry.attempt_id == attempt_id)
            .cloned()
            .ok_or_else(accounting_error)
    }
}

fn accounting_error() -> ModelError {
    ModelError::Protocol {
        message: "restricted provider accounting could not be retained".into(),
    }
}
