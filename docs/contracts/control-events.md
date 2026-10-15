# Durable task and permission publications

Issue #73 adds trusted Agent publication methods for conversation metadata. These reuse the existing encrypted conversation event synchronization. They add no runtime, transport, permission grant, database migration or model-visible message.

## Current-state convergence

`Agent::publish_task_state(task_id)` rereads the authoritative task under the conversation's short write gate. Scheduler tasks use `scheduler::task_publication_records`, which returns the original definition's initial TaskRecord and its current checkpoint record from one validated repository load. Legacy foreground tasks validate their original TaskRecord, current checkpoint and storage identity. The original conversation, task, parent, originating command, instruction and creation time remain unchanged.

The first publication appends TaskCreated containing that original queued record. A differing current status or failure appends TaskStatusChanged. Repeated observation of the same published state appends nothing. Intermediate transitions can occur between observations and are not reconstructed. This is convergence to observed durable state, not a complete transition history. A task change racing publication is repaired by a later reconciliation pass; publication never changes the task itself.

TaskCreated uses a deterministic command identity bound to the task and conversation. Each status command derives from the preceding task publication's event ID and the exact new status/failure payload. The chain distinguishes a return to an earlier state even if timestamps are equal. Event identities derive separately from command identities using established SHA-256 and UUID version-eight formatting. Exact retained journals are authoritative across restart; changed original task identity conflicts.

## Original permission decisions

`Authorizer::decision_publication_ids(after, limit)` lists stored resolution identifiers in exclusive ascending key order. `decision_publication(id)` uses the existing authoritative request and resolution decoders to produce PermissionDecisionPublication. Its private fields have no deserializer or public constructor. It retains the original operation, conversation, resolution time and Allow/Deny choice, with a digest of the complete immutable request and resolution.

`Agent::publish_permission_decision` rereads that proof from its own Store before publishing or replaying. A proof minted by another Store cannot substitute a different decision or scope. The command identity is stable per original operation, while the event identity also binds the complete immutable source digest. A changed scope, resolution, payload or original time conflicts with its retained journal. Expiration or revocation does not manufacture a new Deny decision. A historical Allow publication describes the original resolution and grants no current authority. The Authorizer remains the authority for current eligibility, expiry and revocation.

A decision can be published before the native kernel commits its PermissionRequested event: the Authorizer request is already durable and owner-inspectable while a broker result is being saved. Replica projections must retain decisions independently of request arrival, or consult their already-stored decision events when later creating a request row. A repeated or late request must never reset a decided operation to pending. Conversation sequence orders publication, not the original creation of every underlying policy record.

## Checkpoint preservation and concurrent native writes

Each publication appends exactly one bounded event with the unchanged private conversation checkpoint in the same sequence/revision-CAS transaction. Original command, native phase, definition, counters, deadline, opaque continuation, tool mappings, input queue, compaction and message history are retained. Task creation and a differing current state use separate single-event transactions; a crash between them is repaired by repetition.

The native merge permits at most 256 intervening events. Its previous strict equality checks for all non-input checkpoint state and all model messages remain in force. It reconstructs accepted input prefixes in event order. Every intervening event must match a single-event command journal, exact envelope/row metadata and the stored state-update digest for that precise expected revision and reconstructed checkpoint. Metadata additionally requires deterministic identities for TaskCreated, TaskStatusChanged or PermissionDecided. An append without private CAS, a message insertion, an unrelated event, changed native state, missing sequence, changed digest or unexpected revision fails closed. Only these three metadata variants and the typed task-input acceptance events are admitted by this merge.

The concurrent merge applies to the native kernel. Direct legacy text-only library callers must publish fresh metadata only while their execution is quiescent; the headless host owns one state lease and uses the native scheduler.

Publications never enter context construction or consume model/tool budgets. They confer no uncertain retry, cleanup or execution authority. Existing encrypted device access, pin, cursor and revocation checks continue to govern delivery.

## Bounds and composition

Task and resolution source pages contain at most 64 identifiers and use indexed exclusive ID scans. Task input/checkpoint sizes are checked before loading source JSON. Latest-publication reads use the existing conversation/sequence index and reject payloads above the public 256-KiB event limit before allocation. The existing bounded conversation history and private-checkpoint limits still apply; an oversized legacy task fails explicitly rather than being truncated.

Unchanged task and permission observations verify exact retained journals without loading the complete conversation history. The private checkpoint and bounded message history are loaded only when an event must append. Source pages seek directly in their ID indexes rather than scanning an already returned prefix.

The caller must advance through every source page, handle individual source failures without starving later identifiers, and restart a completed pass from None to discover late lower identifiers and revisit changed tasks. Reconciliation notifications are hints; restart must rescan durable sources. Host composition must supply bounded passes, wake ownership, backoff and diagnostics. These leaf methods do not create a second scheduler or an autonomous retry service.

## Observed validation

On 2026-10-15, the original leaf integration passed Rust 1.88 formatting, default and all-feature workspace all-target checks, strict Clippy in both configurations, and the all-feature workspace release build. The following observations describe that integration; static and runtime validation of this focused extraction remains pending.

Disposable external runtime walkthroughs exercised the actual Agent, Authorizer, Scheduler, Broker and Rust DeviceEndpoint implementations. A native stream captured before concurrent task metadata, real Allow/Deny resolutions and steering preserved its private definition/deadline, consumed the steering and made one local HTTP request that returned 503. Its saved uncertain attempt remained unchanged by later metadata and ordinary resume made no additional request. A legitimate different-Store permission proof was rejected. Real primary/child scheduler task creation, running status and cascading cancellation converged through repeated publication. A separately labeled seeded TaskRow transition sequence at one timestamp produced distinct chained status identities.

Actual Rust pairing and encrypted event synchronization delivered task publications and permission decisions with cursor/application-acknowledgement convergence. An unauthorized conversation and a revoked sender were rejected. Reopening the host retained the same sequences and private checkpoint without duplicate publications. This direct envelope transfer did not exercise a relay network or an Apple implementation.

Separate disposable rejection checks covered well-formed metadata with untrusted identities, a public-only append without private CAS, a changed native counter and an explicitly corrupted command-journal digest. Each failed before a model request. Publishing 257 actual stored Deny resolutions triggered the concurrent-event bound. A temporary SQLite trigger forcing the private update to abort left the public event head and private checkpoint unchanged; removing that trigger allowed one successful publication and exact deduplication. One-row keyset scans reached all six runtime task identifiers and all 257 decision identifiers without duplicates; oversized page requests were rejected.

Full headless-host notification, diagnostic/backoff and shutdown composition requires its own integration walkthrough. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32). No successful model inference, Apple interoperability or complete transient lifecycle history is implied.
