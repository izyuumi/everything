# Tools contracts

## Implementations and descriptors

`Tool: Send + Sync` exposes `descriptor(&self) -> &ToolDescriptor` and asynchronous `execute(&self, context: &ToolExecutionContext, arguments: serde_json::Value) -> Result<ToolOutput, ToolError>`. Implementations use `async_trait`. `ToolOutput.content` is a JSON value owned by the result.

`ToolDescriptor` contains `name`, `description`, `input_schema`, `capabilities`, `read_only`, and `concurrency`. Names contain 1–128 ASCII letters, digits, underscores, dots, or hyphens; descriptions contain 1–8192 bytes and cannot be whitespace-only. Capability values are `filesystem_read`, `filesystem_write`, `shell`, `network`, and `mcp`. Concurrency is `concurrent` or `exclusive`. These values declare execution requirements for the future broker; they are not permission grants or enforcement.

## Registry

`ToolRegistry::new()` creates an empty registry. `register(Arc<dyn Tool>)` synchronously compiles and validates the descriptor's JSON Schema before adding the tool. Call registration during startup or on a blocking worker. Registration snapshots the descriptor and cannot replace an existing tool. Schemas may use locally embedded references; external resource retrieval is disabled. Supported JSON Schema drafts follow the `jsonschema` library's supported drafts and schema auto-detection. Format assertions are enabled. Schemas with regular expressions that exceed configured resource limits fail registration or argument validation.

`discover()` returns cloned descriptors in ascending name order. `descriptor(name)` returns a borrowed descriptor or `UnknownTool`. `validate(name, &arguments)` performs synchronous validation; callers must move potentially expensive calls off async executor threads. `execute(name, &context, arguments)` performs validation on a Tokio blocking worker and requires a Tokio runtime. Registry execution does not acquire permission, persist operations, or serialize exclusive tools; the broker must provide those boundaries before it exposes execution to models.

Serialized schemas are limited to `MAX_TOOL_SCHEMA_BYTES` (262144 bytes) and arguments to `MAX_TOOL_ARGUMENT_BYTES` (1048576 bytes). Invalid argument results contain up to 32 `ArgumentViolation` values with JSON Pointer instance and schema paths and a diagnostic message truncated to 2048 characters. Schema-registration diagnostics have the same character limit. Diagnostics can contain submitted values and must receive the same privacy treatment as tool arguments; the general invalid-argument error display includes only the tool name. Schema validation does not mutate arguments or insert defaults.

## Trusted context and cancellation

`ToolExecutionContext::new(request_id, task_id, workspace_root, cancellation)` accepts host-supplied identifiers and a `tokio_util::sync::CancellationToken`. Identifiers contain 1–256 bytes, cannot be whitespace-only, and cannot contain control characters. The workspace root must be absolute. Private context fields are exposed through read-only accessors. The context has no Serde deserializer and must never be constructed by decoding model arguments.

`check_cancelled()` returns the typed `Cancelled` error. The registry checks cancellation before validation and before invoking the implementation. Once execution starts, implementations must observe `cancellation()`, stop their work, and release resources. The registry does not drop an executing future automatically, because doing so would not guarantee cleanup. An absolute workspace path is metadata, not a security boundary; arbitrary shell confinement requires operating-system enforcement or explicit trusted execution policy.

## Errors and deferred verification

`ToolError` distinguishes `UnknownTool`, `DuplicateTool`, `InvalidDescriptor`, `InvalidSchema`, `InvalidArguments`, `ResourceLimit`, `InvalidContext`, `Cancelled`, `Execution`, and `ValidationWorker`. Validation and lookup errors occur before tool invocation. Implementations use `Execution` for explicit tool failures and propagate cancellation instead of presenting it as successful output.

This issue does not include privileged implementations, permission policy, durable tracking, or scheduling. Automated registration, discovery, invalid-argument, and unknown-tool tests are deferred to [#32](https://github.com/izyuumi/everything/issues/32) under the current no-new-tests instruction.
