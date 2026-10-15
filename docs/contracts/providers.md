# Provider contract

The model-neutral API lives in `src/model/mod.rs` and `src/model/types.rs`. It does not depend on a model gateway or another agent harness. Concrete adapters implement native provider APIs separately.

## Provider boundary

`ModelProvider: Send + Sync` is object-safe through `async_trait`. `id()` returns the stable adapter identifier. `capabilities()` returns a snapshot of supported streaming, tool calling, parallel calls, opaque continuation, temperature, and optional token limits. Limits and capabilities may vary by model; adapters must reject a request whose selected model cannot support its options.

`stream(ModelRequest, CancellationToken)` asynchronously returns `ProviderStream`, a pinned, owned, sendable stream of `Result<ModelEvent, ModelError>`. The stream can be moved between Tokio workers. Every adapter must call `request.validate()` before network access, check relevant capabilities, and reject continuation whose `provider()` does not match its own identifier. Unsupported options must produce `ModelError::Unsupported` rather than disappear silently.

Cancellation applies during connection setup and response consumption. Adapters observe the supplied token, close pending transport work, and terminate with `ModelError::Cancelled`. Dropping the stream must release the underlying response and stop its producer; implementations must not detach unbounded background producers. A token that is already cancelled must not start a request.

## Request and content

`ModelRequest` carries a model name, optional instructions, ordered messages, tool definitions, tool choice, optional output-token cap and temperature, and optional continuation. `ModelRequest::new(model, messages)` validates the initial request. Its public fields allow context construction; call `validate()` again after edits or deserialization. Serde decoding alone does not validate domain invariants.

`Message` has `MessageRole::{System, User, Assistant, Tool}` and ordered `ContentBlock::{Text, ToolCall, ToolResult}` values. Tool calls occur only in assistant messages. Tool messages contain only results. Text may be empty, but messages contain at least one block. A `ToolCall` has its provider-issued call identifier, tool name, and a JSON object of arguments. A `ToolResult` identifies the call, carries an arbitrary JSON output value, and records whether execution failed. Identifiers are opaque and must be preserved exactly through runtime execution and result submission. Providers must reject invalid argument JSON rather than silently substituting an empty object.

`ToolDefinition` contains a name, description, and JSON-object input schema. The centralized tool registry owns full JSON Schema validation and tool execution. This layer validates shape and size, without granting execution permission. `ToolChoice` supports auto, none, required, and a named tool. Required selection needs available tools, named selection must refer to a provided definition, and tool names must be unique.

General limits are 4096 messages, 1024 content blocks per message, 1024 tool definitions, 16 MiB per text value, 1 MiB per tool schema/arguments/result, 4 MiB per continuation value, and 32 MiB per serialized request or completed response. Identifiers contain no whitespace or control characters and are at most 256 bytes. Text rejects NUL. Output-token caps must be positive; temperature must be finite and within 0–2. Adapters apply stricter provider limits where required. Size validation counts JSON bytes without creating a second serialized buffer. External transports must also bound bodies before deserialization and bound accumulated stream fragments.

## Stream semantics

An adapter emits at most one `ResponseStarted { id, model }`. Content indexes identify positions in the completed assistant message. Distinct blocks may interleave.

- `TextDelta { index, delta }` appends exact text to that block.
- `ToolCallStarted { index, id, name }` introduces a call before argument fragments for its index.
- `ToolCallArgumentsDelta { index, delta }` appends an exact fragment of the JSON argument document. Fragments are not individually valid JSON and cannot be executed.
- `ContentBlockCompleted { index, block }` supplies a fully parsed and validated block. It is the authoritative form for that index.
- `Usage { usage }` supplies a cumulative snapshot for this response, not a delta to add to earlier usage events.
- `Completed { response }` supplies the complete validated response and is the sole success terminal event. Consumers use its message, usage, stop reason, and continuation as authoritative; prior deltas support presentation.

A stream ends after one completed event or one error. End-of-stream without a terminal event is a protocol failure, including a truncated network response. No successful completion may be synthesized from partial output. Consumers must not execute streamed argument fragments. Adapters must parse and validate complete tool arguments before publishing a completed call or response. A response's message always has assistant role.

`TokenUsage` records input and output counts, with optional cached-input and reasoning counts when the provider reports them. Optional counters are subsets of their corresponding input/output totals and must not be added to those totals. Unreported counters remain `None`; a provider that supplies no usage leaves primary counts zero. `total_tokens()` saturates rather than wrapping. Stop reasons preserve end-turn, tool-call, output-limit, stop-sequence, content-filter, and unknown provider reason strings.

## Opaque continuation

`ProviderContinuation::new(provider, state)` validates a provider identifier and bounded JSON state. Runtime and context modules preserve the state without inspecting or rewriting it. Only the owning adapter interprets `state()`. It can hold a response identifier, signed reasoning items, or other native continuation data without introducing provider-specific domain types. Continuation is additional provider state, not a replacement for the authoritative local conversation history. Adapters define whether continuation augments explicit messages or allows sending only a suffix and must prevent replaying the same history twice.

Continuation implements serialization for durable execution state. Its debug representation omits its payload; serialization still includes the payload and must be treated as potentially sensitive conversation data. Adapters must not place credentials in continuation or errors.

## Errors and durable conversion

`ModelError` distinguishes invalid requests, unsupported features, authentication, rate limits with optional retry delay, transport failures, provider failures with code and retryability, malformed provider responses, and cancellation. Adapters sanitize provider error bodies before surfacing them. Retry policy belongs to the runtime and must account for the possibility that a request already reached the provider.

The durable timeline's protocol types retain message identifiers and timestamps; provider messages intentionally omit those storage concerns. Runtime code converts roles and content explicitly. Durable tool calls retain an internal UUID and a separate `provider_call_id`; store the model's opaque call identifier in that field. Results refer to the internal UUID, so reconstruction looks up the original call and restores its provider identifier. Preserve this mapping across recovery. Host-initiated calls may have no provider identifier and must not be submitted as provider call results. A model JSON tool output is serialized into durable tool-result content and decoded when reconstructing model content. Provider calls never grant tool execution authority.

## Validation status

The foundation is validated by Rust formatting, compilation, Clippy, and release compilation. It contains no native adapter and makes no network requests. Automated serialization, stream ordering, cancellation, and tool roundtrip assertions are deferred in [#32](https://github.com/izyuumi/everything/issues/32) under the mission's no-tests requirement; those acceptance criteria are unverified.
