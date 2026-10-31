use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::accounting::{ProviderAccounting, ProviderWork};
use super::{
    ContentBlock, MAX_CONTENT_BLOCKS, MAX_CONTINUATION_BYTES, MAX_JSON_BYTES, MAX_REQUEST_BYTES,
    MAX_TEXT_BYTES, Message, MessageRole, ModelError, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ProviderCapabilities, ProviderContinuation, ProviderStream, StopReason,
    TokenUsage, ToolCall, ToolChoice,
};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const MAX_STREAM_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_EVENT_BYTES: usize = 2 * 1_024 * 1_024;
const MAX_ERROR_BYTES: usize = 64 * 1_024;
const MAX_STREAM_EVENTS: usize = 262_144;

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    api_key: HeaderValue,
    endpoint: Url,
    connect_timeout: Duration,
    request_timeout: Duration,
    idle_timeout: Duration,
    default_max_output_tokens: u32,
    thinking_budget_tokens: Option<u32>,
    accounting: Option<Arc<ProviderAccounting>>,
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Result<Self, ModelError> {
        let secret = Zeroizing::new(api_key.into());
        if secret.is_empty()
            || secret.len() > 8_192
            || !secret.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(invalid(
                "api_key",
                "must be a bounded nonempty ASCII credential",
            ));
        }
        let mut api_key = HeaderValue::from_str(&secret)
            .map_err(|_| invalid("api_key", "cannot be represented as an HTTP header"))?;
        api_key.set_sensitive(true);
        Ok(Self {
            api_key,
            endpoint: Url::parse(ENDPOINT)
                .map_err(|_| invalid("endpoint", "invalid official Messages endpoint"))?,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(90),
            default_max_output_tokens: 4_096,
            thinking_budget_tokens: None,
            accounting: None,
        })
    }

    pub(crate) fn with_accounting(mut self, accounting: Arc<ProviderAccounting>) -> Self {
        self.accounting = Some(accounting);
        self
    }

    pub fn with_loopback_endpoint(mut self, endpoint: &str) -> Result<Self, ModelError> {
        let endpoint = Url::parse(endpoint)
            .map_err(|_| invalid("endpoint", "invalid loopback Messages endpoint"))?;
        let loopback = endpoint
            .host_str()
            .and_then(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .ok()
            })
            .is_some_and(|address| address.is_loopback());
        if !loopback
            || !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(invalid(
                "endpoint",
                "custom endpoints require an explicit loopback IP, HTTP(S), and no URL credentials, query, or fragment",
            ));
        }
        self.endpoint = endpoint;
        Ok(self)
    }

    pub fn with_timeouts(
        mut self,
        connect: Duration,
        request: Duration,
        idle: Duration,
    ) -> Result<Self, ModelError> {
        if [connect, request, idle]
            .iter()
            .any(|duration| duration.is_zero() || *duration > Duration::from_secs(3_600))
        {
            return Err(invalid("timeouts", "must be positive and at most one hour"));
        }
        self.connect_timeout = connect;
        self.request_timeout = request;
        self.idle_timeout = idle;
        Ok(self)
    }

    pub fn with_default_max_output_tokens(mut self, tokens: u32) -> Result<Self, ModelError> {
        if tokens == 0 {
            return Err(invalid("max_output_tokens", "must be positive"));
        }
        self.default_max_output_tokens = tokens;
        Ok(self)
    }

    pub fn with_thinking_budget(mut self, tokens: u32) -> Result<Self, ModelError> {
        if tokens < 1_024 {
            return Err(invalid(
                "thinking_budget",
                "must contain at least 1024 tokens",
            ));
        }
        self.thinking_budget_tokens = Some(tokens);
        Ok(self)
    }
}

pub struct AnthropicProvider {
    client: Client,
    config: AnthropicConfig,
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Result<Self, ModelError> {
        Self::with_config(AnthropicConfig::new(api_key)?)
    }

    pub fn with_config(config: AnthropicConfig) -> Result<Self, ModelError> {
        let builder = Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("vorra/{}", crate::VERSION));
        let builder = if config.endpoint.as_str() != ENDPOINT {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|_| transport("could not construct native Anthropic HTTP client"))?;
        Ok(Self { client, config })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: true,
            continuation: true,
            temperature: self.config.thinking_budget_tokens.is_none(),
            max_context_tokens: None,
            max_output_tokens: None,
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ProviderStream, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let config = PreparationConfig {
            default_max_output_tokens: self.config.default_max_output_tokens,
            thinking_budget_tokens: self.config.thinking_budget_tokens,
        };
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| transport("Anthropic requests require a Tokio runtime"))?;
        let mut request = ProviderWork::new(request, self.config.accounting.as_ref())?;
        request.fork_preparation()?;
        let preparation = runtime.spawn_blocking(move || {
            request.prepare(|request| {
                request.validate()?;
                let (body, names, continuation, index) = prepare_request(request, config)?;
                let mut encoded = BoundedRequestBody(Vec::new());
                serde_json::to_writer(&mut encoded, &body).map_err(|_| {
                    invalid(
                        "request",
                        "native Anthropic request exceeds 32 MiB or cannot be encoded",
                    )
                })?;
                Ok::<_, ModelError>((encoded.0, names, continuation, index))
            })
        });
        let ProviderWork {
            value,
            root,
            preparation,
        } = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
            prepared = preparation => prepared.map_err(|_| transport("Anthropic request preparation failed"))?,
        };
        let (body, tool_names, continuation, assistant_index) = value?;
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let send = self
            .client
            .post(self.config.endpoint.clone())
            .header("x-api-key", self.config.api_key.clone())
            .header("anthropic-version", API_VERSION)
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send();
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
            response = send => response.map_err(|_| transport("Anthropic connection or request failed"))?,
        };
        if !response.status().is_success() {
            return Err(http_error(response, &cancellation, self.config.idle_timeout).await);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next());
        if !content_type.is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(protocol(
                "Anthropic success response is not an event stream",
            ));
        }
        let idle_timeout = self.config.idle_timeout;
        let accounting = self.config.accounting.clone();
        let body_cancellation = cancellation.clone();
        let bytes = async_stream::try_stream! {
            let mut response = response;
            let mut budget = WireBudget::default();
            loop {
                let chunk = read_chunk(&mut response, &body_cancellation, idle_timeout).await?;
                let Some(chunk) = chunk else { break };
                budget.consume(&chunk)?;
                yield chunk;
            }
        };
        let stream = async_stream::try_stream! {
            let mut events = Box::pin(bytes.eventsource());
            let mut state = ResponseState::new(tool_names, continuation, assistant_index);
            let mut event_count = 0;
            let mut observed = RestrictedUsage::default();
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(ModelError::Cancelled),
                    event = events.next() => Ok(event),
                }?;
                let Some(event) = event else { break };
                let event = event.map_err(|error| match error {
                    EventStreamError::Transport(error) => error,
                    _ => protocol("Anthropic sent invalid SSE framing or UTF-8"),
                })?;
                event_count += 1;
                if event_count > MAX_STREAM_EVENTS || event.data.len() > MAX_EVENT_BYTES {
                    Err(protocol("Anthropic event stream exceeded its event limits"))?;
                }
                if let Some(accounting) = &accounting {
                    observed.record(&event.data, accounting)?;
                }
                let event: ApiEvent = serde_json::from_str(&event.data)
                    .map_err(|_| protocol("Anthropic sent malformed event JSON"))?;
                let emitted = state.accept(event)?;
                if state.completed
                    && let Some(accounting) = &accounting
                {
                    accounting.observe(&state.usage, true, true)?;
                }
                for event in emitted {
                    yield event;
                }
                if state.completed {
                    break;
                }
            }
            if !state.completed {
                Err(protocol("Anthropic event stream ended without message_stop"))?;
            }
        };
        Ok(ProviderWork {
            value: Box::pin(stream) as ProviderStream,
            root,
            preparation,
        }
        .into_stream())
    }
}

#[derive(Serialize)]
struct NativeRequest {
    model: String,
    messages: Vec<NativeMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<NativeBlock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<NativeTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    stream: bool,
}

#[derive(Serialize)]
struct NativeMessage {
    role: &'static str,
    content: Vec<NativeBlock>,
}

#[derive(Serialize)]
struct NativeTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NativeBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Continuation {
    version: u16,
    messages: Vec<AssistantContinuation>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssistantContinuation {
    assistant_index: usize,
    response_id: String,
    assistant_digest: String,
    opaque_blocks: Vec<OpaquePlacement>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpaquePlacement {
    index: usize,
    block: NativeBlock,
}

type PreparedRequest = (NativeRequest, HashMap<String, String>, Continuation, usize);

struct PreparationConfig {
    default_max_output_tokens: u32,
    thinking_budget_tokens: Option<u32>,
}

struct BoundedRequestBody(Vec<u8>);

impl Write for BoundedRequestBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REQUEST_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("native Anthropic request exceeds 32 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn prepare_request(
    request: ModelRequest,
    config: PreparationConfig,
) -> Result<PreparedRequest, ModelError> {
    if request
        .temperature
        .is_some_and(|temperature| temperature > 1.0)
    {
        return Err(invalid(
            "temperature",
            "Anthropic accepts values between 0 and 1",
        ));
    }
    let max_tokens = request
        .max_output_tokens
        .unwrap_or(config.default_max_output_tokens);
    if let Some(budget) = config.thinking_budget_tokens {
        if budget >= max_tokens {
            return Err(invalid(
                "thinking_budget",
                "must be smaller than max_output_tokens",
            ));
        }
        if request.temperature.is_some()
            || matches!(
                request.tool_choice,
                ToolChoice::Required | ToolChoice::Specific { .. }
            )
        {
            return Err(ModelError::Unsupported {
                feature: "temperature or forced tool choice with Anthropic extended thinking"
                    .to_owned(),
            });
        }
    }
    let continuation = request
        .continuation
        .as_ref()
        .map(|continuation| {
            if continuation.provider() != "anthropic" {
                return Err(invalid("continuation", "belongs to another provider"));
            }
            let continuation: Continuation =
                serde_json::from_value(continuation.state().clone())
                    .map_err(|_| invalid("continuation", "invalid Anthropic continuation"))?;
            if continuation.version != 1
                || continuation.messages.is_empty()
                || continuation.messages.len() > MAX_CONTENT_BLOCKS
            {
                return Err(invalid(
                    "continuation",
                    "unsupported or empty Anthropic continuation",
                ));
            }
            Ok(continuation)
        })
        .transpose()?;
    let assistant_messages: Vec<_> = request
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect();
    let assistant_index = assistant_messages.len();
    let continuation = continuation.unwrap_or(Continuation {
        version: 1,
        messages: Vec::new(),
    });
    let mut previous_index = None;
    for entry in &continuation.messages {
        let message = assistant_messages
            .get(entry.assistant_index)
            .ok_or_else(|| {
                invalid(
                    "continuation",
                    "its assistant message is missing from local history",
                )
            })?;
        if previous_index.is_some_and(|previous| entry.assistant_index <= previous)
            || message_digest(message)? != entry.assistant_digest
            || entry.opaque_blocks.is_empty()
            || message
                .content
                .len()
                .saturating_add(entry.opaque_blocks.len())
                > MAX_CONTENT_BLOCKS
        {
            return Err(invalid(
                "continuation",
                "does not match ordered assistant history",
            ));
        }
        previous_index = Some(entry.assistant_index);
    }
    let mut tool_names = HashMap::with_capacity(request.tools.len());
    let tools = request
        .tools
        .into_iter()
        .map(|tool| {
            let name = native_tool_name(&tool.name);
            if tool_names.insert(name.clone(), tool.name).is_some() {
                return Err(invalid(
                    "tools",
                    "tool names collide after Anthropic encoding",
                ));
            }
            Ok(NativeTool {
                name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(match request.tool_choice {
            ToolChoice::Auto => serde_json::json!({ "type": "auto" }),
            ToolChoice::None => serde_json::json!({ "type": "none" }),
            ToolChoice::Required => serde_json::json!({ "type": "any" }),
            ToolChoice::Specific { name } => {
                serde_json::json!({ "type": "tool", "name": native_tool_name(&name) })
            }
        })
    };
    let mut system = Vec::new();
    if let Some(text) = request.instructions
        && !text.is_empty()
    {
        system.push(NativeBlock::Text { text });
    }
    let mut messages: Vec<NativeMessage> = Vec::new();
    let mut pending_calls = HashSet::new();
    let mut seen_calls = HashSet::new();
    let mut current_assistant_index = 0;
    for message in request.messages {
        if message.role == MessageRole::System {
            if !messages.is_empty() {
                return Err(invalid(
                    "messages",
                    "Anthropic system messages must precede conversation history",
                ));
            }
            for block in message.content {
                if let ContentBlock::Text(text) = block
                    && !text.is_empty()
                {
                    system.push(NativeBlock::Text { text });
                }
            }
            continue;
        }
        if message.role != MessageRole::Tool && !pending_calls.is_empty() {
            return Err(invalid(
                "messages",
                "all tool results must immediately follow their assistant calls",
            ));
        }
        let mut content = Vec::with_capacity(message.content.len());
        for block in message.content {
            content.push(match block {
                ContentBlock::Text(text) => {
                    if text.is_empty() {
                        return Err(invalid(
                            "messages",
                            "Anthropic text blocks must be nonempty",
                        ));
                    }
                    NativeBlock::Text { text }
                }
                ContentBlock::ToolCall(call) => {
                    if !seen_calls.insert(call.id.clone()) {
                        return Err(invalid("messages", "tool call identifiers must be unique"));
                    }
                    pending_calls.insert(call.id.clone());
                    NativeBlock::ToolUse {
                        id: call.id,
                        name: native_tool_name(&call.name),
                        input: call.arguments,
                    }
                }
                ContentBlock::ToolResult(result) => {
                    if !pending_calls.remove(&result.call_id) {
                        return Err(invalid(
                            "messages",
                            "tool result has no pending matching call",
                        ));
                    }
                    let text = match result.output {
                        Value::String(text) => text,
                        output => serde_json::to_string(&output)
                            .map_err(|_| invalid("tool_result", "cannot encode tool output"))?,
                    };
                    NativeBlock::ToolResult {
                        tool_use_id: result.call_id,
                        content: text,
                        is_error: result.is_error,
                    }
                }
            });
        }
        if message.role == MessageRole::Assistant {
            if let Some(entry) = continuation
                .messages
                .iter()
                .find(|entry| entry.assistant_index == current_assistant_index)
            {
                let mut previous = None;
                for placement in &entry.opaque_blocks {
                    if previous.is_some_and(|previous| placement.index <= previous)
                        || placement.index > content.len()
                        || !matches!(
                            placement.block,
                            NativeBlock::Thinking { .. } | NativeBlock::RedactedThinking { .. }
                        )
                    {
                        return Err(invalid(
                            "continuation",
                            "invalid opaque block position or type",
                        ));
                    }
                    match &placement.block {
                        NativeBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            if thinking.contains('\0')
                                || signature.is_empty()
                                || signature.contains('\0')
                            {
                                return Err(invalid(
                                    "continuation",
                                    "invalid signed thinking content",
                                ));
                            }
                        }
                        NativeBlock::RedactedThinking { data } => {
                            if data.is_empty() || data.contains('\0') {
                                return Err(invalid(
                                    "continuation",
                                    "invalid redacted thinking content",
                                ));
                            }
                        }
                        _ => return Err(invalid("continuation", "invalid opaque content type")),
                    }
                    previous = Some(placement.index);
                    content.insert(placement.index, placement.block.clone());
                }
            }
            current_assistant_index += 1;
        }
        if content.is_empty() {
            return Err(invalid(
                "messages",
                "empty assistant history requires its opaque continuation",
            ));
        }
        let role = if message.role == MessageRole::Assistant {
            "assistant"
        } else {
            "user"
        };
        if let Some(previous) = messages.last_mut().filter(|previous| previous.role == role) {
            previous.content.extend(content);
        } else {
            messages.push(NativeMessage { role, content });
        }
    }
    if !pending_calls.is_empty() {
        return Err(invalid(
            "messages",
            "tool calls require results before another model request",
        ));
    }
    if messages.is_empty()
        || messages
            .first()
            .is_some_and(|message| message.role != "user")
    {
        return Err(invalid(
            "messages",
            "Anthropic conversation must begin with a user message",
        ));
    }
    Ok((
        NativeRequest {
            model: request.model,
            messages,
            system,
            tools,
            tool_choice,
            max_tokens,
            temperature: request.temperature,
            thinking: config
                .thinking_budget_tokens
                .map(|budget| serde_json::json!({ "type": "enabled", "budget_tokens": budget })),
            stream: true,
        },
        tool_names,
        continuation,
        assistant_index,
    ))
}

fn native_tool_name(name: &str) -> String {
    if name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return name.to_owned();
    }
    let prefix: String = name
        .chars()
        .take(40)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let digest = Sha256::digest(name.as_bytes());
    let suffix: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}_{suffix}")
}

fn message_digest(message: &Message) -> Result<String, ModelError> {
    let bytes = serde_json::to_vec(message)
        .map_err(|_| invalid("continuation", "cannot encode assistant history"))?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiEvent {
    MessageStart {
        message: ApiMessageStart,
    },
    ContentBlockStart {
        index: u32,
        content_block: NativeBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: ApiDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: ApiMessageDelta,
        usage: ApiUsage,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct ApiMessageStart {
    id: String,
    model: String,
    role: String,
    content: Vec<NativeBlock>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
}

#[derive(Default, Deserialize)]
struct ApiUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct AccountingEventKind {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct AccountingMessageStart {
    message: Option<AccountingMessage>,
}

#[derive(Deserialize)]
struct AccountingMessageDelta {
    usage: Option<AccountingUsage>,
}

#[derive(Deserialize)]
struct AccountingMessage {
    usage: Option<AccountingUsage>,
}

#[derive(Deserialize)]
struct AccountingUsage {
    input_tokens: Option<Value>,
    output_tokens: Option<Value>,
    cache_creation_input_tokens: Option<Value>,
    cache_read_input_tokens: Option<Value>,
}

#[derive(Default)]
struct RestrictedUsage {
    raw_input_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: Option<u64>,
    output_tokens: u64,
    input_observed: bool,
    output_observed: bool,
}

impl RestrictedUsage {
    fn record(&mut self, data: &str, accounting: &ProviderAccounting) -> Result<(), ModelError> {
        let kind: AccountingEventKind = serde_json::from_str(data)
            .map_err(|_| protocol("invalid Anthropic accounting event"))?;
        if kind.kind == "message_stop" {
            if !self.input_observed || !self.output_observed {
                accounting.invalidate(true)?;
                return Err(protocol("Anthropic terminal token usage is incomplete"));
            }
            return accounting.observe(&self.usage()?, true, false);
        }
        if !matches!(kind.kind.as_str(), "message_start" | "message_delta") {
            return Ok(());
        }
        let usage = if kind.kind == "message_start" {
            serde_json::from_str::<AccountingMessageStart>(data)
                .map(|envelope| envelope.message.and_then(|message| message.usage))
        } else {
            serde_json::from_str::<AccountingMessageDelta>(data).map(|envelope| envelope.usage)
        };
        let usage = match usage {
            Ok(usage) => usage,
            Err(_) => {
                accounting.invalidate(false)?;
                return Err(protocol("invalid Anthropic accounting envelope"));
            }
        };
        let Some(usage) = usage else {
            accounting.invalidate(false)?;
            return Err(protocol("Anthropic accounting usage is malformed"));
        };
        let mut malformed = false;
        let mut count = |value: Option<Value>| match value {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_u64() {
                Some(tokens) => Some(tokens),
                None => {
                    malformed = true;
                    None
                }
            },
        };
        let usage = ApiUsage {
            input_tokens: count(usage.input_tokens),
            output_tokens: count(usage.output_tokens),
            cache_creation_input_tokens: count(usage.cache_creation_input_tokens),
            cache_read_input_tokens: count(usage.cache_read_input_tokens),
        };
        let invalid = usage
            .input_tokens
            .is_some_and(|input| input < self.raw_input_tokens)
            || usage
                .cache_creation_input_tokens
                .is_some_and(|cache| cache < self.cache_creation_tokens)
            || usage.cache_read_input_tokens.is_some_and(|cache| {
                self.cache_read_tokens
                    .is_some_and(|previous| cache < previous)
            })
            || usage
                .output_tokens
                .is_some_and(|output| output < self.output_tokens);
        if let Some(input) = usage.input_tokens {
            self.raw_input_tokens = input;
            self.input_observed = true;
        }
        if let Some(cache) = usage.cache_creation_input_tokens {
            self.cache_creation_tokens = cache;
        }
        if let Some(cache) = usage.cache_read_input_tokens {
            self.cache_read_tokens = Some(cache);
        }
        if let Some(output) = usage.output_tokens {
            self.output_tokens = output;
            self.output_observed = true;
        }
        let observed = match self.usage() {
            Ok(usage) => usage,
            Err(error) => {
                let lower_bound = TokenUsage {
                    input_tokens: self
                        .raw_input_tokens
                        .max(self.cache_creation_tokens)
                        .max(self.cache_read_tokens.unwrap_or(0)),
                    output_tokens: self.output_tokens,
                    cached_input_tokens: self.cache_read_tokens,
                    reasoning_tokens: None,
                };
                accounting.observe(&lower_bound, false, false)?;
                accounting.invalidate(false)?;
                return Err(error);
            }
        };
        accounting.observe(&observed, false, false)?;
        if invalid || malformed {
            accounting.invalidate(false)?;
            return Err(protocol(
                "Anthropic cumulative token usage is malformed or decreased",
            ));
        }
        Ok(())
    }

    fn usage(&self) -> Result<TokenUsage, ModelError> {
        let input_tokens = self
            .raw_input_tokens
            .checked_add(self.cache_creation_tokens)
            .and_then(|tokens| tokens.checked_add(self.cache_read_tokens.unwrap_or(0)))
            .ok_or_else(|| protocol("Anthropic token usage overflowed"))?;
        Ok(TokenUsage {
            input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cache_read_tokens,
            reasoning_tokens: None,
        })
    }
}

struct BlockState {
    native: NativeBlock,
    visible_index: Option<u32>,
    arguments: String,
    completed: bool,
}

struct ResponseState {
    id: Option<String>,
    model: Option<String>,
    blocks: Vec<BlockState>,
    tool_names: HashMap<String, String>,
    tool_call_ids: HashSet<String>,
    visible_blocks: u32,
    usage: TokenUsage,
    raw_input_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: Option<u64>,
    stop_reason: Option<StopReason>,
    accumulated_bytes: usize,
    completed: bool,
    continuation: Continuation,
    assistant_index: usize,
}

impl ResponseState {
    fn new(
        tool_names: HashMap<String, String>,
        continuation: Continuation,
        assistant_index: usize,
    ) -> Self {
        Self {
            id: None,
            model: None,
            blocks: Vec::new(),
            tool_names,
            tool_call_ids: HashSet::new(),
            visible_blocks: 0,
            usage: TokenUsage::default(),
            raw_input_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: None,
            stop_reason: None,
            accumulated_bytes: 0,
            completed: false,
            continuation,
            assistant_index,
        }
    }

    fn accept(&mut self, event: ApiEvent) -> Result<Vec<ModelEvent>, ModelError> {
        match event {
            ApiEvent::MessageStart { message } => {
                if self.id.is_some() || message.role != "assistant" || !message.content.is_empty() {
                    return Err(protocol("invalid or duplicate Anthropic message_start"));
                }
                validate_response_id(&message.id)?;
                validate_response_id(&message.model)?;
                self.update_usage(message.usage)?;
                self.id = Some(message.id.clone());
                self.model = Some(message.model.clone());
                Ok(vec![
                    ModelEvent::ResponseStarted {
                        id: message.id,
                        model: message.model,
                    },
                    ModelEvent::Usage {
                        usage: self.usage.clone(),
                    },
                ])
            }
            ApiEvent::Ping | ApiEvent::Unknown => Ok(Vec::new()),
            ApiEvent::Error { error } => Err(api_error(&error.kind, None)),
            event => {
                if self.id.is_none() {
                    return Err(protocol("Anthropic content preceded message_start"));
                }
                match event {
                    ApiEvent::ContentBlockStart {
                        index,
                        content_block,
                    } => self.start_block(index, content_block),
                    ApiEvent::ContentBlockDelta { index, delta } => self.delta(index, delta),
                    ApiEvent::ContentBlockStop { index } => self.stop_block(index),
                    ApiEvent::MessageDelta { delta, usage } => {
                        if self.blocks.iter().any(|block| !block.completed) {
                            return Err(protocol(
                                "Anthropic message_delta preceded content completion",
                            ));
                        }
                        self.update_usage(usage)?;
                        if let Some(reason) = delta.stop_reason {
                            if self.stop_reason.is_some() {
                                return Err(protocol("Anthropic sent duplicate stop reasons"));
                            }
                            self.stop_reason = Some(match reason.as_str() {
                                "end_turn" => StopReason::EndTurn,
                                "tool_use" => StopReason::ToolCall,
                                "max_tokens" => StopReason::MaxTokens,
                                "stop_sequence" => StopReason::StopSequence,
                                "refusal" => StopReason::ContentFilter,
                                _ => StopReason::Other(reason),
                            });
                        }
                        Ok(vec![ModelEvent::Usage {
                            usage: self.usage.clone(),
                        }])
                    }
                    ApiEvent::MessageStop => self.finish(),
                    _ => Err(protocol("invalid Anthropic event order")),
                }
            }
        }
    }

    fn start_block(
        &mut self,
        index: u32,
        native: NativeBlock,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        if index as usize != self.blocks.len()
            || self.blocks.len() >= MAX_CONTENT_BLOCKS
            || self.stop_reason.is_some()
        {
            return Err(protocol(
                "invalid Anthropic content block index or lifecycle",
            ));
        }
        let encoded_bytes = serde_json::to_vec(&native)
            .map_err(|_| protocol("cannot encode Anthropic content block"))?
            .len();
        self.add_bytes(encoded_bytes)?;
        let visible_index = if matches!(
            native,
            NativeBlock::Text { .. } | NativeBlock::ToolUse { .. }
        ) {
            let index = self.visible_blocks;
            self.visible_blocks += 1;
            Some(index)
        } else {
            None
        };
        let events = match &native {
            NativeBlock::Text { text } => {
                validate_response_text(text, MAX_TEXT_BYTES)?;
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![ModelEvent::TextDelta {
                        index: visible_index
                            .ok_or_else(|| protocol("Anthropic visible block has no index"))?,
                        delta: text.clone(),
                    }]
                }
            }
            NativeBlock::ToolUse { id, name, input } => {
                let name = self
                    .tool_names
                    .get(name)
                    .ok_or_else(|| protocol("Anthropic requested an unadvertised tool"))?;
                ToolCall::new(id.clone(), name.clone(), input.clone())
                    .map_err(|_| protocol("Anthropic sent an invalid tool call"))?;
                if !self.tool_call_ids.insert(id.clone()) {
                    return Err(protocol("Anthropic reused a tool call identifier"));
                }
                vec![ModelEvent::ToolCallStarted {
                    index: visible_index
                        .ok_or_else(|| protocol("Anthropic visible block has no index"))?,
                    id: id.clone(),
                    name: name.clone(),
                }]
            }
            NativeBlock::Thinking {
                thinking,
                signature,
            } => {
                validate_response_text(thinking, MAX_CONTINUATION_BYTES)?;
                validate_response_text(signature, MAX_JSON_BYTES)?;
                Vec::new()
            }
            NativeBlock::RedactedThinking { data } => {
                validate_response_text(data, MAX_CONTINUATION_BYTES)?;
                Vec::new()
            }
            NativeBlock::ToolResult { .. } => {
                return Err(protocol(
                    "Anthropic returned a tool result as assistant content",
                ));
            }
        };
        self.blocks.push(BlockState {
            native,
            visible_index,
            arguments: String::new(),
            completed: false,
        });
        Ok(events)
    }

    fn delta(&mut self, index: u32, delta: ApiDelta) -> Result<Vec<ModelEvent>, ModelError> {
        let length = match &delta {
            ApiDelta::Text { text } => text.len(),
            ApiDelta::InputJson { partial_json } => partial_json.len(),
            ApiDelta::Thinking { thinking } => thinking.len(),
            ApiDelta::Signature { signature } => signature.len(),
        };
        self.add_bytes(length)?;
        let block = self
            .blocks
            .get_mut(index as usize)
            .filter(|block| !block.completed)
            .ok_or_else(|| protocol("Anthropic delta references an absent or completed block"))?;
        match (&mut block.native, delta) {
            (NativeBlock::Text { text }, ApiDelta::Text { text: delta }) => {
                append_bounded(text, &delta, MAX_TEXT_BYTES)?;
                Ok(vec![ModelEvent::TextDelta {
                    index: block
                        .visible_index
                        .ok_or_else(|| protocol("Anthropic visible block has no index"))?,
                    delta,
                }])
            }
            (NativeBlock::ToolUse { input, .. }, ApiDelta::InputJson { partial_json }) => {
                if input.as_object().is_none_or(|input| !input.is_empty()) {
                    return Err(protocol(
                        "Anthropic streamed arguments over nonempty initial input",
                    ));
                }
                append_bounded(&mut block.arguments, &partial_json, MAX_JSON_BYTES)?;
                Ok(vec![ModelEvent::ToolCallArgumentsDelta {
                    index: block
                        .visible_index
                        .ok_or_else(|| protocol("Anthropic visible block has no index"))?,
                    delta: partial_json,
                }])
            }
            (NativeBlock::Thinking { thinking, .. }, ApiDelta::Thinking { thinking: delta }) => {
                append_bounded(thinking, &delta, MAX_CONTINUATION_BYTES)?;
                Ok(Vec::new())
            }
            (NativeBlock::Thinking { signature, .. }, ApiDelta::Signature { signature: delta }) => {
                append_bounded(signature, &delta, MAX_JSON_BYTES)?;
                Ok(Vec::new())
            }
            _ => Err(protocol(
                "Anthropic delta does not match its content block type",
            )),
        }
    }

    fn stop_block(&mut self, index: u32) -> Result<Vec<ModelEvent>, ModelError> {
        let block = self
            .blocks
            .get_mut(index as usize)
            .filter(|block| !block.completed)
            .ok_or_else(|| protocol("Anthropic completed an absent or completed content block"))?;
        if let NativeBlock::ToolUse { input, .. } = &mut block.native
            && !block.arguments.is_empty()
        {
            *input = serde_json::from_str(&block.arguments)
                .map_err(|_| protocol("Anthropic returned malformed tool argument JSON"))?;
        }
        if let NativeBlock::Thinking { signature, .. } = &block.native
            && signature.is_empty()
        {
            return Err(protocol(
                "Anthropic thinking block has no continuation signature",
            ));
        }
        let model_block = model_block(&block.native, &self.tool_names)?;
        block.completed = true;
        Ok(match (block.visible_index, model_block) {
            (Some(index), Some(block)) => vec![ModelEvent::ContentBlockCompleted { index, block }],
            (None, None) => Vec::new(),
            _ => return Err(protocol("Anthropic content index mapping is inconsistent")),
        })
    }

    fn update_usage(&mut self, usage: ApiUsage) -> Result<(), ModelError> {
        if let Some(input) = usage.input_tokens {
            self.raw_input_tokens = input;
        }
        if let Some(cache) = usage.cache_creation_input_tokens {
            self.cache_creation_tokens = cache;
        }
        if let Some(cache) = usage.cache_read_input_tokens {
            self.cache_read_tokens = Some(cache);
        }
        self.usage.input_tokens = self
            .raw_input_tokens
            .checked_add(self.cache_creation_tokens)
            .and_then(|tokens| tokens.checked_add(self.cache_read_tokens.unwrap_or(0)))
            .ok_or_else(|| protocol("Anthropic token usage overflowed"))?;
        self.usage.cached_input_tokens = self.cache_read_tokens;
        if let Some(output) = usage.output_tokens {
            if output < self.usage.output_tokens {
                return Err(protocol("Anthropic cumulative output usage decreased"));
            }
            self.usage.output_tokens = output;
        }
        self.usage
            .validate()
            .map_err(|_| protocol("Anthropic returned inconsistent token usage"))
    }

    fn add_bytes(&mut self, bytes: usize) -> Result<(), ModelError> {
        self.accumulated_bytes = self
            .accumulated_bytes
            .checked_add(bytes)
            .filter(|bytes| *bytes <= MAX_REQUEST_BYTES)
            .ok_or_else(|| protocol("Anthropic accumulated output exceeded 32 MiB"))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<ModelEvent>, ModelError> {
        if self.blocks.iter().any(|block| !block.completed) {
            return Err(protocol(
                "Anthropic stopped before content blocks completed",
            ));
        }
        let stop_reason = self
            .stop_reason
            .take()
            .ok_or_else(|| protocol("Anthropic message_stop has no stop reason"))?;
        let mut content = Vec::with_capacity(self.visible_blocks as usize);
        let mut opaque_blocks = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if let Some(block) = model_block(&block.native, &self.tool_names)? {
                content.push(block);
            } else {
                opaque_blocks.push(OpaquePlacement {
                    index,
                    block: block.native.clone(),
                });
            }
        }
        let message = Message {
            role: MessageRole::Assistant,
            content,
        };
        let has_tool_call = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        if stop_reason == StopReason::ToolCall && !has_tool_call {
            return Err(protocol(
                "Anthropic stop reason does not match its tool calls",
            ));
        }
        let id = self
            .id
            .take()
            .ok_or_else(|| protocol("Anthropic response has no ID"))?;
        if !opaque_blocks.is_empty() {
            if self.continuation.messages.len() >= MAX_CONTENT_BLOCKS {
                return Err(protocol(
                    "Anthropic continuation exceeded its history limit",
                ));
            }
            self.continuation.messages.push(AssistantContinuation {
                assistant_index: self.assistant_index,
                response_id: id.clone(),
                assistant_digest: message_digest(&message)?,
                opaque_blocks,
            });
        }
        let continuation = if self.continuation.messages.is_empty() {
            None
        } else {
            let state = serde_json::to_value(&self.continuation)
                .map_err(|_| protocol("could not encode Anthropic continuation"))?;
            Some(
                ProviderContinuation::new("anthropic", state)
                    .map_err(|_| protocol("Anthropic continuation exceeded its size limit"))?,
            )
        };
        let response = ModelResponse {
            id,
            model: self
                .model
                .take()
                .ok_or_else(|| protocol("Anthropic response has no model"))?,
            message,
            usage: self.usage.clone(),
            stop_reason,
            continuation,
        };
        response
            .validate()
            .map_err(|_| protocol("Anthropic completed response violates model limits"))?;
        self.completed = true;
        Ok(vec![ModelEvent::Completed { response }])
    }
}

fn model_block(
    native: &NativeBlock,
    tool_names: &HashMap<String, String>,
) -> Result<Option<ContentBlock>, ModelError> {
    Ok(match native {
        NativeBlock::Text { text } => Some(ContentBlock::Text(text.clone())),
        NativeBlock::ToolUse { id, name, input } => {
            let name = tool_names
                .get(name)
                .ok_or_else(|| protocol("Anthropic requested an unadvertised tool"))?;
            Some(ContentBlock::ToolCall(
                ToolCall::new(id.clone(), name.clone(), input.clone())
                    .map_err(|_| protocol("Anthropic returned invalid completed tool arguments"))?,
            ))
        }
        NativeBlock::Thinking { .. } | NativeBlock::RedactedThinking { .. } => None,
        NativeBlock::ToolResult { .. } => {
            return Err(protocol(
                "Anthropic response contains an invalid tool result",
            ));
        }
    })
}

#[derive(Default)]
struct WireBudget {
    total: usize,
    frame: usize,
    line: usize,
    previous_cr: bool,
}

impl WireBudget {
    fn consume(&mut self, bytes: &[u8]) -> Result<(), ModelError> {
        self.total = self
            .total
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_STREAM_BYTES)
            .ok_or_else(|| protocol("Anthropic event stream exceeded 64 MiB"))?;
        for byte in bytes {
            self.frame += 1;
            if self.frame > MAX_EVENT_BYTES {
                return Err(protocol("Anthropic SSE frame exceeded 2 MiB"));
            }
            if self.previous_cr && *byte == b'\n' {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = *byte == b'\r';
            if matches!(*byte, b'\r' | b'\n') {
                if self.line == 0 {
                    self.frame = 0;
                }
                self.line = 0;
            } else {
                self.line += 1;
            }
        }
        Ok(())
    }
}

async fn read_chunk(
    response: &mut Response,
    cancellation: &CancellationToken,
    idle_timeout: Duration,
) -> Result<Option<bytes::Bytes>, ModelError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ModelError::Cancelled),
        chunk = tokio::time::timeout(idle_timeout, response.chunk()) => {
            chunk.map_err(|_| transport("Anthropic response idle timeout"))?
                .map_err(|_| transport("Anthropic response stream disconnected"))
        }
    }
}

async fn http_error(
    mut response: Response,
    cancellation: &CancellationToken,
    idle_timeout: Duration,
) -> ModelError {
    let status = response.status();
    let retry_after_ms = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1_000));
    let mut bytes = Vec::new();
    loop {
        match read_chunk(&mut response, cancellation, idle_timeout).await {
            Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= MAX_ERROR_BYTES => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            Err(ModelError::Cancelled) => return ModelError::Cancelled,
            _ => break,
        }
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return ModelError::Authentication;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ModelError::RateLimited { retry_after_ms };
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        error: ApiError,
    }
    if let Ok(body) = serde_json::from_slice::<ErrorBody>(&bytes) {
        return api_error(&body.error.kind, Some(status));
    }
    ModelError::Provider {
        code: Some(format!("http_{}", status.as_u16())),
        message: format!("Anthropic request failed with HTTP {}", status.as_u16()),
        retryable: status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT,
    }
}

fn api_error(kind: &str, status: Option<StatusCode>) -> ModelError {
    match kind {
        "authentication_error" | "permission_error" => ModelError::Authentication,
        "rate_limit_error" => ModelError::RateLimited {
            retry_after_ms: None,
        },
        kind => {
            let code = match kind {
                "invalid_request_error"
                | "not_found_error"
                | "request_too_large"
                | "api_error"
                | "overloaded_error" => kind,
                _ => "unknown_api_error",
            };
            ModelError::Provider {
                code: Some(code.to_owned()),
                message: "Anthropic rejected or could not complete the request".to_owned(),
                retryable: matches!(code, "api_error" | "overloaded_error")
                    || status.is_some_and(|status| {
                        status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT
                    }),
            }
        }
    }
}

fn append_bounded(target: &mut String, delta: &str, limit: usize) -> Result<(), ModelError> {
    if target
        .len()
        .checked_add(delta.len())
        .is_none_or(|length| length > limit)
        || delta.contains('\0')
    {
        return Err(protocol(
            "Anthropic content exceeded its limit or contained NUL",
        ));
    }
    target.push_str(delta);
    Ok(())
}

fn validate_response_text(text: &str, limit: usize) -> Result<(), ModelError> {
    if text.len() > limit || text.contains('\0') {
        return Err(protocol(
            "Anthropic content exceeded its limit or contained NUL",
        ));
    }
    Ok(())
}

fn validate_response_id(value: &str) -> Result<(), ModelError> {
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(protocol(
            "Anthropic returned an invalid response identifier",
        ));
    }
    Ok(())
}

fn invalid(field: &str, message: &str) -> ModelError {
    ModelError::InvalidRequest {
        field: field.to_owned(),
        message: message.to_owned(),
    }
}

fn protocol(message: &str) -> ModelError {
    ModelError::Protocol {
        message: message.to_owned(),
    }
}

fn transport(message: &str) -> ModelError {
    ModelError::Transport {
        message: message.to_owned(),
    }
}
