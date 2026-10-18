use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::accounting::{ProviderAccounting, ProviderWork};
use super::{
    ContentBlock, MAX_CONTENT_BLOCKS, MAX_CONTINUATION_BYTES, MAX_JSON_BYTES, MAX_REQUEST_BYTES,
    MAX_TEXT_BYTES, Message, MessageRole, ModelError, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ProviderCapabilities, ProviderContinuation, ProviderStream, StopReason,
    TokenUsage, ToolCall, ToolChoice,
};

const PROVIDER_ID: &str = "openai";
const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const MAX_SSE_EVENT_BYTES: usize = MAX_REQUEST_BYTES + 64 * 1_024;
const MAX_STREAM_BYTES: usize = 128 * 1_024 * 1_024;
const MAX_STREAM_EVENTS: usize = 262_144;
const MAX_OUTPUT_ITEMS: usize = 1_024;

#[derive(Clone)]
pub struct OpenAiConfig {
    authorization: HeaderValue,
    endpoint: Url,
    connect_timeout: Duration,
    request_timeout: Duration,
    idle_timeout: Duration,
    accounting: Option<Arc<ProviderAccounting>>,
}

impl OpenAiConfig {
    pub fn new(api_key: impl AsRef<str>) -> Result<Self, ModelError> {
        let api_key = api_key.as_ref();
        if api_key.is_empty()
            || api_key.len() > 4_096
            || api_key
                .chars()
                .any(|value| value.is_control() || value.is_whitespace())
        {
            return Err(invalid("api_key", "must be a nonempty bounded credential"));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| invalid("api_key", "cannot be encoded as an authorization header"))?;
        authorization.set_sensitive(true);
        Ok(Self {
            authorization,
            endpoint: Url::parse(DEFAULT_ENDPOINT)
                .map_err(|_| invalid("endpoint", "the official endpoint is invalid"))?,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(120),
            accounting: None,
        })
    }

    pub(crate) fn with_accounting(mut self, accounting: Arc<ProviderAccounting>) -> Self {
        self.accounting = Some(accounting);
        self
    }

    pub fn with_endpoint(mut self, endpoint: &str) -> Result<Self, ModelError> {
        self.endpoint = parse_endpoint(endpoint, false)?;
        Ok(self)
    }

    pub fn with_loopback_endpoint(mut self, endpoint: &str) -> Result<Self, ModelError> {
        self.endpoint = parse_endpoint(endpoint, true)?;
        Ok(self)
    }

    pub fn with_timeouts(
        mut self,
        connect: Duration,
        request: Duration,
        idle: Duration,
    ) -> Result<Self, ModelError> {
        if connect.is_zero()
            || request.is_zero()
            || idle.is_zero()
            || connect > request
            || idle > request
            || request > Duration::from_secs(3_600)
        {
            return Err(invalid(
                "timeouts",
                "timeouts must be positive, at most one hour, and bounded by request timeout",
            ));
        }
        self.connect_timeout = connect;
        self.request_timeout = request;
        self.idle_timeout = idle;
        Ok(self)
    }
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiConfig")
            .field(
                "endpoint_origin",
                &self.endpoint.origin().ascii_serialization(),
            )
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    client: Client,
    config: OpenAiConfig,
}

impl OpenAiProvider {
    pub fn new(api_key: impl AsRef<str>) -> Result<Self, ModelError> {
        Self::with_config(OpenAiConfig::new(api_key)?)
    }

    pub fn with_config(config: OpenAiConfig) -> Result<Self, ModelError> {
        let mut builder = Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if is_loopback_endpoint(&config.endpoint) {
            builder = builder.no_proxy();
            if config.endpoint.host_str() == Some("localhost") {
                let port = config
                    .endpoint
                    .port_or_known_default()
                    .ok_or_else(|| invalid("endpoint", "endpoint has no valid port"))?;
                builder = builder.resolve("localhost", SocketAddr::from(([127, 0, 0, 1], port)));
            }
        }
        let client = builder
            .build()
            .map_err(|_| transport("could not initialize the HTTP client"))?;
        Ok(Self { client, config })
    }
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: true,
            continuation: true,
            temperature: true,
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
        let mut request = ProviderWork::new(request, self.config.accounting.as_ref())?;
        request.fork_preparation()?;
        let preparation = tokio::task::spawn_blocking(move || {
            request.prepare(|request| {
                request.validate()?;
                let continuation = read_continuation(&request)?;
                let body = prepare_request(&request, &continuation)?;
                Ok::<_, ModelError>((request, continuation, body))
            })
        });
        let ProviderWork {
            value,
            root,
            preparation,
        } = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
            prepared = preparation => prepared.map_err(|_| transport("request preparation failed"))?,
        };
        let (request, continuation, body) = value?;
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
            response = self.client.post(self.config.endpoint.clone())
                .header(AUTHORIZATION, self.config.authorization.clone())
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "text/event-stream")
                .body(body)
                .send() => response.map_err(map_transport_error)?,
        };
        if !response.status().is_success() {
            return Err(http_error(response.status(), response.headers()));
        }
        if response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_none_or(|value| !value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(protocol("expected a server-sent event response"));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_STREAM_BYTES as u64)
        {
            return Err(protocol("stream body exceeds its byte limit"));
        }
        let idle_timeout = self.config.idle_timeout;
        let accounting = self.config.accounting.clone();
        let loopback = is_loopback_endpoint(&self.config.endpoint);
        let raw = async_stream::try_stream! {
            let mut bytes = response.bytes_stream();
            let mut limits = RawLimits::default();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(map_transport_error)?;
                limits.observe(&chunk)?;
                yield chunk;
            }
        };
        let stream = async_stream::try_stream! {
            let events = raw.eventsource();
            futures_util::pin_mut!(events);
            let mut state = StreamState {
                loopback,
                ..StreamState::default()
            };
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(ModelError::Cancelled),
                    next = tokio::time::timeout(idle_timeout, events.next()) => {
                        next.map_err(|_| transport("event stream idle timeout"))
                    },
                }?;
                let event = next.ok_or_else(|| protocol("stream ended without a terminal event"))?
                    .map_err(|error| match error {
                        eventsource_stream::EventStreamError::Transport(error) => error,
                        _ => protocol("invalid server-sent event framing"),
                    })?;
                if event.data.len() > MAX_SSE_EVENT_BYTES {
                    Err(protocol("server-sent event exceeds its byte limit"))?;
                }
                if let Some(accounting) = &accounting {
                    observe_terminal_usage(&event.data, accounting)?;
                }
                let frame: WireFrame = serde_json::from_str(&event.data)
                    .map_err(|_| protocol("invalid server-sent event JSON"))?;
                if event.event != "message" && !event.event.is_empty() && frame.event.kind().is_some_and(|kind| event.event != kind) {
                    Err(protocol("server-sent event type does not match its payload"))?;
                }
                let progress = state.apply(frame)?;
                for event in progress.events {
                    if cancellation.is_cancelled() {
                        Err(ModelError::Cancelled)?;
                    }
                    yield event;
                }
                if let Some(response) = progress.response {
                    let response = finish_response(response, &request, continuation, &state)?;
                    if let Some(accounting) = &accounting {
                        accounting.observe(&response.usage, true, true)?;
                    }
                    for (index, block) in response.message.content.iter().enumerate() {
                        if cancellation.is_cancelled() {
                            Err(ModelError::Cancelled)?;
                        }
                        yield ModelEvent::ContentBlockCompleted {
                            index: u32::try_from(index).map_err(|_| protocol("content index overflow"))?,
                            block: block.clone(),
                        };
                    }
                    yield ModelEvent::Usage { usage: response.usage.clone() };
                    if cancellation.is_cancelled() {
                        Err(ModelError::Cancelled)?;
                    }
                    yield ModelEvent::Completed { response };
                    break;
                }
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

fn parse_endpoint(endpoint: &str, loopback: bool) -> Result<Url, ModelError> {
    let url = Url::parse(endpoint).map_err(|_| invalid("endpoint", "must be an absolute URL"))?;
    let is_loopback = is_loopback_endpoint(&url);
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (loopback && !is_loopback)
        || (url.scheme() != "https" && !(loopback && url.scheme() == "http"))
    {
        return Err(invalid(
            "endpoint",
            "requires HTTPS or explicitly configured loopback HTTP without URL credentials, query, or fragment",
        ));
    }
    Ok(url)
}

fn is_loopback_endpoint(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: Vec<InputItem>,
    tools: Vec<WireTool<'a>>,
    tool_choice: Value,
    stream: bool,
    store: bool,
    include: [&'static str; 1],
    truncation: &'static str,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
    strict: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputItem {
    Message {
        role: String,
        content: Vec<InputText>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        encrypted_content: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct InputText {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ContinuationState {
    version: u16,
    model: String,
    last_response_id: String,
    turns: Vec<ReasoningTurn>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ReasoningTurn {
    history_len: usize,
    history_sha256: String,
    insertions: Vec<ReasoningInsertion>,
    phases: Vec<MessagePhase>,
}

#[derive(Clone, Serialize, Deserialize)]
struct MessagePhase {
    before_block: usize,
    after_block: usize,
    phase: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ReasoningInsertion {
    before_block: usize,
    item: StoredReasoning,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredReasoning {
    id: String,
    summary: Vec<ReasoningSummary>,
    encrypted_content: String,
}

fn read_continuation(request: &ModelRequest) -> Result<ContinuationState, ModelError> {
    let Some(continuation) = &request.continuation else {
        return Ok(ContinuationState {
            version: 1,
            model: request.model.clone(),
            last_response_id: String::new(),
            turns: Vec::new(),
        });
    };
    if continuation.provider() != PROVIDER_ID {
        return Err(invalid("continuation", "belongs to a different provider"));
    }
    let state: ContinuationState = serde_json::from_value(continuation.state().clone())
        .map_err(|_| invalid("continuation", "invalid OpenAI continuation state"))?;
    if state.model != request.model {
        return Err(invalid(
            "continuation",
            "model changed; clear native continuation when changing models",
        ));
    }
    if state.version != 1 || state.turns.len() > MAX_OUTPUT_ITEMS {
        return Err(invalid(
            "continuation",
            "unsupported or oversized OpenAI continuation",
        ));
    }
    let hashes = history_prefix_hashes(&request.messages, &state.turns)?;
    let mut previous_len = 0;
    for turn in &state.turns {
        if turn.history_len <= previous_len || turn.history_len > request.messages.len() {
            return Err(invalid(
                "continuation",
                "history does not contain the continued assistant turn",
            ));
        }
        let message = &request.messages[turn.history_len - 1];
        if message.role != MessageRole::Assistant
            || hashes.get(&turn.history_len) != Some(&turn.history_sha256)
            || (turn.insertions.is_empty() && turn.phases.is_empty())
            || turn.insertions.len() > MAX_OUTPUT_ITEMS
            || turn.phases.len() > MAX_OUTPUT_ITEMS
        {
            return Err(invalid(
                "continuation",
                "history changed; clear continuation when compacting or editing history",
            ));
        }
        let mut previous_block = 0;
        for insertion in &turn.insertions {
            if insertion.before_block < previous_block
                || insertion.before_block > message.content.len()
            {
                return Err(invalid(
                    "continuation",
                    "invalid reasoning insertion position",
                ));
            }
            validate_stored_reasoning(&insertion.item)
                .map_err(|_| invalid("continuation", "invalid encrypted reasoning item"))?;
            previous_block = insertion.before_block;
        }
        let mut previous_end = 0;
        for phase in &turn.phases {
            if phase.before_block < previous_end
                || phase.before_block >= phase.after_block
                || phase.after_block > message.content.len()
                || !matches!(phase.phase.as_str(), "commentary" | "final_answer")
                || message.content[phase.before_block..phase.after_block]
                    .iter()
                    .any(|block| !matches!(block, ContentBlock::Text(_)))
            {
                return Err(invalid("continuation", "invalid assistant message phase"));
            }
            previous_end = phase.after_block;
        }
        previous_len = turn.history_len;
    }
    Ok(state)
}

fn prepare_request(
    request: &ModelRequest,
    continuation: &ContinuationState,
) -> Result<Vec<u8>, ModelError> {
    if request.temperature.is_some()
        && (request.model.starts_with("o1")
            || request.model.starts_with("o3")
            || request.model.starts_with("o4")
            || request.model.starts_with("gpt-5"))
    {
        return Err(ModelError::Unsupported {
            feature: "temperature for the selected reasoning model".to_owned(),
        });
    }
    for tool in &request.tools {
        validate_tool_name(&tool.name)?;
    }
    let mut input = Vec::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        let turn = continuation
            .turns
            .iter()
            .find(|turn| turn.history_len == message_index + 1);
        let mut text = Vec::new();
        let mut current_phase = None;
        for block_index in 0..=message.content.len() {
            let phase = turn
                .and_then(|turn| {
                    turn.phases.iter().find(|phase| {
                        phase.before_block <= block_index && block_index < phase.after_block
                    })
                })
                .map(|phase| phase.phase.as_str());
            if current_phase != phase {
                flush_text(&mut input, &mut text, message.role, current_phase);
                current_phase = phase;
            }
            if let Some(turn) = turn {
                for insertion in turn
                    .insertions
                    .iter()
                    .filter(|item| item.before_block == block_index)
                {
                    flush_text(&mut input, &mut text, message.role, current_phase);
                    input.push(InputItem::Reasoning {
                        id: insertion.item.id.clone(),
                        summary: insertion.item.summary.clone(),
                        encrypted_content: insertion.item.encrypted_content.clone(),
                    });
                }
            }
            let Some(block) = message.content.get(block_index) else {
                break;
            };
            match block {
                ContentBlock::Text(value) => text.push(InputText {
                    kind: match message.role {
                        MessageRole::Assistant => "output_text",
                        _ => "input_text",
                    }
                    .to_owned(),
                    text: value.clone(),
                }),
                ContentBlock::ToolCall(call) => {
                    flush_text(&mut input, &mut text, message.role, current_phase);
                    validate_tool_name(&call.name)?;
                    input.push(InputItem::FunctionCall {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: serde_json::to_string(&call.arguments)
                            .map_err(|_| invalid("arguments", "cannot encode tool arguments"))?,
                    });
                }
                ContentBlock::ToolResult(result) => {
                    flush_text(&mut input, &mut text, message.role, current_phase);
                    let output = if result.is_error {
                        serde_json::json!({"is_error": true, "output": result.output})
                    } else {
                        result.output.clone()
                    };
                    input.push(InputItem::FunctionCallOutput {
                        call_id: result.call_id.clone(),
                        output: serde_json::to_string(&output)
                            .map_err(|_| invalid("output", "cannot encode tool result"))?,
                    });
                }
            }
        }
        flush_text(&mut input, &mut text, message.role, current_phase);
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| WireTool {
            kind: "function",
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.input_schema,
            strict: false,
        })
        .collect();
    let tool_choice = match &request.tool_choice {
        ToolChoice::Auto => Value::String("auto".to_owned()),
        ToolChoice::None => Value::String("none".to_owned()),
        ToolChoice::Required => Value::String("required".to_owned()),
        ToolChoice::Specific { name } => serde_json::json!({"type": "function", "name": name}),
    };
    let native = WireRequest {
        model: &request.model,
        instructions: request.instructions.as_deref(),
        input,
        tools,
        tool_choice,
        stream: true,
        store: false,
        include: ["reasoning.encrypted_content"],
        truncation: "disabled",
        parallel_tool_calls: true,
        max_output_tokens: request.max_output_tokens,
        temperature: request.temperature,
    };
    let mut body = BoundedBody(Vec::new());
    serde_json::to_writer(&mut body, &native)
        .map_err(|_| invalid("request", "native request exceeds its byte limit"))?;
    Ok(body.0)
}

fn flush_text(
    input: &mut Vec<InputItem>,
    text: &mut Vec<InputText>,
    role: MessageRole,
    phase: Option<&str>,
) {
    if !text.is_empty() {
        let role = match role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        input.push(InputItem::Message {
            role: role.to_owned(),
            content: std::mem::take(text),
            phase: phase.map(str::to_owned),
        });
    }
}

struct BoundedBody(Vec<u8>);

impl Write for BoundedBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REQUEST_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("native request byte limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct History<'a> {
    messages: &'a [Message],
    appended: Option<&'a Message>,
}

impl Serialize for History<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(
            self.messages.len() + usize::from(self.appended.is_some()),
        ))?;
        for message in self.messages {
            sequence.serialize_element(message)?;
        }
        if let Some(message) = self.appended {
            sequence.serialize_element(message)?;
        }
        sequence.end()
    }
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hash_history(messages: &[Message], appended: Option<&Message>) -> Result<String, ModelError> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, &History { messages, appended })
        .map_err(|_| invalid("history", "cannot encode model history"))?;
    Ok(format!("{:x}", writer.0.finalize()))
}

fn history_prefix_hashes(
    messages: &[Message],
    turns: &[ReasoningTurn],
) -> Result<HashMap<usize, String>, ModelError> {
    let wanted: HashSet<usize> = turns.iter().map(|turn| turn.history_len).collect();
    let mut hashes = HashMap::with_capacity(wanted.len());
    let mut writer = HashWriter(Sha256::new());
    writer.0.update(b"[");
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            writer.0.update(b",");
        }
        serde_json::to_writer(&mut writer, message)
            .map_err(|_| invalid("history", "cannot encode model history"))?;
        if wanted.contains(&(index + 1)) {
            let mut snapshot = writer.0.clone();
            snapshot.update(b"]");
            hashes.insert(index + 1, format!("{:x}", snapshot.finalize()));
        }
    }
    Ok(hashes)
}

#[derive(Default)]
struct RawLimits {
    total: usize,
    event: usize,
    line_has_data: bool,
    previous_cr: bool,
}

impl RawLimits {
    fn observe(&mut self, bytes: &[u8]) -> Result<(), ModelError> {
        self.total = self
            .total
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_STREAM_BYTES)
            .ok_or_else(|| protocol("stream body exceeds its byte limit"))?;
        for byte in bytes {
            self.event += 1;
            if self.event > MAX_SSE_EVENT_BYTES {
                return Err(protocol("raw server-sent event exceeds its byte limit"));
            }
            if self.previous_cr && *byte == b'\n' {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = *byte == b'\r';
            if *byte == b'\r' || *byte == b'\n' {
                if !self.line_has_data {
                    self.event = 0;
                }
                self.line_has_data = false;
            } else {
                self.line_has_data = true;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct WireFrame {
    sequence_number: u64,
    #[serde(flatten)]
    event: WireEvent,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WireEvent {
    #[serde(rename = "response.created")]
    Created { response: WireResponse },
    #[serde(rename = "response.in_progress")]
    InProgress { response: WireResponse },
    #[serde(rename = "response.output_item.added")]
    ItemAdded { output_index: u32, item: WireItem },
    #[serde(rename = "response.output_item.done")]
    ItemDone { output_index: u32, item: WireItem },
    #[serde(rename = "response.content_part.added")]
    PartAdded {
        output_index: u32,
        content_index: u32,
        item_id: String,
        part: WirePart,
    },
    #[serde(rename = "response.content_part.done")]
    PartDone {
        output_index: u32,
        content_index: u32,
        item_id: String,
        part: WirePart,
    },
    #[serde(rename = "response.output_text.delta")]
    TextDelta {
        output_index: u32,
        content_index: u32,
        item_id: String,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    TextDone {
        output_index: u32,
        content_index: u32,
        item_id: String,
        text: String,
    },
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta {
        output_index: u32,
        content_index: u32,
        item_id: String,
        delta: String,
    },
    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        output_index: u32,
        content_index: u32,
        item_id: String,
        refusal: String,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    ArgumentsDelta {
        output_index: u32,
        item_id: String,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    ArgumentsDone {
        output_index: u32,
        item_id: String,
        arguments: String,
    },
    #[serde(rename = "response.completed")]
    Completed { response: WireResponse },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: WireResponse },
    #[serde(rename = "response.failed")]
    Failed { response: WireResponse },
    #[serde(rename = "error")]
    Error { code: Option<String> },
    #[serde(
        rename = "response.reasoning_summary_part.added",
        alias = "response.reasoning_summary_part.done",
        alias = "response.reasoning_summary_text.delta",
        alias = "response.reasoning_summary_text.done",
        alias = "response.reasoning_text.delta",
        alias = "response.reasoning_text.done",
        alias = "response.output_text.annotation.added"
    )]
    Metadata,
    #[serde(other)]
    Unknown,
}

impl WireEvent {
    fn kind(&self) -> Option<&'static str> {
        Some(match self {
            Self::Created { .. } => "response.created",
            Self::InProgress { .. } => "response.in_progress",
            Self::ItemAdded { .. } => "response.output_item.added",
            Self::ItemDone { .. } => "response.output_item.done",
            Self::PartAdded { .. } => "response.content_part.added",
            Self::PartDone { .. } => "response.content_part.done",
            Self::TextDelta { .. } => "response.output_text.delta",
            Self::TextDone { .. } => "response.output_text.done",
            Self::RefusalDelta { .. } => "response.refusal.delta",
            Self::RefusalDone { .. } => "response.refusal.done",
            Self::ArgumentsDelta { .. } => "response.function_call_arguments.delta",
            Self::ArgumentsDone { .. } => "response.function_call_arguments.done",
            Self::Completed { .. } => "response.completed",
            Self::Incomplete { .. } => "response.incomplete",
            Self::Failed { .. } => "response.failed",
            Self::Error { .. } => "error",
            Self::Metadata | Self::Unknown => return None,
        })
    }
}

#[derive(Deserialize)]
struct WireResponse {
    id: String,
    model: String,
    status: String,
    output: Vec<WireItem>,
    usage: Option<WireUsage>,
    error: Option<WireError>,
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    reason: String,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    input_tokens_details: Option<InputTokenDetails>,
    output_tokens_details: Option<OutputTokenDetails>,
}

#[derive(Deserialize)]
struct InputTokenDetails {
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct OutputTokenDetails {
    reasoning_tokens: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireItem {
    Message {
        id: String,
        role: String,
        status: String,
        content: Vec<WirePart>,
        phase: Option<String>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: Option<String>,
    },
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        encrypted_content: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Serialize, Deserialize)]
struct ReasoningSummary {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WirePart {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    ReasoningText {},
    #[serde(other)]
    Unknown,
}

impl WirePart {
    fn text(&self) -> Result<(&str, bool), ModelError> {
        match self {
            Self::OutputText { text } => Ok((text, false)),
            Self::Refusal { refusal } => Ok((refusal, true)),
            Self::ReasoningText {} | Self::Unknown => {
                Err(protocol("unsupported output content part"))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum ItemIdentity {
    Message(String),
    Function {
        id: String,
        call_id: String,
        name: String,
    },
    Reasoning(String),
}

impl ItemIdentity {
    fn id(&self) -> &str {
        match self {
            Self::Message(id) | Self::Reasoning(id) => id,
            Self::Function { id, .. } => id,
        }
    }
}

impl WireItem {
    fn identity(&self) -> Result<ItemIdentity, ModelError> {
        let identity = match self {
            Self::Message { id, role, .. } if role == "assistant" => {
                ItemIdentity::Message(id.clone())
            }
            Self::FunctionCall {
                id, call_id, name, ..
            } => {
                bounded_identifier(call_id)?;
                validate_tool_name(name).map_err(|_| protocol("invalid provider function name"))?;
                ItemIdentity::Function {
                    id: id.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                }
            }
            Self::Reasoning { id, .. } => ItemIdentity::Reasoning(id.clone()),
            _ => return Err(protocol("unsupported output item or role")),
        };
        bounded_identifier(identity.id())?;
        Ok(identity)
    }
}

#[derive(PartialEq, Eq)]
enum PendingKind {
    Text { refusal: bool },
    Function { call_id: String, name: String },
}

struct PendingBlock {
    kind: PendingKind,
    data: String,
    done: bool,
}

#[derive(Default)]
struct StreamState {
    response_id: Option<String>,
    model: Option<String>,
    sequence: Option<u64>,
    event_count: usize,
    items: BTreeMap<u32, ItemIdentity>,
    item_ids: HashSet<String>,
    call_ids: HashSet<String>,
    content_bytes: usize,
    reasoning_bytes: usize,
    completed_items: HashSet<u32>,
    reasoning: BTreeMap<u32, StoredReasoning>,
    locations: HashMap<(u32, Option<u32>), usize>,
    blocks: Vec<PendingBlock>,
    loopback: bool,
}

#[derive(Default)]
struct Progress {
    events: Vec<ModelEvent>,
    response: Option<WireResponse>,
}

impl StreamState {
    fn apply(&mut self, frame: WireFrame) -> Result<Progress, ModelError> {
        self.event_count += 1;
        if self.event_count > MAX_STREAM_EVENTS {
            return Err(protocol("stream event count exceeds its limit"));
        }
        if self
            .sequence
            .is_some_and(|previous| frame.sequence_number <= previous)
        {
            return Err(protocol("stream sequence numbers must strictly increase"));
        }
        self.sequence = Some(frame.sequence_number);
        if let WireEvent::Error { code } = &frame.event {
            return Err(provider_error(
                code.as_deref().map_or("unknown", |code| code),
            ));
        }
        if let WireEvent::Failed { response } = &frame.event {
            return Err(provider_error(
                response
                    .error
                    .as_ref()
                    .map_or("server_error", |error| error.code.as_str()),
            ));
        }
        if matches!(&frame.event, WireEvent::Completed { response } if response.status != "completed")
            || matches!(&frame.event, WireEvent::Incomplete { response } if response.status != "incomplete")
        {
            return Err(protocol(
                "terminal event type does not match response status",
            ));
        }
        let mut progress = Progress::default();
        if let WireEvent::Created { response } = frame.event {
            if self.response_id.is_some() {
                return Err(protocol("response started more than once"));
            }
            bounded_identifier(&response.id)?;
            bounded_identifier(&response.model)?;
            if !response.output.is_empty()
                || !matches!(response.status.as_str(), "in_progress" | "queued")
            {
                return Err(protocol("invalid initial response state"));
            }
            progress.events.push(ModelEvent::ResponseStarted {
                id: response.id.clone(),
                model: response.model.clone(),
            });
            self.response_id = Some(response.id);
            self.model = Some(response.model);
            return Ok(progress);
        }
        if self.response_id.is_none() {
            return Err(protocol("event arrived before response creation"));
        }
        match frame.event {
            WireEvent::InProgress { response } => {
                self.check_response(&response)?;
            }
            WireEvent::ItemAdded { output_index, item } => {
                if self.items.len() >= MAX_OUTPUT_ITEMS || output_index as usize != self.items.len()
                {
                    return Err(protocol("invalid output item ordering or count"));
                }
                let identity = item.identity()?;
                if !self.item_ids.insert(identity.id().to_owned()) {
                    return Err(protocol("duplicate output item identifier"));
                }
                if let ItemIdentity::Function { call_id, .. } = &identity
                    && !self.call_ids.insert(call_id.clone())
                {
                    return Err(protocol("duplicate provider function call identifier"));
                }
                self.items.insert(output_index, identity);
                match item {
                    WireItem::Message { content, .. } => {
                        for (index, part) in content.into_iter().enumerate() {
                            self.add_text(
                                output_index,
                                u32::try_from(index)
                                    .map_err(|_| protocol("content index overflow"))?,
                                part,
                                &mut progress.events,
                            )?;
                        }
                    }
                    WireItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        let index = self.add_block(
                            output_index,
                            None,
                            PendingKind::Function {
                                call_id: call_id.clone(),
                                name: name.clone(),
                            },
                            String::new(),
                        )?;
                        progress.events.push(ModelEvent::ToolCallStarted {
                            index,
                            id: call_id,
                            name,
                        });
                        if !arguments.is_empty() {
                            self.append(output_index, None, &arguments, None)?;
                            progress.events.push(ModelEvent::ToolCallArgumentsDelta {
                                index,
                                delta: arguments,
                            });
                        }
                    }
                    WireItem::Reasoning { .. } => {}
                    WireItem::Unknown => return Err(protocol("unsupported output item")),
                }
            }
            WireEvent::PartAdded {
                output_index,
                content_index,
                item_id,
                part,
            } => {
                self.check_item(output_index, &item_id)?;
                if !self.is_reasoning_part(output_index, &part) {
                    self.add_text(output_index, content_index, part, &mut progress.events)?;
                }
            }
            WireEvent::TextDelta {
                output_index,
                content_index,
                item_id,
                delta,
            } => {
                self.check_item(output_index, &item_id)?;
                let index = self.append(output_index, Some(content_index), &delta, Some(false))?;
                progress.events.push(ModelEvent::TextDelta { index, delta });
            }
            WireEvent::RefusalDelta {
                output_index,
                content_index,
                item_id,
                delta,
            } => {
                self.check_item(output_index, &item_id)?;
                let index = self.append(output_index, Some(content_index), &delta, Some(true))?;
                progress.events.push(ModelEvent::TextDelta { index, delta });
            }
            WireEvent::ArgumentsDelta {
                output_index,
                item_id,
                delta,
            } => {
                self.check_item(output_index, &item_id)?;
                let index = self.append(output_index, None, &delta, None)?;
                progress
                    .events
                    .push(ModelEvent::ToolCallArgumentsDelta { index, delta });
            }
            WireEvent::TextDone {
                output_index,
                content_index,
                item_id,
                text,
            } => {
                self.check_item(output_index, &item_id)?;
                self.complete_block(output_index, Some(content_index), &text, Some(false))?;
            }
            WireEvent::RefusalDone {
                output_index,
                content_index,
                item_id,
                refusal,
            } => {
                self.check_item(output_index, &item_id)?;
                self.complete_block(output_index, Some(content_index), &refusal, Some(true))?;
            }
            WireEvent::ArgumentsDone {
                output_index,
                item_id,
                arguments,
            } => {
                self.check_item(output_index, &item_id)?;
                self.complete_block(output_index, None, &arguments, None)?;
            }
            WireEvent::PartDone {
                output_index,
                content_index,
                item_id,
                part,
            } => {
                self.check_item(output_index, &item_id)?;
                if !self.is_reasoning_part(output_index, &part) {
                    let (text, refusal) = part.text()?;
                    self.complete_block(output_index, Some(content_index), text, Some(refusal))?;
                }
            }
            WireEvent::ItemDone { output_index, item } => {
                if self.items.get(&output_index) != Some(&item.identity()?)
                    || self.completed_items.contains(&output_index)
                {
                    return Err(protocol("completed output item does not match its start"));
                }
                match item {
                    WireItem::Message { content, .. } => {
                        let count = self
                            .locations
                            .keys()
                            .filter(|(output, _)| *output == output_index)
                            .count();
                        if count != content.len() {
                            return Err(protocol("message content does not match streamed blocks"));
                        }
                        for (index, part) in content.iter().enumerate() {
                            let (text, refusal) = part.text()?;
                            self.complete_block(
                                output_index,
                                Some(
                                    u32::try_from(index)
                                        .map_err(|_| protocol("content index overflow"))?,
                                ),
                                text,
                                Some(refusal),
                            )?;
                        }
                    }
                    WireItem::FunctionCall {
                        arguments, status, ..
                    } => {
                        if status
                            .as_deref()
                            .is_some_and(|status| status != "completed")
                        {
                            return Err(protocol("function call did not complete"));
                        }
                        self.complete_block(output_index, None, &arguments, None)?;
                    }
                    WireItem::Reasoning {
                        encrypted_content: None,
                        ..
                    } if self.loopback => {}
                    WireItem::Reasoning {
                        id,
                        summary,
                        encrypted_content,
                    } => {
                        let item = StoredReasoning {
                            id,
                            summary,
                            encrypted_content: encrypted_content.ok_or_else(|| {
                                protocol("completed reasoning is missing encrypted content")
                            })?,
                        };
                        validate_stored_reasoning(&item)?;
                        self.reasoning_bytes = self
                            .reasoning_bytes
                            .checked_add(item.encrypted_content.len())
                            .and_then(|size| {
                                item.summary.iter().try_fold(size, |size, summary| {
                                    size.checked_add(summary.text.len())
                                })
                            })
                            .filter(|size| *size <= MAX_CONTINUATION_BYTES)
                            .ok_or_else(|| {
                                protocol("accumulated reasoning exceeds its byte limit")
                            })?;
                        self.reasoning.insert(output_index, item);
                    }
                    WireItem::Unknown => return Err(protocol("unsupported completed item")),
                }
                self.completed_items.insert(output_index);
            }
            WireEvent::Completed { response } | WireEvent::Incomplete { response } => {
                self.check_response(&response)?;
                if response.output.len() != self.items.len()
                    || self.completed_items.len() != self.items.len()
                {
                    return Err(protocol(
                        "terminal response does not contain all completed output items",
                    ));
                }
                for (index, item) in response.output.iter().enumerate() {
                    if self.items.get(&(index as u32)) != Some(&item.identity()?) {
                        return Err(protocol("terminal output item does not match the stream"));
                    }
                }
                progress.response = Some(response);
            }
            WireEvent::Metadata => {}
            WireEvent::Unknown => return Err(protocol("unsupported stream event")),
            WireEvent::Created { .. } | WireEvent::Error { .. } | WireEvent::Failed { .. } => {
                return Err(protocol("invalid stream state"));
            }
        }
        Ok(progress)
    }

    fn check_response(&self, response: &WireResponse) -> Result<(), ModelError> {
        if self.response_id.as_deref() != Some(response.id.as_str())
            || self.model.as_deref() != Some(response.model.as_str())
        {
            return Err(protocol("response identity changed within a stream"));
        }
        Ok(())
    }

    fn check_item(&self, index: u32, id: &str) -> Result<(), ModelError> {
        if self.items.get(&index).is_none_or(|item| item.id() != id)
            || self.completed_items.contains(&index)
        {
            return Err(protocol(
                "event references an unknown or completed output item",
            ));
        }
        Ok(())
    }

    fn is_reasoning_part(&self, output_index: u32, part: &WirePart) -> bool {
        matches!(part, WirePart::ReasoningText {})
            && matches!(
                self.items.get(&output_index),
                Some(ItemIdentity::Reasoning(_))
            )
    }

    fn add_text(
        &mut self,
        output_index: u32,
        content_index: u32,
        part: WirePart,
        events: &mut Vec<ModelEvent>,
    ) -> Result<(), ModelError> {
        if !matches!(
            self.items.get(&output_index),
            Some(ItemIdentity::Message(_))
        ) {
            return Err(protocol("text content requires an assistant message item"));
        }
        let expected = self
            .locations
            .keys()
            .filter(|(output, _)| *output == output_index)
            .count();
        if content_index as usize != expected {
            return Err(protocol("content parts must be introduced in order"));
        }
        let (text, refusal) = part.text()?;
        let index = self.add_block(
            output_index,
            Some(content_index),
            PendingKind::Text { refusal },
            String::new(),
        )?;
        if !text.is_empty() {
            self.append(output_index, Some(content_index), text, Some(refusal))?;
            events.push(ModelEvent::TextDelta {
                index,
                delta: text.to_owned(),
            });
        }
        Ok(())
    }

    fn add_block(
        &mut self,
        output: u32,
        content: Option<u32>,
        kind: PendingKind,
        data: String,
    ) -> Result<u32, ModelError> {
        if self.blocks.len() >= MAX_CONTENT_BLOCKS
            || self.locations.contains_key(&(output, content))
        {
            return Err(protocol(
                "duplicate content block or content count limit exceeded",
            ));
        }
        let index = self.blocks.len();
        self.blocks.push(PendingBlock {
            kind,
            data,
            done: false,
        });
        self.locations.insert((output, content), index);
        u32::try_from(index).map_err(|_| protocol("content index overflow"))
    }

    fn append(
        &mut self,
        output: u32,
        content: Option<u32>,
        delta: &str,
        refusal: Option<bool>,
    ) -> Result<u32, ModelError> {
        let index = *self
            .locations
            .get(&(output, content))
            .ok_or_else(|| protocol("delta arrived before its block start"))?;
        let block = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| protocol("invalid content block index"))?;
        check_block_kind(&block.kind, refusal)?;
        let limit = if content.is_some() {
            MAX_TEXT_BYTES
        } else {
            MAX_JSON_BYTES
        };
        if block.done
            || delta.len() > limit.saturating_sub(block.data.len())
            || delta.contains('\0')
        {
            return Err(protocol("invalid or oversized content delta"));
        }
        self.content_bytes = self
            .content_bytes
            .checked_add(delta.len())
            .filter(|size| *size <= MAX_REQUEST_BYTES)
            .ok_or_else(|| protocol("accumulated content exceeds its byte limit"))?;
        block.data.push_str(delta);
        u32::try_from(index).map_err(|_| protocol("content index overflow"))
    }

    fn complete_block(
        &mut self,
        output: u32,
        content: Option<u32>,
        value: &str,
        refusal: Option<bool>,
    ) -> Result<(), ModelError> {
        let index = *self
            .locations
            .get(&(output, content))
            .ok_or_else(|| protocol("completed block was not started"))?;
        let block = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| protocol("invalid content block index"))?;
        check_block_kind(&block.kind, refusal)?;
        if block.data != value {
            return Err(protocol("completed block differs from streamed deltas"));
        }
        block.done = true;
        Ok(())
    }
}

fn check_block_kind(kind: &PendingKind, refusal: Option<bool>) -> Result<(), ModelError> {
    if matches!((kind, refusal), (PendingKind::Text { refusal: actual }, Some(expected)) if *actual == expected)
        || matches!((kind, refusal), (PendingKind::Function { .. }, None))
    {
        Ok(())
    } else {
        Err(protocol("stream content block type changed"))
    }
}

#[derive(Deserialize)]
struct AccountingEventKind {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct AccountingEnvelope {
    response: Option<AccountingResponse>,
}

#[derive(Deserialize)]
struct AccountingResponse {
    status: Option<Value>,
    usage: Option<AccountingUsage>,
}

#[derive(Deserialize)]
struct AccountingUsage {
    input_tokens: Option<Value>,
    output_tokens: Option<Value>,
    total_tokens: Option<Value>,
    input_tokens_details: Option<AccountingInputDetails>,
    output_tokens_details: Option<AccountingOutputDetails>,
}

#[derive(Deserialize)]
struct AccountingInputDetails {
    cached_tokens: Option<Value>,
}

#[derive(Deserialize)]
struct AccountingOutputDetails {
    reasoning_tokens: Option<Value>,
}

fn observe_terminal_usage(data: &str, accounting: &ProviderAccounting) -> Result<(), ModelError> {
    let kind: AccountingEventKind =
        serde_json::from_str(data).map_err(|_| protocol("invalid provider accounting event"))?;
    let expected_status = match kind.kind.as_str() {
        "response.completed" => "completed",
        "response.incomplete" => "incomplete",
        "response.failed" => "failed",
        _ => return Ok(()),
    };
    let envelope: AccountingEnvelope = match serde_json::from_str(data) {
        Ok(envelope) => envelope,
        Err(_) => {
            accounting.invalidate(true)?;
            return Err(protocol("invalid terminal provider accounting envelope"));
        }
    };
    let Some(response) = envelope.response else {
        accounting.invalidate(true)?;
        return Err(protocol("terminal response has no accounting envelope"));
    };
    let Some(usage) = response.usage else {
        accounting.invalidate(true)?;
        return Err(protocol("terminal response has no token usage"));
    };
    let input = usage.input_tokens.as_ref().and_then(Value::as_u64);
    let output = usage.output_tokens.as_ref().and_then(Value::as_u64);
    let total = usage.total_tokens.as_ref().and_then(Value::as_u64);
    let mut invalid = input.is_none() || output.is_none() || total.is_none();
    let mut detail = |value: Option<Value>| match value.as_ref().and_then(Value::as_u64) {
        Some(tokens) => Some(tokens),
        None => {
            invalid = true;
            None
        }
    };
    let cached_input_tokens = usage
        .input_tokens_details
        .and_then(|details| detail(details.cached_tokens));
    let reasoning_tokens = usage
        .output_tokens_details
        .and_then(|details| detail(details.reasoning_tokens));
    let observed = TokenUsage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        cached_input_tokens,
        reasoning_tokens,
    };
    accounting.observe(&observed, true, false)?;
    if invalid
        || input.and_then(|input| output.and_then(|output| input.checked_add(output))) != total
        || response.status.as_ref().and_then(Value::as_str) != Some(expected_status)
    {
        accounting.invalidate(true)?;
        return Err(protocol(
            "terminal provider accounting is malformed or inconsistent",
        ));
    }
    Ok(())
}

fn finish_response(
    native: WireResponse,
    request: &ModelRequest,
    mut continuation: ContinuationState,
    state: &StreamState,
) -> Result<ModelResponse, ModelError> {
    let mut content = Vec::new();
    let mut insertions = Vec::new();
    let mut phases = Vec::new();
    let mut refusal = false;
    for (output_index, item) in native.output.into_iter().enumerate() {
        match item {
            WireItem::Message {
                role,
                status,
                content: parts,
                phase,
                ..
            } => {
                if role != "assistant" || !matches!(status.as_str(), "completed" | "incomplete") {
                    return Err(protocol("invalid terminal assistant message"));
                }
                let before_block = content.len();
                for part in parts {
                    let (text, is_refusal) = part.text()?;
                    refusal |= is_refusal;
                    content.push(ContentBlock::Text(text.to_owned()));
                }
                if let Some(phase) = phase {
                    if !matches!(phase.as_str(), "commentary" | "final_answer") {
                        return Err(protocol("unsupported assistant message phase"));
                    }
                    if content.len() > before_block {
                        phases.push(MessagePhase {
                            before_block,
                            after_block: content.len(),
                            phase,
                        });
                    }
                }
            }
            WireItem::FunctionCall {
                call_id,
                name,
                arguments,
                status,
                ..
            } => {
                if status
                    .as_deref()
                    .is_some_and(|status| status != "completed")
                {
                    return Err(protocol("terminal function call is incomplete"));
                }
                let arguments: Value = serde_json::from_str(&arguments)
                    .map_err(|_| protocol("function arguments are not valid JSON"))?;
                let call = ToolCall::new(call_id, name, arguments)
                    .map_err(|_| protocol("function call violates the model contract"))?;
                content.push(ContentBlock::ToolCall(call));
            }
            WireItem::Reasoning {
                encrypted_content, ..
            } => {
                let item = match state.reasoning.get(&(output_index as u32)) {
                    Some(item) => item,
                    None if state.loopback && encrypted_content.is_none() => continue,
                    None => return Err(protocol("reasoning output was not completed")),
                };
                if encrypted_content
                    .as_ref()
                    .is_some_and(|encrypted| encrypted != &item.encrypted_content)
                {
                    return Err(protocol(
                        "terminal encrypted reasoning differs from the completed item",
                    ));
                }
                insertions.push(ReasoningInsertion {
                    before_block: content.len(),
                    item: item.clone(),
                });
            }
            WireItem::Unknown => return Err(protocol("unsupported terminal output item")),
        }
    }
    if content.len() != state.blocks.len() {
        return Err(protocol("terminal content count differs from the stream"));
    }
    for (block, pending) in content.iter().zip(&state.blocks) {
        let matches = match (block, &pending.kind) {
            (ContentBlock::Text(text), PendingKind::Text { .. }) => text == &pending.data,
            (ContentBlock::ToolCall(call), PendingKind::Function { call_id, name }) => {
                call.id == *call_id
                    && call.name == *name
                    && serde_json::from_str::<Value>(&pending.data)
                        .is_ok_and(|arguments| arguments == call.arguments)
            }
            _ => false,
        };
        if !pending.done || !matches {
            return Err(protocol("terminal content differs from the stream"));
        }
    }
    let tool_call = content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)));
    let stop_reason = match native.status.as_str() {
        "completed" if refusal => StopReason::ContentFilter,
        "completed" if tool_call => StopReason::ToolCall,
        "completed" => StopReason::EndTurn,
        "incomplete" => {
            let reason = native
                .incomplete_details
                .ok_or_else(|| protocol("incomplete response has no reason"))?
                .reason;
            match reason.as_str() {
                "max_output_tokens" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                _ => {
                    bounded_identifier(&reason)?;
                    StopReason::Other(reason)
                }
            }
        }
        _ => return Err(protocol("terminal response has an invalid status")),
    };
    let message = Message {
        role: MessageRole::Assistant,
        content,
    };
    if !insertions.is_empty() || !phases.is_empty() {
        if continuation.turns.len() >= MAX_OUTPUT_ITEMS {
            return Err(protocol("native continuation turn limit exceeded"));
        }
        continuation.turns.push(ReasoningTurn {
            history_len: request.messages.len() + 1,
            history_sha256: hash_history(&request.messages, Some(&message))?,
            insertions,
            phases,
        });
    }
    continuation.last_response_id = native.id.clone();
    let continuation = ProviderContinuation::new(
        PROVIDER_ID,
        serde_json::to_value(continuation)
            .map_err(|_| protocol("cannot encode native continuation"))?,
    )
    .map_err(|_| protocol("native continuation exceeds its limit"))?;
    let usage = match native.usage {
        Some(usage) => {
            if usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens) {
                return Err(protocol("provider token usage totals are inconsistent"));
            }
            TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage
                    .input_tokens_details
                    .map(|details| details.cached_tokens),
                reasoning_tokens: usage
                    .output_tokens_details
                    .map(|details| details.reasoning_tokens),
            }
        }
        None => TokenUsage::default(),
    };
    let response = ModelResponse {
        id: native.id,
        model: native.model,
        message,
        usage,
        stop_reason,
        continuation: Some(continuation),
    };
    response
        .validate()
        .map_err(|_| protocol("completed response violates the model contract"))?;
    Ok(response)
}

fn validate_stored_reasoning(item: &StoredReasoning) -> Result<(), ModelError> {
    bounded_identifier(&item.id)?;
    if item.encrypted_content.is_empty()
        || item.encrypted_content.len() > MAX_CONTINUATION_BYTES
        || item.encrypted_content.contains('\0')
        || item.summary.len() > MAX_CONTENT_BLOCKS
    {
        return Err(protocol("invalid encrypted reasoning item"));
    }
    let mut summary_bytes = 0usize;
    for summary in &item.summary {
        summary_bytes = summary_bytes
            .checked_add(summary.text.len())
            .filter(|size| *size <= MAX_CONTINUATION_BYTES)
            .ok_or_else(|| protocol("reasoning summary exceeds its byte limit"))?;
        if summary.kind != "summary_text" || summary.text.contains('\0') {
            return Err(protocol("invalid reasoning summary"));
        }
    }
    Ok(())
}

fn validate_tool_name(name: &str) -> Result<(), ModelError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ModelError::Unsupported {
            feature:
                "OpenAI function names require 1-64 ASCII letters, digits, underscores, or hyphens"
                    .to_owned(),
        });
    }
    Ok(())
}

fn bounded_identifier(value: &str) -> Result<(), ModelError> {
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(protocol("invalid provider identifier"));
    }
    Ok(())
}

fn map_transport_error(error: reqwest::Error) -> ModelError {
    if error.is_timeout() {
        transport("HTTP request timed out")
    } else if error.is_connect() {
        transport("HTTP connection failed")
    } else {
        transport("HTTP request or response failed")
    }
}

fn http_error(status: StatusCode, headers: &HeaderMap) -> ModelError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ModelError::Authentication,
        StatusCode::TOO_MANY_REQUESTS => ModelError::RateLimited {
            retry_after_ms: retry_after(headers),
        },
        _ => ModelError::Provider {
            code: None,
            message: format!("OpenAI returned HTTP {}", status.as_u16()),
            retryable: status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT,
        },
    }
}

fn retry_after(headers: &HeaderMap) -> Option<u64> {
    let millis = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| seconds.saturating_mul(1_000))
        });
    millis.map(|value| value.min(3_600_000))
}

fn provider_error(code: &str) -> ModelError {
    match code {
        "invalid_api_key" | "authentication_error" => ModelError::Authentication,
        "rate_limit_exceeded" | "rate_limit_error" => ModelError::RateLimited {
            retry_after_ms: None,
        },
        _ => ModelError::Provider {
            code: match code {
                "server_error"
                | "invalid_request_error"
                | "context_length_exceeded"
                | "insufficient_quota"
                | "content_policy_violation" => Some(code.to_owned()),
                _ => None,
            },
            message: "OpenAI rejected the streamed response".to_owned(),
            retryable: code == "server_error",
        },
    }
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
