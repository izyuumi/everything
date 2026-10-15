# Native models

`src/model/types.rs` defines provider-neutral messages, structured tool calls/results, request validation, capabilities, streaming events, token usage, stop reasons and bounded opaque continuation. `ModelProvider` returns an owned sendable stream and receives a cancellation token. A stream consumer must require a completed response before treating inference as successful.

The native [Anthropic Messages adapter](anthropic.md) is integrated. The [OpenAI Responses adapter](https://github.com/izyuumi/everything/pull/41) is in focused integration; its documentation is committed on that branch. Both use native APIs rather than a mandatory model gateway. Adapter runtime limits, native ordering and continuation details belong to their documents and the [provider contract](../contracts/providers.md).

Provider configuration owns sensitive credentials in memory; credentials are not part of model requests or durable conversation data. Official HTTPS endpoints are defaults. Custom local endpoints require explicit trusted configuration. HTTP errors are sanitized, redirects disabled, and stream/body resource limits enforced. Request preparation uses blocking workers so large JSON/history operations do not occupy async runtime workers.

Continuation is opaque provider state, potentially containing signed thinking or encrypted reasoning. History rewrites must respect provider replay requirements; the context builder preserves required state during tool-result resumption or returns an explicit error. Private conversation checkpoints are being added for atomic persistence with public messages; consult [status](../status/implementation.md) for the current integration gate.

No live inference has been verified in this environment. Automated HTTP mapping and cancellation coverage is deferred to #32.
