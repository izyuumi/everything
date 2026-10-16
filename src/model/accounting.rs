use std::sync::Mutex;

use crate::protocol::CommandId;

use super::{ModelError, TokenUsage};

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
}

#[derive(Debug, Default)]
pub(crate) struct ProviderAccounting {
    state: Mutex<Observations>,
}

impl ProviderAccounting {
    pub(crate) fn arm(&self, attempt_id: CommandId) -> Result<(), ModelError> {
        let mut state = self.state.lock().map_err(|_| accounting_error())?;
        if state
            .current
            .and_then(|index| state.entries[index].as_ref())
            .is_some_and(|entry| !entry.terminal_observed || !entry.response_valid || entry.invalid)
        {
            return Err(accounting_error());
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
