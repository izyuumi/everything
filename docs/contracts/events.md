# Domain and event contracts

`src/protocol.rs` defines the durable conversation wire contract for issue [#2](https://github.com/izyuumi/everything/issues/2). It contains no provider API structures, transport encryption, persistence engine, or runtime executor.

## Authority and identifiers

The local host is authoritative. Clients submit commands; only the host accepts commands, changes task state, and appends conversation events. Receiving or decrypting a client message does not authorize its contents.

`ConversationId`, `MessageId`, `TaskId`, `CommandId`, `EventId`, `ToolCallId`, `OperationId`, and `DeviceId` are distinct UUID types. They serialize as UUID strings, reject malformed and nil UUIDs, and generate UUIDv4 values with `new()`. `as_uuid()` exposes a borrowed UUID for established encoding libraries. A model provider's tool-call identifier is opaque external metadata. `ToolCall.provider_call_id` preserves that exact string alongside the native `ToolCallId`; host-initiated tool calls may omit it. The runtime must persist the mapping and use the provider identifier when constructing model tool results, rather than parse arbitrary provider identifiers as native UUIDs.

Command IDs are unique across the host's command journal. A retried command retains its original ID and exact contents. The host must atomically deduplicate command acceptance and event append, return the original outcome for an identical replay, and reject reuse of an ID for different contents. UUID generation makes collisions unlikely; it does not replace the durable uniqueness constraint. `issuing_device_id` is attribution only and must be checked against authenticated device identity.

## Versioning and serialization

`SchemaVersion::CURRENT` and `CURRENT_SCHEMA_VERSION` are version `1`. `EventEnvelope`, `CommandEnvelope`, and `TaskRecord` require an explicit `schema_version` field. Deserialization rejects missing or unsupported versions. Constructors always select the current version. There is no implicit downgrade or migration.

Enums use snake-case names. `ConversationEvent`, `Command`, and `MessageContent` are internally tagged with `kind`. Unknown event or command kinds fail deserialization. Unknown additional object fields are ignored to permit additive fields within a compatible schema. Changes to required fields or existing meanings require a new schema version and an explicit migration strategy.

`UnixMillis` is a nonnegative Unix timestamp in milliseconds, bounded to SQLite's signed 64-bit integer range. `now()` reports clock errors. `EventSequence` is a positive `u64`, begins at `FIRST` (`1`), and reports overflow from `next()`. The storage layer may enforce a narrower sequence bound for its integer encoding.

## Commands

`CommandEnvelope` contains `schema_version`, `command_id`, `conversation_id`, optional `issuing_device_id`, `issued_at`, and `payload`. A create-conversation command proposes the envelope's conversation ID; only host acceptance creates that conversation. Client timestamps are informational and cannot determine authoritative event ordering.

| Command kind | Payload |
| --- | --- |
| `create_conversation` | Optional title |
| `send_message` | Durable `ConversationMessage` |
| `create_task` | Instruction and optional parent task ID |
| `steer_task` | Task ID and new instruction |
| `cancel_task` | Task ID |
| `resolve_permission` | Operation ID and `allow` or `deny` decision |

These data structures do not execute commands or grant permissions. The receiving host must enforce command authorization, allowed message roles, conversation/task ownership, parent relationships, instruction and payload limits, and valid pending permission operations before committing changes. A permission decision authorizes only its referenced operation; persistent grants have a separate policy contract.

## Append-only events

`EventEnvelope` contains `schema_version`, `event_id`, `conversation_id`, `sequence`, `occurred_at`, optional command `causation_id`, and `payload`. The host assigns immutable event IDs, contiguous per-conversation sequences, and host timestamps when appending. Replay order is sequence order, independent of timestamps. The envelope type validates scalar encoding; atomic append and cross-record invariants belong to storage and the runtime.

The event payload supports conversation creation, appended messages, assistant text deltas, task creation and status changes, tool requests and completion, permission requests and decisions, and accepted steering instructions. Durable message append carries the complete message. Text deltas are incremental output for a message ID; consumers replace accumulated deltas with the final appended message rather than concatenate its text again. Deltas alone do not prove successful task completion.

`task_created` contains the initial `TaskRecord`. `task_status_changed` contains the new status and optional structured failure. A host must apply the same status-transition and failure rules used by `TaskRecord::transition`. A task or message referenced by an event must belong to the envelope's conversation. Events originating from a command carry its ID as `causation_id`; background work may omit it.

The bounded sync payload contract below reuses these envelopes. Network framing, encryption, authenticated event delivery, and event-store access belong to their transport and synchronization implementations.

## Encrypted synchronization payloads

Issue [#7](https://github.com/izyuumi/everything/issues/7) uses these structures inside authenticated encrypted application payloads. Conversation IDs, event sequences, commands, and events must not become plaintext relay metadata. Types and validation alone do not authenticate a sender or perform synchronization.

`SyncCursor { conversation_id, after_sequence }` identifies the last host event already accepted and persisted by a client. Sequence zero requests the start of the conversation; subsequent cursor values come from accepted host events. A cursor cannot assert new host history or authorize access. The host must verify device access before serving events and reject a cursor beyond its authoritative journal.

`SyncRequest::new(cursors, max_events)` creates a versioned request. `cursors` contains 1–64 distinct conversation IDs. `max_events` is a per-conversation batch cap between 1 and 256. Serialized requests are limited to 16 KiB. `validate()` enforces these limits; deserialization also validates them.

`SyncBatch::new(cursor, events, has_more)` returns one conversation's events after the exact requested cursor. `schema_version` is required. Batches contain at most 256 events and serialize to at most 512 KiB, leaving room inside the cryptographic envelope's ciphertext limit. Validation requires matching conversation and schema, unique event IDs, and strictly ordered contiguous sequences starting at `after_sequence + 1`. Sequence overflow is an error. An empty batch must have `has_more: false`; a batch promising more events must advance the cursor.

`SyncBatch::validate_for(&request)` additionally verifies that its cursor was requested and its event count respects the requested cap. Consumers must call it before accepting a response. `next_cursor()` returns the last event sequence, or the original cursor for an empty batch. Advance the durable client cursor only in the same transaction that persists the accepted events. A client retry retains its prior durable cursor until acceptance succeeds. Duplicate delivery across requests must be checked against the existing local journal; within-batch validation does not check history.

`has_more` describes the host's snapshot while producing the batch. A false value does not prevent later host events. The host emits another bounded batch per requested conversation as needed. There is no aggregate multi-conversation batch object or unbounded catch-up response.

These structs have public fields for integration, so call `validate()` again after mutation. Their deserializers enforce semantic bounds after decoding; transport code must enforce ciphertext/plaintext byte limits before JSON deserialization. The host must ensure each appendable event can fit a bounded sync batch or explicitly reject that event for synchronized conversations. Oversized events fail validation; they are never truncated or skipped. Chunking large durable messages and atomic client application are integration responsibilities of [#22](https://github.com/izyuumi/everything/issues/22).

## Durable conversation messages

`ConversationMessage` contains `id`, `role`, `content`, and `created_at`. Roles are `system`, `user`, `assistant`, and `tool`. Content blocks preserve text, native tool calls, and tool results:

- Text contains a UTF-8 string.
- `ToolCall` contains `id`, optional `provider_call_id`, `name`, and JSON `arguments`.
- `ToolResult` contains `tool_call_id`, textual `content`, and `is_error`.

Tool argument schema validation, names, execution limits, result redaction, and duplicate-call policy belong to the Tool Broker. JSON arguments are confined to the external tool boundary. Durable messages are separate from provider-neutral model requests and streamed adapter responses, which can carry richer model capabilities. Runtime integration must explicitly map these records, preserve provider tool-call identifiers, and recover the mapping from durable call records after a restart. Persisted image or other binary content is not implemented by this contract.

## Durable task state

`TaskRecord` contains its schema version, ID, conversation ID, optional parent, originating command ID, instruction, status, creation/update timestamps, and optional `TaskFailure { code, message, retryable }`. `new()` creates a queued task with no failure. Parent existence and acyclic relationships require host validation.

| Current state | Allowed next states |
| --- | --- |
| `queued` | `running`, `failed`, `cancelled` |
| `running` | `queued`, `awaiting_permission`, `waiting_for_children`, `completed`, `failed`, `cancelled` |
| `awaiting_permission`, `waiting_for_children` | `queued`, `running`, `failed`, `cancelled` |
| `completed`, `failed`, `cancelled` | None |

`transition()` rejects other transitions, decreasing update timestamps, failed states without a failure, and nonfailed states with a failure. It changes no fields on rejection. Returning an interrupted task to `queued` supports a host recovery decision; it does not by itself resume or retry execution. Terminal task records are immutable under this API. A deliberate retry creates a new task and command identity.

Struct fields are public for integration and serialization. Deserialization guarantees typed IDs, timestamps, sequence, and schema version; it does not enforce cross-field task consistency, authorization, message-role policy, or historical transitions. The host must use validated constructors and transitions when modifying durable state and validate any imported records against its journal.

## Validation limits

Validation completed with `cargo fmt --all --check`, `cargo check --workspace --all-targets --locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo build --workspace --release --locked`. A disposable manual executable serialized and deserialized a tool-request event while preserving its opaque provider call ID, observed rejection of schema version 2, nil UUIDs, and sequence zero, and observed terminal task transition rejection with unchanged state. A second disposable manual executable roundtripped a sync batch, observed cursor advancement, and observed rejection of sequence gaps, mismatched conversations, duplicate request cursors, zero caps, empty nonterminal batches, and oversized payloads. No provider requests or user data were involved. Automated roundtrip, schema-rejection, and synchronization-bound assertions required by #2/#7 are deliberately deferred to consolidated follow-up [#32](https://github.com/izyuumi/everything/issues/32); those test acceptance criteria are not claimed as verified. Durable deduplication, event append ordering, runtime recovery, authenticated client commands, and cross-client synchronization require their owning implementation issues.
