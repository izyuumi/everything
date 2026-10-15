# Documentation index

Read [AGENTS.md](../AGENTS.md), then [implementation status](status/implementation.md).

## Architecture

[Overview](architecture/overview.md) · [Runtime](architecture/runtime.md) · [Agents](architecture/agents.md) · [Scheduling](architecture/scheduling.md) · [Models](architecture/models.md) · [Tools](architecture/tools.md) · [MCP](architecture/mcp.md) · [Built-ins](architecture/builtin-tools.md) · [Discovery](architecture/selector.md) · [System One](architecture/system-one.md) · [Permissions](architecture/permissions.md) · [Broker](architecture/broker.md) · [Context](architecture/context.md) · [Memory](architecture/memory.md) · [Wiki maintenance](architecture/consolidation.md) · [Hybrid retrieval](architecture/hybrid.md) · [EmbeddingGemma 2](architecture/embeddinggemma2.md) · [Transport](architecture/transport.md) · [Devices](architecture/devices.md) · [Relay](architecture/relay.md) · [Service](architecture/service.md) · [Headless host](architecture/host.md) · [Host settings](architecture/host-settings.md) · [Security](architecture/security.md)

## Contracts

[Events](contracts/events.md) · [Agent kernel](contracts/agent.md) · [Scheduler](contracts/scheduler.md) · [Device sync](contracts/device-sync.md) · [Providers](contracts/providers.md) · [Tools](contracts/tools.md) · [MCP](contracts/mcp.md) · [Built-ins](contracts/builtin-tools.md) · [Discovery](contracts/selector.md) · [System One](contracts/system-one.md) · [Native credentials](contracts/credentials.md) · [Broker](contracts/broker.md) · [Storage](contracts/storage.md) · [Schedule storage](contracts/scheduling-storage.md) · [Schedule engine and calendar](contracts/scheduling-engine.md) · [Host inbox](contracts/host-inbox.md) · [Owner IPC](contracts/owner-ipc.md) · [Wiki maintenance](contracts/consolidation.md) · [Hybrid retrieval](contracts/hybrid.md) · [Embeddings](contracts/embeddings.md) · [Local IPC](contracts/local-ipc.md) · [Context](contracts/context.md)

## Development

[Setup](development/setup.md) · [CLI](development/cli.md) · [Headless host](development/host.md) · [System One setup](development/system-one.md) · [Conventions](development/conventions.md) · [Workflows](development/workflows.md) · [Backlog synchronization](development/backlog-sync.md) · [Validation](development/validation.md) · [Embedding benchmark](development/embedding-benchmark.md) · [Scheduler](development/scheduler.md)

## Decisions

[ADR-0001: Host authority and local-first boundaries](decisions/ADR-0001-host-authority.md) · [ADR-0002: Local EmbeddingGemma 2 ONNX](decisions/ADR-0002-local-embeddinggemma2-onnx.md) · [ADR-0003: Native MCP SDK](decisions/ADR-0003-native-mcp-sdk.md) · [ADR-0004: Scoped SQLite vector indexing](decisions/ADR-0004-scoped-sqlite-vector-index.md) · [ADR-0005: Scheduling calendar and admission](decisions/ADR-0005-scheduling-calendar-and-admission.md)

## Worker delegation

[Architecture](architecture/cloud-worker.md) · [Worker protocol](contracts/worker.md) · [Staging](contracts/worker-staging.md) · [Executor](contracts/worker-executor.md) · [Terminal sources](contracts/worker-terminal-sources.md) · [Bridge](contracts/worker-bridge.md) · [Revocation](contracts/worker-revocation.md)

[Native reservations](contracts/remote-tasks.md) · [Binding lookup](contracts/remote-binding-lookup.md) · [Attributed reports](contracts/remote-results.md) · [Atomic external storage](contracts/external-task-storage.md) · [Native result admission](contracts/remote-result-admission.md)

[Host data](contracts/host-worker.md) · [Host service](contracts/host-worker-service.md) · [Host admission](contracts/host-worker-admission.md) · [Validation](development/worker.md) · [Files](development/worker-files.md) · [Coordinator CLI](development/worker-coordinator-cli.md) · [ADR-0008: Scoped worker authority](decisions/ADR-0008-scoped-worker-authority.md)
