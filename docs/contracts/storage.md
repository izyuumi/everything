# Storage contracts

`src/storage.rs` implements durable host storage for issue #3. `Store` owns one SQLite connection behind a shared mutex. Clones share that connection. The API is synchronous: async callers must execute storage calls inside `tokio::task::spawn_blocking` or an equivalent dedicated blocking boundary. Owned asynchronous exclusion guards may remain held through that blocking transaction when the component's lock order requires continuous authority; remote admission transfers those guards and its leased Agent into the actual blocking closure so caller cancellation cannot release them early. Do not block Tokio workers, hold synchronous mutex guards across awaits, or create a dependency cycle in which a storage worker needs a lock held by its waiting caller. The public API exposes no connection or transaction handle. Trusted crate-internal typed repositories may acquire the same mutex-protected connection through `Store::connection`; they must not open a second connection or expose raw SQL publicly.

## Permanent connection quarantine

Every Store clone shares a permanent connection-health latch. `connection()` checks it before attempting the real mutex and again after acquisition, so a waiter cannot inherit a quarantined connection. `quarantine_connection()` is crate-private and sets the shared latch without SQL. A writer must invoke it while still holding that exact Main connection mutex after an observed rollback failure, before releasing the mutex. All subsequent checked borrows return `StorageError::Quarantined`; no reset, retry, repair or alternate connection is provided.

This protects unrelated Store users from an uncertain transaction left active after rollback failure. It does not prove whether a prior commit or rollback occurred, destroy data, retire Source/permit ownership or establish Host drain. The original structured primary and rollback errors remain with their actual worker. Writer integration, strict compilation and disposable engine/concurrency/fault observations remain separate gates; no forced failure was executed for this latch. See [ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md).

## Initialization and migrations

`Store::open(path)` opens or creates a database at an existing parent directory. Startup enables foreign keys, WAL, full synchronous durability, and a five-second busy timeout before applying pending versioned migrations in one immediate transaction. Fresh databases apply migrations `0001_initial.sql` through `0017_settings_transport.sql` in order; existing databases apply only their pending migrations. The current schema version is SQLite `user_version = 17`; newer databases are rejected. The ordered additions are:

| Migration | Addition |
| --- | --- |
| 1 | Initial conversation, task, operation, permission, device and outbox storage |
| 2 | Revisioned private conversation state |
| 3 | Immutable Unknown cleanup attestations |
| 4 | Paired-device transport and replica state |
| 5 | Eventless private checkpoint command identities |
| 6 | Scheduled jobs and immutable run history |
| 7 | Bounded Host command inbox |
| 8 | Immutable first-attempt schedule identities and captured execution policy |
| 9 | Scheduled attempts, outcomes, owner Unknown acknowledgments and delivery outbox |
| 10 | Worker-purpose endpoint, peers, profile, delegations, messages and native admissions |
| 11 | Immutable original worker terminal-packet sources |
| 12 | Indexed, ambiguity-preserving native remote-delegation lookup |
| 13 | Revisioned Settings operations, native requests and immutable operation updates |
| 14 | Restricted Settings namespace, original sessions/turn fences and cleanup attestations |
| 15 | Immutable scoped Settings grant decisions, current heads and reserved revocation capacity |
| 16 | Durable scoped Settings Originals, bounded lifecycle metadata and protected operation roots |
| 17 | Private reply reservations and immutable encrypted outbox associations |

See [scheduled storage](scheduling-storage.md), [host inbox](host-inbox.md), [schedule admission](scheduling-engine.md), [scheduled execution](scheduled-execution.md), [worker terminal sources](worker-terminal-sources.md) and [remote binding lookup](remote-binding-lookup.md) for component invariants. `open_in_memory` uses the same migrations and constraints, with SQLite's in-memory journal mode because WAL is unavailable for memory databases. There is no backup or restore API.

The schema contains conversations, ordered events, command deduplication records, tasks, tool calls, permission grants, devices, and encrypted outbox envelopes. All tables use SQLite STRICT typing. Foreign keys reject dangling references. Parent tasks must exist and may belong to another conversation so child agents can isolate their histories. Associated tool calls must belong to their task's conversation. The scheduler owns parent authority inheritance and child lifecycle enforcement. IDs are nonempty strings of at most 256 bytes without control characters; domain UUID interpretation belongs to `protocol`. JSON payloads are opaque to storage and limited to 1 MiB each. All timestamps are nonnegative Unix milliseconds. Callers own access control and semantic validation of opaque data.

## Conversation events and commands

`create_conversation` creates a conversation with `last_sequence = 0`. `conversation` reads one record; `conversations(limit)` lists the most recently updated records.

`append(&AppendRequest)` accepts a conversation ID, globally unique command ID, expected last sequence, and 1–256 events containing unique IDs, kind strings, opaque JSON payloads, and occurrence times. A request is limited to 4 MiB. One immediate transaction checks the expected sequence, inserts contiguous events, updates the conversation head, and persists the original request with its sequence range. Any insert or constraint failure rolls back the whole append. Event sequence starts at one and cannot exceed SQLite's signed integer range. Event IDs are globally unique.

An exact retry of a committed append returns its original rows with `AppendOutcome.replayed = true`, even when the expected sequence is now stale. Reusing a command ID with a different original append returns `CommandConflict`. A fresh command with an incorrect expected sequence returns `SequenceConflict` without writing anything. Request equality uses deterministic JSON serialization, including event IDs, timestamps, and expected sequence.

`command_request(command_id)` returns the original append request; `command_outcome(command_id)` returns its persisted events. Hosts receiving a repeated external command must consult these before generating fresh event IDs or timestamps and validate the original command identity against their protocol payload. Storage deduplicates append requests; it does not infer equivalence between different external command envelopes.

`events(conversation_id, after_sequence, limit)` returns events in ascending sequence order. Every list API requires a limit of 1–1000; invalid limits are rejected. Results also have a 16 MiB serialized-page budget; a larger page returns `ReadBudgetExceeded` and the caller must request fewer rows. Event pagination uses an exclusive sequence cursor.

## Coherent conversation reads

`conversation_snapshot(conversation_id, after_sequence, limit)` returns `ConversationSnapshot { conversation, state, events }` from one read transaction on the existing connection. It captures the conversation head, private checkpoint, and an ordered event page from the same SQLite snapshot. Unknown conversations return `NotFound`. Events have sequence greater than the supplied cursor and at most the captured `conversation.last_sequence`. An exclusive cursor beyond that head returns an empty event page with the captured metadata.

The 1–1000 row limit and 16 MiB aggregate serialized-page budget apply. The budget includes conversation metadata, the checkpoint payload, event rows, and JSON container overhead. A large checkpoint may require a smaller event page. The API returns no partial snapshot on budget exhaustion.

Raw event rows are immutable. If history spans multiple pages, retain the first snapshot's checkpoint and head, fetch subsequent pages through `events`, and stop at that captured head, filtering any later events returned by an intervening append. Do not combine a captured history with a checkpoint obtained from a later standalone read. A snapshot is private host data because it includes the checkpoint; remote synchronization must use the event-only APIs.

## Private conversation checkpoints

`ConversationStateRow { conversation_id, revision, payload, updated_at }` is a single private host checkpoint, read through `conversation_state(conversation_id)`. `conversation_states` references an existing conversation and stores a positive revision with an opaque JSON payload of at most 8 MiB. There is no bulk state-list API. Provider continuation tokens and other native runtime checkpoints belong here rather than in public event payloads. This state is local to the host and is excluded from conversation-event synchronization, including encrypted client sync. Callers own the checkpoint payload's versioned schema and must protect it like provider credentials or conversation data.

`append_with_state(&AppendRequest, &ConversationStateUpdate { expected_revision, payload })` commits events, conversation sequence, command identity, and a checkpoint update in the same existing-connection immediate transaction. Expected revision zero requires an absent checkpoint and creates revision one. Subsequent updates compare the current revision and increment it by one; stale updates return `StateRevisionConflict { expected, actual }` without writing events, state, or a command record. Revision overflow is rejected. The checkpoint timestamp follows the conversation's monotonic update timestamp derived from appended event times; it never moves backward.

`append` retains its existing signature and leaves the checkpoint unchanged. Command identity also binds whether a state update was supplied. A command committed through `append` cannot be replayed through `append_with_state`, or vice versa. A checkpoint payload or expected-revision change under the same command ID returns `CommandConflict`. State-update identity is SHA-256 over the namespace bytes `everything.conversation.state-update.v1` followed by one NUL byte and the full typed update's JSON serialization, including expected revision. Only that digest is retained in `commands.state_digest`; old checkpoint payloads are not duplicated into command history.

An exact command retry compares original append request and state digest before checking the current sequence or checkpoint revision. It returns the original committed events without rewriting the checkpoint. Replaying an earlier command therefore cannot overwrite a newer checkpoint. `command_request` and `command_outcome` keep their original `AppendRequest` and `AppendOutcome` shapes and do not expose private state.

## Eventless private phase checkpoints

Schema five adds `conversation_checkpoint_commands`, a dedicated private journal for `checkpoint_conversation_state(conversation_id, expected_sequence, command_id, &ConversationStateUpdate) -> ConversationStateRow`. The method advances only the private state revision. It does not append a public event, move the conversation head, update the public conversation timestamp, or encode an empty append. Existing append requests still require 1–256 events.

A new checkpoint uses the existing Store connection and one immediate transaction. It compares the conversation's public head and private revision, increments that revision by one, writes the bounded 8 MiB private payload, and inserts its command journal identity atomically. Expected revision zero creates an absent checkpoint. Stale head or revision returns the existing `SequenceConflict` or `StateRevisionConflict` without changing state or consuming the command ID. Unknown conversations, exhausted revisions and oversized payloads fail explicitly. The returned timestamp is the maximum of existing conversation/state metadata timestamps; actual phase timestamps belong in the caller's private versioned payload.

Identity binds the private-checkpoint namespace, command ID, conversation, expected public head and full typed update, including expected revision. The SHA256 digest covers namespace bytes `everything.conversation.private-checkpoint.v1` followed by NUL and JSON serialization of `(conversation_id, expected_sequence, update)`. A committed exact replay is recognized before current head/revision or capacity checks. It returns the original committed revision and timestamp with the digest-validated payload supplied by the retry. It never rewrites or returns a newer checkpoint. The journal stores only bounded identity metadata/digest, without duplicating historical provider payloads.

Command IDs share one namespace across event appends, event-plus-state appends and private checkpoints. Both append and checkpoint transactions reject cross-kind reuse with `CommandConflict`. Changed conversation, head, revision or payload under a private checkpoint command ID also conflicts. `command_request` and `command_outcome` remain append-only reads and return `None` for a private checkpoint command; callers replay private checkpoints through their dedicated API with the same original update.

The journal retains at most 100,000 private checkpoint identities globally. IDs retain their existing 256-byte limit; each row holds those bounded IDs, a 64-byte digest and fixed integer metadata. At capacity a new private transition returns `CheckpointJournalFull` before mutation, while exact committed replays continue to work. There is no automatic eviction that could forget an accepted identity or silently turn a replay into a new phase. This is an explicit capacity limit, not a retention or backup system; a future owner-directed lifecycle policy must preserve replay safety before changing it. Public append identity retention is unchanged.

Automated checkpoint migration, command-collision, concurrency and recovery coverage is deferred to #32. Disposable manual validation covers absence of public effects, exact old replay after later state/head changes, strict identity conflicts, stale CAS, injected journal-insert rollback, large private payload bounds, reopen, concurrent connections, journal-capacity rejection with replay, migration from schema four and SQLite integrity. These checks validate the storage transaction; external tool effects still belong to the Tool Broker and runtime recovery contracts.

## Tasks and tool operations

`create_task` creates a queued task with opaque input and checkpoint JSON. A parent task may span conversations, while tool calls remain bound to their task's conversation. `task` and `tasks` read tasks, optionally filtering by conversation and state. `transition_task` atomically compares the previous state and writes a new checkpoint and timestamp. `checkpoint_task` updates a nonterminal task without transitioning. Timestamps may not move backward. Terminal tasks cannot be reopened.

Allowed task transitions are queued → running/failed/cancelled, running → queued/waiting/completed/failed/cancelled, and waiting → queued/running/failed/cancelled. Running → queued exists for deliberate recovery after the caller reconciles external effects. Storage never automatically replays task or tool side effects.

The crate-private [external-task transaction](external-task-storage.md) is a separate narrow path for a proven root task in Waiting. It compares the complete original task row and atomically appends the attributed result event, updates the private conversation checkpoint and settles that original external task. It rejects local tool rows for either the task or its dedicated conversation. This does not loosen the general transition table or provide authentication; the actual Agent/Scheduler capability supplies authority.

`reserve_tool_call` persists a pending operation before execution and returns `{created, call}`. A repeat with the same operation ID, conversation, task, tool name, and request returns the existing row, including its current state and durable result. Different execution inputs return `ToolCallConflict`. Include every permission/execution context field that affects semantics in the opaque request JSON. A caller must inspect the returned journal state before executing; an existing reservation does not authorize a second side effect.

`tool_call` and `tool_calls` read journal entries. `transition_tool_call` atomically compares state, rejects backward timestamps, and persists terminal results. Pending → running/failed/cancelled/denied and running → succeeded/failed/cancelled/unknown are allowed. Running requires no result; every terminal state requires a JSON result. Failed and denied outcomes remain queryable. `unknown` is terminal and prevents automatic retry after an indeterminate external effect.

`mark_running_tool_calls_unknown(result, timestamp)` is an explicit startup recovery operation. It marks every in-flight operation unknown with a supplied diagnostic result and preserves monotonic row timestamps. Invoke it only before starting execution workers, never concurrently with live operations. It does not execute or repeat tools.

## Unknown-outcome cleanup audit

`tool_calls_after_id(state, after_id, limit)` provides keyset pagination in ascending operation ID order with the existing row-count and aggregate-byte bounds. Startup quarantine can inspect every unknown operation without treating a full first page as the end of the journal.

`tool_cleanup_status(operation_id)` requires an existing unknown tool call and coherently returns its immutable result digest plus an optional `ToolCleanupAttestation`. The digest is SHA-256 over namespace bytes `everything.tool-call.unknown-result.v1` followed by NUL, then byte one and the exact persisted result JSON bytes. A missing SQL result uses byte zero instead. This preserves identity for unknown legacy formats without rewriting or interpreting them.

`attest_tool_cleanup(&ToolCleanupAttestation { operation_id, original_result_digest, operator, note, attested_at })` checks unknown state, current result digest, and an attestation time at or after the journal's update time in one immediate transaction. The separate audit row is immutable: an exact retry returns the original row, while a changed digest or attestation returns `ToolCleanupConflict`. It never modifies the tool result, reopens the operation, or authorizes retry. The broker exposes this only through a host-owner cleanup review API, which may clear a cleanup quarantine while preserving uncertainty about the original effect. Operator and note fields are bounded and cannot contain control characters. Disposable manual inspection verified keyset page order, initial digest lookup, immutable attestation creation, exact replay, changed-note rejection, and unchanged unknown journal contents.

## Permissions, devices, and delivery

`permission(id)` reads an exact permission record including expired or revoked state, enabling durable approval and revocation recovery. `create_permission` persists opaque policy data with optional conversation scope and expiration. `active_permissions(conversation_id, now, limit)` returns unrevoked, unexpired grants created by `now`, including global grants and the requested conversation's grants. An expiration equal to `now` is expired. `revoke_permission` records revocation with a compare-and-update operation. The policy engine owns grant interpretation and applicability checks. This method returns `PermissionLimitExceeded` when the complete active set exceeds the requested limit; policy evaluation must fail closed rather than evaluate a truncated set. The read-byte budget also applies.

`create_device` persists the public identity metadata supplied by transport; no private keys belong in that payload. `device` and `devices` expose registrations. Crypto verification and proof of possession belong to transport. `revoke_device` atomically revokes registration and removes pending outbound envelopes for the revoked device. Revocation cannot be undone through this API.

`enqueue` accepts a new encrypted envelope of 1 byte–1 MiB addressed to an existing, active device. Storage does not construct or inspect ciphertext. New envelopes must have zero attempts and no delivery timestamp. `pending_outbox(now, limit)` lists due envelopes for active devices in delivery order. `defer_outbox` atomically increments attempts and advances the next due time; `acknowledge_outbox` durably marks delivery. Revoked devices are excluded from delivery and new enqueue operations.

## Validation and limitations

Formatting, compilation, Clippy, and focused disposable-data inspection passed during implementation. Disposable inspection verified WAL/schema initialization, exact command replay and conflicts, partial-insert rollback, task transitions, interrupted tool recovery, permission expiration, device revocation, foreign-key rejection, and reopen persistence. SQLite reported no foreign-key violations and an `ok` integrity check. Release compilation passed before integration. Automated migration, restart, atomicity, concurrency, and command replay assertions are deferred to [issue #32](https://github.com/izyuumi/everything/issues/32). Snapshot inspection verified matching captured head/checkpoint, pagination filtered after an intervening append, missing-conversation errors, and actual aggregate budget rejection for event pages and combined checkpoint/event snapshots. A smaller snapshot serialized to 14,541,075 bytes and succeeded while the larger page was rejected. Checkpoint inspection additionally verified version-one migration, a checkpoint larger than the public event limit, state revision conflicts, unchanged newer state after old command replay, None/Some command conflicts, the 8 MiB state bound, atomic rollback, and reopen persistence. Database files are local plaintext and require appropriate filesystem permissions from host setup; at-rest encryption is not implemented. Event append and individual journal transitions are durable transactions, but there is no cross-module transaction spanning a model request or external tool side effect.

## Schema 4: paired-device transport

Migration `0004_device_transport.sql` extends the existing connection with immutable local endpoint configuration, used pairing IDs, durable replay/inbox rows, transport ciphertext expiry metadata, original remote command journal, immutable replica event rows, replica cursors, persisted sync offers and host device cursors. The replay marker and plaintext inbox are the same atomically inserted row; relay ACK requires that insertion or a verified exact duplicate. Native commands and provider checkpoint state remain separate from replica events.

`src/storage/transport.rs` owns these Store operations without exposing a raw SQL connection. Receipt and application transactions recheck the full pinned public identity/access metadata and sticky revocation. Native host command reservation preserves the full original command JSON and digest; completion plus response ciphertext is one transaction. Completed duplicate command replies and inbox completion are also atomic. Replica event application, cursor update, outgoing encrypted application ACK/continuation request and inbox completion share one transaction. A host device cursor advances only against a matching persisted application ACK offer.

Transport read pages retain the existing row/aggregate byte ceilings. Pending inbox and outbox have additional global/per-peer count and byte quotas. Pending accepted work is retained after network expiry; old terminal payloads may be archived while command identity digests remain. Expiry/archival returns a report, and quota errors fail explicitly. See [device synchronization](device-sync.md) and [device architecture](../architecture/devices.md) for exact APIs, bounds, retention and caller responsibilities. Plaintext local inbox/replica state requires the service's private state directory; relay E2EE does not provide local disk encryption.

## Validated database handle lifetime

On Unix, `Agent::open` passes the real validated owner-only, no-follow, single-link database descriptor into `Store::open_validated`. Every cloned Store retains that descriptor. Store fields close the SQLite connection before releasing the final descriptor; this ordering is an invariant because closing another descriptor for the same inode can release POSIX record locks held by SQLite. The private state-directory and database validation rules are unchanged. Issue [#81](https://github.com/izyuumi/everything/issues/81) records the original live-WAL unlink defect separately from corrected historical observations.

The validated Store/Agent descriptor ownership from [PR #84](https://github.com/izyuumi/everything/pull/84) and the current Host startup guards remain in this source union. The Host passes its actual validated descriptor to the same constructor; Worker file validation uses metadata rather than opening another descriptor for that live inode. These current source/format checks do not establish this Worker union's live-lock, competing-Store, fresh-reader or restart behavior. Fresh exact-head CI and disposable runtime validation remain pending; historical observations retain their original source qualification. Apple behavior and automated assertions remain separately qualified; automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

## Remote delegation lookup

Schema 12 adds the nonunique `tasks_remote_delegation` expression index after schema11. The locator restricts native scheduler version1 and object remote definitions, returns at most two bounded task IDs, and preserves ambiguity errors and complete stored-task/binding validation. SQLite maintains it for writes from every actual Store handle; it is no approval or cached authority. See [the lookup contract](remote-binding-lookup.md) for migration and validation scope.

## Scoped Settings grant decisions

Schema 15 adds immutable canonical public grant versions and receipts with exact current heads on the same Store connection. One existing-connection Immediate transaction validates the actual Host/full peer pin, exact original replay, expected head and bounded history; the insertion installs its head atomically. Every Active head reserves logical room for a future Revoked version/receipt. No original is evicted and no new grant record grants remote authority by itself. Authenticated local-owner consumers retain the real Host/Agent/lease and mutation guard through the actual blocking worker. See [grant storage and owner routes](settings-grants.md) for the closed DTOs, replay/rotation rules, logical resource limits and outstanding schema/runtime validation. Native Settings schema 13–14 records and ordinary data remain unchanged.

## Metadata-only transport rejection

The crate-private rejection path shares ordinary inbox validation and its immediate transaction. A fresh refused packet inserts `transport_inbox.state=rejected` directly with `payload=NULL`, `command_id=NULL` and a fixed detail; schema15 already permits that terminal shape. It consumes one of the existing100000 replay markers and no pending-payload capacity. Complete pin/local-recipient checks, exact ciphertext replay and duplicate speculative-prune rollback are preserved. A duplicate never rewrites an older pending row or claims historical erasure. See [private packet rejection](private-settings-ingress.md); actual encrypted/storage observations remain separate from source compilation.

## Connection health observations

The crate-private `Store::check_connection_health` reads the existing shared quarantine flag with Acquire ordering and the existing Main mutex poison state, returning the actual Quarantined or Poisoned error. It acquires no mutex, executes no SQL and does not clear either condition. Existing connection admission calls it before and after acquiring that same mutex; the lock result still independently preserves poisoning errors. All Store clones share the original connection and one-way quarantine flag.

The narrow read permits the assigned queued-reply lifetime validator to reject an already unhealthy Store after worker waits without taking Main again. It observes health at that instant; it does not attest a transaction's origin, current SQL pins/grants/history, a true worker join, cleanup, recovery or future health. Those actual ownership/current-state gates remain required. No reset or replacement connection API is added. Source/static validation does not establish fault-injection or runtime acceptance; automated verification remains deferred to #32.
