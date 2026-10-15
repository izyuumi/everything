# Models

The model foundation provides one provider-independent request, response, and event vocabulary in `src/model`. Its `ModelProvider` interface supports native adapters through an object-safe asynchronous boundary. No provider adapter, gateway, model request, or agent loop is implemented by this module.

The runtime constructs validated `ModelRequest` values from conversation history and context. A concrete adapter translates them into its native HTTP protocol and returns a cancellation-aware `ProviderStream`. The runtime consumes text deltas for presentation, waits for parsed tool-call completion before considering execution, and persists the final response and provider-owned continuation. The tool broker remains responsible for tool permissions and execution; model output alone provides no authority.

`types.rs` owns message roles, text/tool content, tool definitions and selection, request limits, provider capabilities, usage and stop reasons, structured errors, completed responses, and streaming events. `mod.rs` owns only the trait, stream alias, and exports. These modules have no storage, tool registry, UI, or protocol-event dependency. The runtime is the explicit conversion boundary between durable protocol messages and provider messages.

Opaque continuation is bounded JSON tagged with its owning provider. It can preserve provider-specific capabilities such as response cursors or signed reasoning without adding provider-specific fields throughout the runtime. Debug output hides the payload; persisted state still requires the same protection as conversation content. Explicit local history remains authoritative.

The [provider contract](../contracts/providers.md) defines validation, stream ordering, cancellation, usage semantics, and adapter responsibilities. Concrete OpenAI Responses and Anthropic Messages adapters are separate dependent issues. Automated verification is deferred to [#32](https://github.com/izyuumi/everything/issues/32); this foundation does not claim a live provider roundtrip.
