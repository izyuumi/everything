# Storage contracts

`src/storage.rs` implements durable host storage for issue #3. `Store` owns one SQLite connection behind a shared mutex. Clones share that connection. The API is synchronous: async callers must execute storage calls inside `tokio::task::spawn_blocking` or an equivalent dedicated blocking boundary and must never retain application locks while waiting for storage. No connection or transaction handle escapes the module.

## Initialization and migrations

`Store::open(path)` opens or creates a database at an existing parent directory. Startup enables foreign keys, WAL, full synchronous durability, and a five-second busy timeout before applying `migrations/0001_initial.sql` in an immediate transaction. The schema version is SQLite `user_version`; newer databases are rejected. `open_in_memory` uses the same migration and constraints, with SQLite's in-memory journal mode because WAL is unavailable for memory databases. There is no backup or restore API.

The schema contains conversations, ordered events, command deduplication records, tasks, tool calls, permission grants, devices, and encrypted outbox envelopes. All tables use SQLite STRICT typing. Foreign keys reject dangling references. Parent tasks and associated tool calls must belong to their conversation. IDs are nonempty strings of at most 256 bytes without control characters; domain UUID interpretation belongs to `protocol`. JSON payloads are opaque to storage and limited to 1 MiB each. All timestamps are nonnegative Unix milliseconds. Callers own access control and semantic validation of opaque data.

## Conversation events and commands

`create_conversation` creates a conversation with `last_sequence = 0`. `conversation` reads one record; `conversations(limit)` lists the most recently updated records.

`append(&AppendRequest)` accepts a conversation ID, globally unique command ID, expected last sequence, and 1–256 events containing unique IDs, kind strings, opaque JSON payloads, and occurrence times. A request is limited to 4 MiB. One immediate transaction checks the expected sequence, inserts contiguous events, updates the conversation head, and persists the original request with its sequence range. Any insert or constraint failure rolls back the whole append. Event sequence starts at one and cannot exceed SQLite's signed integer range. Event IDs are globally unique.

An exact retry of a committed append returns its original rows with `AppendOutcome.replayed = true`, even when the expected sequence is now stale. Reusing a command ID with a different original append returns `CommandConflict`. A fresh command with an incorrect expected sequence returns `SequenceConflict` without writing anything. Request equality uses deterministic JSON serialization, including event IDs, timestamps, and expected sequence.

`command_request(command_id)` returns the original append request; `command_outcome(command_id)` returns its persisted events. Hosts receiving a repeated external command must consult these before generating fresh event IDs or timestamps and validate the original command identity against their protocol payload. Storage deduplicates append requests; it does not infer equivalence between different external command envelopes.

`events(conversation_id, after_sequence, limit)` returns events in ascending sequence order. Every list API requires a limit of 1–1000; invalid limits are rejected. Results also have a 16 MiB serialized-page budget; a larger page returns `ReadBudgetExceeded` and the caller must request fewer rows. Event pagination uses an exclusive sequence cursor.

## Tasks and tool operations

`create_task` creates a queued task with opaque input and checkpoint JSON. `task` and `tasks` read tasks, optionally filtering by conversation and state. `transition_task` atomically compares the previous state and writes a new checkpoint and timestamp. `checkpoint_task` updates a nonterminal task without transitioning. Timestamps may not move backward. Terminal tasks cannot be reopened.

Allowed task transitions are queued → running/failed/cancelled, running → queued/waiting/completed/failed/cancelled, and waiting → queued/running/failed/cancelled. Running → queued exists for deliberate recovery after the caller reconciles external effects. Storage never automatically replays task or tool side effects.

`reserve_tool_call` persists a pending operation before execution and returns `{created, call}`. A repeat with the same operation ID, conversation, task, tool name, and request returns the existing row, including its current state and durable result. Different execution inputs return `ToolCallConflict`. Include every permission/execution context field that affects semantics in the opaque request JSON. A caller must inspect the returned journal state before executing; an existing reservation does not authorize a second side effect.

`tool_call` and `tool_calls` read journal entries. `transition_tool_call` atomically compares state, rejects backward timestamps, and persists terminal results. Pending → running/failed/cancelled/denied and running → succeeded/failed/cancelled/unknown are allowed. Running requires no result; every terminal state requires a JSON result. Failed and denied outcomes remain queryable. `unknown` is terminal and prevents automatic retry after an indeterminate external effect.

`mark_running_tool_calls_unknown(result, timestamp)` is an explicit startup recovery operation. It marks every in-flight operation unknown with a supplied diagnostic result and preserves monotonic row timestamps. Invoke it only before starting execution workers, never concurrently with live operations. It does not execute or repeat tools.

## Permissions, devices, and delivery

`create_permission` persists opaque policy data with optional conversation scope and expiration. `active_permissions(conversation_id, now, limit)` returns unrevoked, unexpired grants created by `now`, including global grants and the requested conversation's grants. An expiration equal to `now` is expired. `revoke_permission` records revocation with a compare-and-update operation. The policy engine owns grant interpretation and applicability checks. This method returns `PermissionLimitExceeded` when the complete active set exceeds the requested limit; policy evaluation must fail closed rather than evaluate a truncated set. The read-byte budget also applies.

`create_device` persists the public identity metadata supplied by transport; no private keys belong in that payload. `device` and `devices` expose registrations. Crypto verification and proof of possession belong to transport. `revoke_device` atomically revokes registration and removes pending outbound envelopes for the revoked device. Revocation cannot be undone through this API.

`enqueue` accepts a new encrypted envelope of 1 byte–1 MiB addressed to an existing, active device. Storage does not construct or inspect ciphertext. New envelopes must have zero attempts and no delivery timestamp. `pending_outbox(now, limit)` lists due envelopes for active devices in delivery order. `defer_outbox` atomically increments attempts and advances the next due time; `acknowledge_outbox` durably marks delivery. Revoked devices are excluded from delivery and new enqueue operations.

## Validation and limitations

Formatting, compilation, Clippy, and focused disposable-data inspection passed during implementation. Disposable inspection verified WAL/schema initialization, exact command replay and conflicts, partial-insert rollback, task transitions, interrupted tool recovery, permission expiration, device revocation, foreign-key rejection, and reopen persistence. SQLite reported no foreign-key violations and an `ok` integrity check. Release compilation passed before integration. Automated migration, restart, atomicity, concurrency, and command replay assertions are deferred to [issue #32](https://github.com/izyuumi/everything/issues/32). Database files are local plaintext and require appropriate filesystem permissions from host setup; at-rest encryption is not implemented. Event append and individual journal transitions are durable transactions, but there is no cross-module transaction spanning a model request or external tool side effect.
