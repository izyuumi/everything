# Tools

The `tools` module defines the native asynchronous tool boundary. `ToolRegistry` owns tool implementations, immutable descriptor snapshots, and compiled JSON Schema validators. Registration rejects duplicate names, malformed descriptors, invalid schemas, schemas requiring external resources, and oversized schemas. Discovery returns descriptors in ascending tool-name order.

For execution, the registry resolves the tool, checks cancellation, validates arguments on a Tokio blocking worker, checks cancellation again, and invokes the tool. Invalid arguments never reach the implementation. Validation accepts at most 1 MiB of serialized arguments and reports at most 32 violations. Schemas are limited to 256 KiB; regular-expression compilation and backtracking also have limits.

The host constructs `ToolExecutionContext` with trusted request and task identifiers, an absolute workspace root, and a cancellation token. The context is separate from model-controlled JSON arguments and cannot be deserialized. Its workspace root describes the execution scope; it provides no filesystem confinement or shell sandbox. Tool implementations must cooperate with cancellation and clean up resources before returning.

Descriptors declare required capabilities, read-only behavior, and concurrency safety. These declarations describe implementations; the registry does not grant permissions or enforce concurrency policy. The Tool Broker and permission layer remain separate implementation work. No shell, filesystem, MCP, or other privileged tool is supplied by this foundation.

See [tools contracts](../contracts/tools.md) for the public API and [implementation status](../status/implementation.md) for integration gates. Automated tests required by issue #5 are deferred to [#32](https://github.com/izyuumi/everything/issues/32); static validation does not verify those acceptance criteria.
