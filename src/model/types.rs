use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const MAX_MESSAGES: usize = 4_096;
pub const MAX_CONTENT_BLOCKS: usize = 1_024;
pub const MAX_TOOLS: usize = 1_024;
pub const MAX_TEXT_BYTES: usize = 16 * 1_024 * 1_024;
pub const MAX_JSON_BYTES: usize = 1_024 * 1_024;
pub const MAX_CONTINUATION_BYTES: usize = 4 * 1_024 * 1_024;
pub const MAX_REQUEST_BYTES: usize = 32 * 1_024 * 1_024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub model: String,
    pub instructions: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub continuation: Option<ProviderContinuation>,
}

impl ModelRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Result<Self, ModelError> {
        let request = Self {
            model: model.into(),
            instructions: None,
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            continuation: None,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_identifier("model", &self.model, 256)?;
        if self.messages.is_empty() || self.messages.len() > MAX_MESSAGES {
            return Err(invalid(
                "messages",
                "must contain between 1 and 4096 messages",
            ));
        }
        if let Some(instructions) = &self.instructions {
            validate_text("instructions", instructions)?;
        }
        for message in &self.messages {
            message.validate()?;
        }
        if self.tools.len() > MAX_TOOLS {
            return Err(invalid("tools", "must contain at most 1024 tools"));
        }
        let mut names = HashSet::with_capacity(self.tools.len());
        for tool in &self.tools {
            tool.validate()?;
            if !names.insert(tool.name.as_str()) {
                return Err(invalid("tools", "tool names must be unique"));
            }
        }
        match &self.tool_choice {
            ToolChoice::Required if self.tools.is_empty() => {
                return Err(invalid("tool_choice", "required tools are not available"));
            }
            ToolChoice::Specific { name } if !names.contains(name.as_str()) => {
                return Err(invalid("tool_choice", "selected tool is not available"));
            }
            _ => {}
        }
        if self.max_output_tokens == Some(0) {
            return Err(invalid("max_output_tokens", "must be greater than zero"));
        }
        if let Some(temperature) = self.temperature {
            if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
                return Err(invalid("temperature", "must be finite and between 0 and 2"));
            }
        }
        if let Some(continuation) = &self.continuation {
            continuation.validate()?;
        }
        validate_json_size("request", self, MAX_REQUEST_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn text(role: MessageRole, text: impl Into<String>) -> Result<Self, ModelError> {
        let message = Self {
            role,
            content: vec![ContentBlock::Text(text.into())],
        };
        message.validate()?;
        Ok(message)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.content.is_empty() || self.content.len() > MAX_CONTENT_BLOCKS {
            return Err(invalid("content", "must contain between 1 and 1024 blocks"));
        }
        for block in &self.content {
            match block {
                ContentBlock::Text(text) => {
                    validate_text("text", text)?;
                    if self.role == MessageRole::Tool {
                        return Err(invalid("role", "tool messages require tool result blocks"));
                    }
                }
                ContentBlock::ToolCall(call) => {
                    call.validate()?;
                    if self.role != MessageRole::Assistant {
                        return Err(invalid("role", "tool calls require an assistant message"));
                    }
                }
                ContentBlock::ToolResult(result) => {
                    result.validate()?;
                    if self.role != MessageRole::Tool {
                        return Err(invalid("role", "tool results require a tool message"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Result<Self, ModelError> {
        let call = Self {
            id: id.into(),
            name: name.into(),
            arguments,
        };
        call.validate()?;
        Ok(call)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_identifier("tool_call.id", &self.id, 256)?;
        validate_identifier("tool_call.name", &self.name, 256)?;
        if !self.arguments.is_object() {
            return Err(invalid("tool_call.arguments", "must be a JSON object"));
        }
        validate_json_size("tool_call.arguments", &self.arguments, MAX_JSON_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
}

impl ToolResult {
    pub fn new(
        call_id: impl Into<String>,
        output: Value,
        is_error: bool,
    ) -> Result<Self, ModelError> {
        let result = Self {
            call_id: call_id.into(),
            output,
            is_error,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_identifier("tool_result.call_id", &self.call_id, 256)?;
        validate_json_size("tool_result.output", &self.output, MAX_JSON_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_identifier("tool.name", &self.name, 256)?;
        validate_text("tool.description", &self.description)?;
        if !self.input_schema.is_object() {
            return Err(invalid("tool.input_schema", "must be a JSON object"));
        }
        validate_json_size("tool.input_schema", &self.input_schema, MAX_JSON_BYTES)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Specific {
        name: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub continuation: bool,
    pub temperature: bool,
    pub max_context_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderContinuation {
    provider: String,
    state: Value,
}

impl ProviderContinuation {
    pub fn new(provider: impl Into<String>, state: Value) -> Result<Self, ModelError> {
        let continuation = Self {
            provider: provider.into(),
            state,
        };
        continuation.validate()?;
        Ok(continuation)
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn state(&self) -> &Value {
        &self.state
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_identifier("continuation.provider", &self.provider, 256)?;
        validate_json_size("continuation.state", &self.state, MAX_CONTINUATION_BYTES)
    }
}

impl fmt::Debug for ProviderContinuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderContinuation")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self
            .cached_input_tokens
            .is_some_and(|cached| cached > self.input_tokens)
        {
            return Err(invalid(
                "usage.cached_input_tokens",
                "must not exceed input tokens",
            ));
        }
        if self
            .reasoning_tokens
            .is_some_and(|reasoning| reasoning > self.output_tokens)
        {
            return Err(invalid(
                "usage.reasoning_tokens",
                "must not exceed output tokens",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolCall,
    MaxTokens,
    StopSequence,
    ContentFilter,
    Other(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelResponse {
    pub id: String,
    pub model: String,
    pub message: Message,
    pub usage: TokenUsage,
    pub stop_reason: StopReason,
    pub continuation: Option<ProviderContinuation>,
}

impl ModelResponse {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_identifier("response.id", &self.id, 256)?;
        validate_identifier("response.model", &self.model, 256)?;
        if self.message.role != MessageRole::Assistant {
            return Err(invalid("response.message.role", "must be assistant"));
        }
        self.message.validate()?;
        self.usage.validate()?;
        if let Some(continuation) = &self.continuation {
            continuation.validate()?;
        }
        validate_json_size("response", self, MAX_REQUEST_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    ResponseStarted {
        id: String,
        model: String,
    },
    TextDelta {
        index: u32,
        delta: String,
    },
    ToolCallStarted {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: u32,
        delta: String,
    },
    ContentBlockCompleted {
        index: u32,
        block: ContentBlock,
    },
    Usage {
        usage: TokenUsage,
    },
    Completed {
        response: ModelResponse,
    },
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("invalid {field}: {message}")]
    InvalidRequest { field: String, message: String },
    #[error("unsupported provider capability: {feature}")]
    Unsupported { feature: String },
    #[error("provider authentication failed")]
    Authentication,
    #[error("provider rate limit exceeded")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("provider transport failed: {message}")]
    Transport { message: String },
    #[error("provider failed: {message}")]
    Provider {
        code: Option<String>,
        message: String,
        retryable: bool,
    },
    #[error("invalid provider response: {message}")]
    Protocol { message: String },
    #[error("model request cancelled")]
    Cancelled,
}

fn invalid(field: &str, message: &str) -> ModelError {
    ModelError::InvalidRequest {
        field: field.to_owned(),
        message: message.to_owned(),
    }
}

fn validate_identifier(field: &str, value: &str, maximum: usize) -> Result<(), ModelError> {
    if value.is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(invalid(
            field,
            "must be nonempty, bounded, and contain no whitespace or controls",
        ));
    }
    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), ModelError> {
    if value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        return Err(invalid(
            field,
            "text exceeds its size limit or contains NUL",
        ));
    }
    Ok(())
}

fn validate_json_size<T: Serialize>(
    field: &str,
    value: &T,
    maximum: usize,
) -> Result<(), ModelError> {
    let mut writer = SizeLimiter {
        written: 0,
        maximum,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| {
        invalid(
            field,
            "serialized value exceeds its size limit or is not valid JSON",
        )
    })
}

struct SizeLimiter {
    written: usize,
    maximum: usize,
}

impl Write for SizeLimiter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self
            .written
            .checked_add(bytes.len())
            .filter(|written| *written <= self.maximum)
            .ok_or_else(|| io::Error::other("JSON size limit exceeded"))?;
        self.written = written;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
