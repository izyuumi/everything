# Architecture overview

Everything contains one native Rust agent kernel, and one owner-operated `everything` CLI whose optional `relay` subcommand runs the ciphertext relay server and toggles the host relay connection. The CLI initializes and reopens durable conversations, calls native Anthropic Messages or OpenAI Responses adapters, and runs explicit tool rounds through the central broker. The graphical applications do not own runtime state. Background task orchestration and native clients remain under integration; consult [implementation status](../status/implementation.md) for local integration, main-branch and validation boundaries.

## Execution and authority

The [agent kernel](../contracts/agent.md) captures immutable provider configuration and accepts text durably before inference. Native mode saves context inputs, conservative budgets and model-attempt identity before each request. Completed responses supply authoritative tool calls; streamed partial arguments never dispatch. Private native continuation and phase checkpoints remain local and separate from replicated public events. Unknown provider outcomes require explicit recovery; unknown tool effects require broker reconciliation.

One host must own the private state directory lock, shared Agent, registry, Authorizer and [Tool Broker](broker.md). The broker journals stable operations and applies permission checks immediately before execution. Its supervised workers retain admission and concurrency gates through cleanup and durable outcomes. Native filesystem tools use scoped capabilities; shell and MCP require FullHost authority and exact operation approval. Arbitrary shell is not an application-path sandbox. Offline discovery selects only permission-visible schemas and remains advisory.

The CLI currently composes native filesystem/shell tools and offline discovery. The library also supplies explicit native MCP client registration. Background service and IPC libraries exist, but the actual background command processor and concurrent-task scheduler are separate pending integrations. No GUI process is required for the current CLI or relay.

## Storage, context and memory

The host Store uses SQLite WAL, explicit migrations and one mutex-protected connection. Public events, private conversation state, task rows, permission records, operation journals and device transport records have separate invariants. Synchronous storage and filesystem work runs through bounded blocking boundaries when composed with asynchronous execution.

ContextBuilder constructs bounded requests from immutable task inputs, conversation history, provenance-bearing memory and selected native tools. Compaction preserves the original durable history and respects provider continuation constraints. The canonical memory source is a Markdown wiki. A separate rebuildable SQLite FTS5 index contains metadata, links and heading chunks. Wiki consolidation has one serialized writer and a durable prepared-write journal; interrupted writes reconcile their before/after hashes instead of regenerating or overwriting conflicting external edits.

Local EmbeddingGemma 2 is optional and disabled by default. Its generic embedding contract binds model artifacts, preprocessing and dimensions to a profile. Native ONNX inference supports 128, 256, 512 and 768 dimensions, explicit trusted runtime/model files and cancellation. Hybrid vector indexing is a separate workstream; enabling native inference alone does not imply a populated semantic wiki index.

## Transport and lifecycle

Device endpoints pair pinned identities, enforce explicit conversation access, and journal authenticated commands before acknowledgment. Encrypted outboxes and replicated event cursors survive reconnects. The optional relay authenticates enrolled devices and stores bounded ciphertext for offline delivery; it has no conversation plaintext. Relay delivery acknowledgment and application synchronization acknowledgment are distinct. The host still needs its background dispatcher to apply queued remote commands to the native runtime.

All network providers, relay infrastructure, future Apple clients and delegated workers remain optional composition boundaries. The local host retains authority. No backup subsystem or compatibility layer for another harness is present. Builds and disposable manual observations do not establish live-provider interoperability, Apple-client behavior, security review or production readiness; deferred automated verification is tracked in [#32](https://github.com/izyuumi/everything/issues/32).
