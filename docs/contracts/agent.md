# Agent kernel contract

`Agent` shares its Store, host lifetime guard, acceptance gate, and per-conversation execution gates across clones. A trusted host constructs one Agent and one global ToolBroker. `Agent::open` owns the Unix foreground bootstrap; `Agent::from_store` requires the caller to own the state lock. `Agent::store()` returns the same shared Store for trusted composition, not an independent runtime lease.

## Commands and admission

`create_conversation(id,command_id,title,config)` establishes immutable ConversationConfig. `accept_text(TextCommand)` persists the exact conversation/command/text identity and returns Pending or the original completed response. It never performs inference. `conversation(id)` returns a coherent captured history; `conversations(limit)` returns bounded safe metadata.

`run_pending(conversation,command,provider,cancellation)` retains the legacy text path. `run_tools(conversation,command,provider,ToolRunSetup,cancellation)` runs the same native kernel with context and tools. Both return an owned `TurnStream` and reject concurrent execution in the same conversation across Agent clones. Distinct conversations may run concurrently. The stream owns admission and a child cancellation token. A stream must be polled or dropped; dropping cancels its pending work. Host teardown must still respect the broker's supervised cleanup.

The injected provider's ID must match the immutable conversation provider. The trusted host must additionally construct it from the exact saved model endpoint and credential policy. The trait does not expose a trustworthy endpoint attestation; provider ID equality alone is insufficient.

## Native setup

ToolRunSetup contains a scoped TaskToolBroker, the actual shared ToolRegistry, TaskContext, ContextBudget, memory excerpts with provenance, initial selected ToolDefinitions, LoopLimits, and ProviderResume policy. The referenced task must already be Running in the same conversation. Its broker authority supplies the canonical workspace, execution mode, task identity, and inherited allowlist. The caller must not create separate brokers per task.

The native checkpoint saves the definition and its digest. An active resume must preserve that definition, authority, task ID, registered definitions, and limits. `ToolRunSetup::resume(scoped_broker,registry,&conversation,policy)` reconstructs the saved context inputs; it still validates the current registry and authority before execution. `Conversation::native_run()` returns active or last completed NativeRunInfo, including command/task IDs, workspace, mode, allowed tool names, status, attempt/call counts, conservative charged tokens, reported usage, and deadline. Match its command ID before using prior-run metadata for a new command.

Saved catalog and authority checks bind an active turn. A later accepted command starts with its newly selected permitted tools; it need not keep historical tools advertised. The raw historical messages and provider continuation are preserved.

A legacy conversation with provider continuation cannot enter tool/context mode. Native history and opaque state must preserve their exact structure; a caller cannot reset them to overcome a pending tool round. Compaction is delegated to ContextBuilder, preserving raw history and required continuation rules.

## States and effects

LoopStatus is Ready, ProviderInFlight, ExecutingTools, AwaitingPermission, Paused, or Completed. Before a model request the kernel persists ProviderInFlight and its attempt UUID. OnlyUnstarted is the default resume policy: an uncertain attempt remains stopped. RetryUncertain explicitly authorizes another potentially paid request or continuation after a known interrupted round; it never authorizes re-execution of an unknown broker effect.

Completed provider calls commit stable internal UUIDs and exact provider call IDs before dispatch. Each internal call UUID also identifies its broker operation. Pending/Dispatched/AwaitingPermission slots can submit or replay that exact operation. An InFlight pause can be polled again without changing identity. ReconciliationRequired, broker rejection, invalid model output, and exhausted budgets stay stopped; an owner must address the reported condition rather than silently widening permissions or limits.

Known results commit per call, while the model-facing Tool message commits only after every slot has a result, in original call order. Unknown outcomes never become ordinary tool results. Failed output and explicit truncation are preserved with is_error semantics. Saved expanded tool definitions are checked against the current registry before pending calls execute.

TurnEvent reports provisional text, durable assistant messages, context usage/citations, durable model-attempt identity, saved calls/results, exact approval requests, blocked-call diagnostics, paused reasons, and final NativeRunInfo. A Committed assistant message can contain calls and is not itself a completed native task. RunCompleted is the saved final-response signal. An error or end of a paused stream does not mean a provider request or external effect never occurred.

Cancellation propagates through the model and every dispatched broker future. The kernel drains known broker cleanup boundaries; requires_stop and requires_reconciliation are decisive. The host should cancel, keep polling until completion, and retain its global broker and state lock while supervised cleanup remains. Public events and private phase checkpoints use strict sequence/revision CAS. A failed durable append prevents subsequent dispatch or model calls.

Each native tool invocation automatically retains an Agent clone through its broker supervisor, blocking journal work, registry validation, and builtin filesystem worker. Dropping a TurnStream or returning an uncertain foreground result cannot release an Agent-owned state lock while those actual workers remain active. This retention does not hold conversation admission beyond the stream, prove that arbitrary external work stopped, or change unknown-effect handling. Raw broker and startup callers remain responsible for their own lifetime resources; see the [broker lifecycle contract](broker.md#lifecycle-and-shared-limits).

Native preparation, task checks, checkpoint writes, and foreground task helpers explicitly move the complete Agent into their actual SQL worker. Accessing only its Store would allow separate state-lock fields to be dropped when the awaiting future ends. Foreground tool setup likewise retains its Agent through the blocking registry and approval preparation and uses the owner-preserving broker startup path. These lifetime bindings do not change admission, state transitions, or journal semantics.

## Foreground task helpers

`prepare_foreground_task(conversation,command)` derives a typed TaskId from the accepted CommandId, uses its durable user-message timestamp and text, verifies exact existing TaskRecord identity, and admits its Queued/Waiting row to Running. `settle_foreground_task(conversation,command)` verifies the original accepted user identity and records Completed only when its matching durable final assistant response exists and the command is no longer pending; otherwise it leaves a resumable Waiting row. `reconcile_foreground_task` skips absent legacy tasks and repairs completion/settlement crash gaps on exact completed replay, including older native runs and a prepared task later completed through legacy text resume. These helpers support one owner-operated foreground command and do not replace the background scheduler or its wider command/task event orchestration.

The CLI's --tools path registers native builtins and local search_tools once, starts one broker, binds discovery to the saved authority for the full stream lifetime, and reconstructs saved setup on resume. It prints the complete exact approval request before accepting a separate --resume --approve/--deny action. FullHost is explicit; arbitrary shell has full host authority and no application-level path sandbox.

## Bounds and validation

Public events are at most 256 KiB and private checkpoints at most 8 MiB. The kernel validates model attempts, calls, concurrency, tokens, duration, schema definitions, memory, and canonical result bounds before further work. Tool result truncation is an explicit JSON value; raw argument documents are never truncated. See [runtime architecture](../architecture/runtime.md) for concrete limits and [broker contract](broker.md) for effect/recovery semantics.

No automated tests were added. Deferred stream, replay, context, concurrency, permission, and crash-recovery coverage remains in [#32](https://github.com/izyuumi/everything/issues/32). Live native inference requires authorized credentials and is not verified by static analysis or local protocol inspection.

A disposable Rust 1.88 release executable inspected the actual Agent and Store with copied local records and a seeded previous final phase. A fresh command with an empty current registry and tool authority reached the native OpenAI adapter's refused loopback connection and durably recorded one attempt. An active saved tool round whose definition had been removed was rejected before stream creation, with an unchanged checkpoint and no broker dispatch or provider request. This observes tool-catalog admission and preserves the distinction between a fresh command and an active resume; no successful inference was simulated or verified.
