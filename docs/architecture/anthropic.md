# Native Anthropic Messages adapter

`src/model/anthropic.rs` implements `ModelProvider` with the native streaming Messages API. `AnthropicProvider::new(api_key)` uses `https://api.anthropic.com/v1/messages` and API version `2023-06-01`. It has no external gateway, credential lookup, automatic retries, detached task, or tool executor. The host chooses whether to configure and call this adapter.

## Configuration and credentials

`AnthropicConfig::new(api_key)` validates a nonempty ASCII credential and stores it in a sensitive HTTP header. Both configuration and provider debug output redact that header; errors omit raw response bodies, credentials, request data, and raw transport/parser errors. The consumed input string uses zeroizing memory while constructing the header. HTTP libraries may keep other credential copies; this does not claim complete memory erasure.

`AnthropicProvider::with_config(config)` accepts explicit configuration built with:

- `with_loopback_endpoint(url)` for a user's local Messages proxy. It permits HTTP or HTTPS only with an explicit loopback IP, and rejects URL credentials, queries, and fragments. It disables environment proxies for that local endpoint. Custom remote endpoints and DNS hostnames are rejected.
- `with_timeouts(connect, request, idle)` for positive timeouts bounded to one hour. Defaults are 15 seconds to connect, 600 seconds for the complete request, and 90 seconds between response chunks.
- `with_default_max_output_tokens(tokens)` to change the default 4096 output-token cap. A request-specific cap takes precedence. No model-specific context/output limits are guessed.
- `with_thinking_budget(tokens)` to request native extended thinking. The budget must be at least 1024 tokens and smaller than the effective output-token cap. Temperature and forced tool selection are rejected in this mode. Actual model support is determined by the native API.

Redirects are disabled so credentials cannot follow an endpoint redirect. Constructing the provider does not issue a network request. The adapter never loads credentials from disk or writes them to storage.

## Request translation

Every request passes the shared model validation before network access. Validation, native mapping, history hashing, and bounded serialization run on Tokio's blocking pool. Cancelling while awaiting preparation prevents network access; an already-running bounded preparation job may finish after cancellation and has no network or durable side effects. Only scalar configuration values accompany the request into that worker; credential headers stay in the provider. Anthropic temperatures are further limited to 0–1. Instructions and leading system messages become ordered top-level system text blocks. A system message appearing after conversation history is rejected. Conversation history must begin with a user message.

Assistant text and tool calls become native `text` and `tool_use` blocks. Tool results become native `tool_result` blocks in a user message; string outputs are passed as text and other JSON values are serialized. Tool identifiers are preserved exactly. Calls must be unique across supplied history, each result must match a pending call, and all results must immediately follow their assistant calls before further conversation content. Consecutive tool/user messages are merged without reordering their blocks, keeping results ahead of subsequent user text. A new provider request cannot contain unresolved calls. Consecutive same-role messages are combined in their existing order.

Anthropic tool names support at most 64 ASCII letters, digits, underscores, and hyphens. Names already satisfying that rule are retained. Other names receive a deterministic readable prefix and SHA-256-derived suffix; collisions in advertised names are rejected. Completed model calls restore the original registry name. A response naming an unadvertised tool fails instead of assigning it an unknown meaning. Tool schema enforcement and execution authority remain with the registry and broker.

Tool choice maps auto, none, required, and specific selection to Anthropic's native forms. The adapter sends no unrequested built-in server tools, computer use, search, citations, image blocks, or other content extensions. Unsupported response block/delta types fail explicitly; unknown top-level event types are ignored for forward compatibility.

## Stream and cancellation

The adapter owns the HTTP response directly in the returned stream. `eventsource-stream` parses SSE and UTF-8; a framing budget checks bytes before they reach the parser. Limits are 64 MiB of wire data, 2 MiB per SSE frame/event, 262144 parsed events, 1024 native blocks, 32 MiB of accumulated output, and the model contract's text/JSON/continuation limits. Native request JSON is capped at 32 MiB. Error bodies are read only up to 64 KiB. HTTP headers and connection resources remain subject to the HTTP client's own limits.

`message_start` emits response identity and initial usage. Visible block indexes count only text/tool blocks. Text and argument fragments emit incremental events. Tool arguments must parse as a JSON object before block completion; partial fragments never become executable calls. Content indexes, block lifetimes, duplicate call IDs, type-mismatched deltas, missing signatures, and invalid event order are rejected. `message_delta` updates cumulative usage and the native stop reason. Only a valid `message_stop` produces `Completed`. Disconnects or end-of-stream before that event fail with a protocol/transport error; partial output is not promoted into a completed response.

Cancellation is checked before connection setup, during the HTTP request, while reading body chunks, and while consuming already buffered SSE events. A cancelled request yields `ModelError::Cancelled`. Dropping the stream drops its response and pending reads. There is no producer task or unbounded channel to outlive the consumer.

## Usage, stops, and errors

Anthropic fresh input, cache creation, and cache reads are summed into model `input_tokens`. Cache-read usage is also retained as the cached-input subset. Output usage is cumulative; overflow and decreasing output counters fail. Reasoning usage remains absent because the API does not provide a separate reliable reasoning-token counter here.

Native `end_turn`, `tool_use`, `max_tokens`, `stop_sequence`, and `refusal` map to their shared stop reasons. Other reasons remain exact opaque strings. A `tool_use` stop without a tool call is rejected. An output-limit response may contain no visible blocks, or incomplete tool arguments may fail parsing; the host must account for the stop reason before deciding how to continue.

HTTP 401/403 map to authentication and 429 to rate limiting, preserving an integer-seconds `Retry-After` when present. Known native error types map to fixed, sanitized codes/messages. Other HTTP failures retain only the status code and retryability. The runtime owns retry decisions because the provider may already have processed a request. Raw error messages are intentionally excluded because they can contain submitted content or credentials.

## Opaque continuation

Native Messages is stateless and receives explicit local history on every call. Its continuation contains no invented server cursor. The adapter preserves signed `thinking` and `redacted_thinking` blocks as a versioned ledger with each assistant ordinal, response ID, digest of its visible model message, and original native block positions. Opaque thinking is not exposed as model text events. It is reinserted into the corresponding assistant messages before replaying history, including across multiple tool rounds. Returning no new thinking retains prior ledger entries. A response without any accumulated opaque blocks returns no continuation.

The owning provider tag, ledger order, content digests, native positions/types, signatures, and size limits are validated before replay. The signature is provider-owned opaque data; this adapter preserves it and does not claim to verify its provider-specific cryptographic meaning. Empty visible assistant content is valid when opaque blocks remain. No empty text is invented for it.

Continuation is bounded to 4 MiB and 1024 ledger entries. Rewritten, reordered, or removed assistant history fails its ordinal/digest check. A context compaction that intentionally changes history must also clear this provider continuation, then supply a coherent compacted history without dangling tool calls. Callers must preserve the continuation alongside unchanged history when replaying signed thinking. The continuation ledger is sensitive conversation state, not a credential store.

## Validation status

This implementation is checked with Rust formatting, workspace all-target compilation, Clippy with warnings denied, and release compilation. Constructor/cancellation inspection uses only disposable input and does not contact a provider. No live Anthropic request, charged inference, signed-thinking interoperability, or real provider tool roundtrip has been verified. Automated HTTP, serialization, ordering, cancellation, and rate-limit checks required by [#8](https://github.com/izyuumi/everything/issues/8) remain deferred in [#32](https://github.com/izyuumi/everything/issues/32); those acceptance criteria are unverified.
