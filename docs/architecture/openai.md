# Native OpenAI Responses adapter

`src/model/openai.rs` implements issue [#9](https://github.com/izyuumi/everything/issues/9) through OpenAI's native HTTP Responses API and server-sent events. It implements the [provider contract](../contracts/providers.md); it does not use a gateway, another harness, or OpenAI's hosted conversation store.

## Construction and configuration

`OpenAiProvider::new(api_key)` creates the official adapter. `OpenAiConfig::new(api_key)` followed by `OpenAiProvider::with_config(config)` provides explicit endpoint and timeout configuration. The API key is a private, sensitive HTTP header and is absent from both config and provider debug output. No adapter state serializes credentials.

The default endpoint is `https://api.openai.com/v1/responses`. `with_endpoint(url)` explicitly selects another HTTPS endpoint and therefore changes the destination of model context and credentials. A caller must obtain user authorization before selecting a new external destination. `with_loopback_endpoint(url)` accepts only loopback hosts and permits HTTP for local operation. Loopback requests bypass environment proxies, and `localhost` is pinned to `127.0.0.1` rather than resolved through a user-supplied DNS or hosts mapping; IPv6-only local services can use `[::1]`. URL credentials, query strings, and fragments are rejected. Redirects are disabled, including redirects from the official endpoint.

Defaults are a 10-second connection timeout, 600-second total request timeout, and 120-second event idle timeout. `with_timeouts(connect, request, idle)` requires positive values, connection and idle limits within the request timeout, and a request timeout at most one hour. There are no automatic retries: retrying after a transport failure can incur another provider request and belongs to the runtime.

## Native request mapping

The adapter validates the provider-neutral request before sending data. Bounded serialization and continuation validation run on Tokio's blocking pool. Cancellation before preparation or HTTP setup prevents network access. A bounded preparation job already running can finish after cancellation; it has no network or durable side effects.

Every request explicitly selects streaming, `store: false`, `truncation: disabled`, parallel function calls, and `include: ["reasoning.encrypted_content"]`. The full authoritative local message history is sent. No `previous_response_id` or hosted conversation identifier is submitted, so replay cannot combine remote stored history with the same local history twice.

Text becomes native message input; assistant calls become `function_call` items with their exact opaque provider `call_id`; tool replies become `function_call_output`. Arguments and results are serialized JSON. Failed tool replies encode `{ "is_error": true, "output": ... }`, since Responses has no independent tool-error flag. Instructions, optional token caps, tool choice, descriptions, and JSON schemas map to their native fields. Function schema strictness is explicitly false so general validated JSON schemas are not silently rewritten to OpenAI's strict-schema subset.

Function names must contain 1–64 ASCII letters, digits, underscores, or hyphens. Unsupported names are rejected before HTTP; the adapter does not rename tools. The generic registry supports a broader namespace, so runtime-selected builtins and MCP aliases must satisfy this native restriction. Temperature is rejected for the `o1`, `o3`, `o4`, and `gpt-5` model families, whose default reasoning modes do not uniformly support that option. Omit it for those families. Other model-specific provider restrictions remain subject to native API rejection; token capability ceilings are not guessed.

## Streaming and completion

`response.created` supplies the response identifier and actual model. The adapter checks strictly increasing native sequence numbers, stable response and item identities, unique output and function-call IDs, ordered item/content introduction, and agreement between streamed fragments and terminal content: text and refusals must match exactly, while function-call arguments must parse as equal JSON values, so serialization whitespace may differ. The executed and persisted arguments are always the terminal completed value. Text and refusal deltas become `TextDelta`; function introductions and argument fragments become their corresponding model events. Logical content indexes exclude reasoning-only items and match the final assistant message.

Only parsed, completed native function calls can appear in a successful response. Partial arguments are never executable. Refusals remain visible as text and produce a content-filter stop reason. Native incomplete responses preserve output-limit or content-filter reasons; other bounded reason identifiers remain `StopReason::Other`. An incomplete function call is a protocol failure.

Authoritative `ContentBlockCompleted` events are emitted after validation of the terminal response, followed by cumulative usage and one `Completed` event. Presentation deltas can precede these events. Consumers must require the terminal success event before treating the turn as successfully completed. Input/output usage and optional cached/reasoning subsets are preserved, with consistency checks. An assistant response may have no visible content when a reasoning-only response reaches its output cap; the provider foundation permits that case.

Unknown output item or content kinds and unsupported stream events fail explicitly. Recognized reasoning and annotation metadata events are not displayed as assistant text. `reasoning_text` content parts are accepted only on reasoning items and are not displayed; they are rejected on assistant messages. Truncated streams, mismatched final content, malformed JSON, missing completed reasoning, and invalid stream ordering produce sanitized protocol errors. A stream ends after a terminal success or error.

## Stateless native continuation

Opaque continuation is tagged `openai` and has its own internal version. It retains completed encrypted reasoning items from `response.output_item.done`, their original positions among visible content blocks, and native assistant `commentary`/`final_answer` phase ranges. Completed reasoning is used because its earlier added-event payload may be incomplete. The adapter does not reveal reasoning text as visible output. Existing continuation records remain present when a later response has no new reasoning or phase metadata.

Each retained turn is anchored to the exact local assistant-history prefix using the established `sha2` SHA-256 implementation over serialized model messages. Prefix hashes are calculated in one pass. Reconstruction verifies the anchor and reinserts reasoning and phases in their original positions. This is a local consistency check, not authentication or a new cryptographic protocol.

A model change, history edits, role changes, or compaction that remove an anchored turn cause a continuation error. The context builder must intentionally clear continuation when replacing history. The adapter never silently drops encrypted reasoning, guesses matching turns, or duplicates a remote history chain. A completed reasoning item without encrypted content is a protocol error on non-loopback endpoints. On a loopback endpoint, such as a local OpenAI-compatible server that streams plaintext `reasoning_text` instead, it has no replayable state: it is neither retained in continuation nor replayed, and later requests send only the visible local history. Native continuation is limited to 4 MiB by the provider contract; exceeding it fails explicitly. Provider response IDs are retained as local metadata only.

## Resource and security boundaries

Native request JSON and the validated completed response are each limited to 32 MiB. Raw SSE framing is checked before the established `eventsource-stream` parser can accumulate unbounded event data. Limits are 32 MiB plus 64 KiB per raw event, 128 MiB per complete stream, and 262,144 events. Output is limited to 1,024 native items and 1,024 visible content blocks. Text fragments are limited to 16 MiB per block, arguments to 1 MiB per call, accumulated visible content to 32 MiB, and accumulated new reasoning to 4 MiB. JSON escaping and duplicated native completion events can make the overall stream limit stricter than individual content limits.

The returned stream owns the response. It has no detached producer or unbounded channel; dropping it releases the response and parser state. Cancellation is observed while connecting, waiting for events, and emitting model events. Event idle and total request timeouts remain active for slow or stalled providers.

HTTP 401/403 map to authentication errors and 429 to rate limiting. Numeric `retry-after-ms` or integer-second `retry-after` values are preserved up to one hour; unknown delay encodings remain unspecified. Other statuses expose only their numeric code and retryability. Native streamed error codes are classified through a small allowlist. Provider error bodies, arbitrary provider error messages, parser diagnostics, request bodies, and reqwest error strings are not included in returned errors. Conversation data and serialized continuation still require normal local storage protection.

## Verification

Validation passed with `cargo fmt --all --check`, `cargo check --workspace --all-targets --locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo build --workspace --release --locked`. A disposable offline executable observed redacted debug output, credential and endpoint rejection, pre-cancelled requests, native-name/temperature rejection, and foreign-provider continuation rejection. Those probes returned before HTTP access; no server was created. There are no new automated tests, fixtures, mock servers, or live credential requests. Native HTTP text/tool roundtrips, server error behavior, SSE ordering/cancellation during active HTTP, encrypted reasoning replay, and phase replay require the deferred automated coverage in [#32](https://github.com/izyuumi/everything/issues/32) or an authorized live provider session. They are not claimed as runtime-verified here.

The wire mapping was checked against OpenAI's current official SDK definitions for [Responses](https://github.com/openai/openai-python/tree/main/src/openai/types/responses), including native input items, streamed event metadata, encrypted reasoning, and assistant phases.
