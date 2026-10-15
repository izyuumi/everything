# Runtime

The current runtime supports persistent, single-agent text conversations through `init`, `chat`, and `status`. Each `chat` invocation accepts or explicitly resumes one turn. Repeated invocations continue the same conversation. Native Anthropic Messages and OpenAI Responses adapters perform inference directly; no external gateway is required. The CLI requires an explicit provider and model when creating a conversation. See [CLI usage](../development/cli.md).

## Ownership and boundaries

`src/agent.rs` owns conversation acceptance, provider-history construction, streaming, and atomic durable completion. `src/main.rs` owns argument parsing, environment credentials, terminal output, and process signals. Provider adapters own native HTTP protocols. SQLite owns append-only public events and a separately versioned private checkpoint. The reusable `Agent::from_store` constructor accepts an already opened store; its host must hold the application's state lock. `Agent::open` bootstraps the foreground Unix host using the service module's descriptor-based private directory validation and exclusive `host.lock`. It rejects a second foreground or background owner instead of sharing runtime ownership.

SQLite opening, migrations, directory validation, lock acquisition, history loading, request construction, serialization, and durable appends run on blocking workers. Async network streams remain on Tokio. Linux and macOS are the initial foreground platforms; other platforms return an explicit unsupported-platform error.

`Agent` admits one active provider turn at a time across its clones. The admission guard remains attached to the returned stream. This prevents concurrent duplicate inference inside that runtime, including simultaneous attempts to resume the same command. The host lock prevents competing foreground processes. Acceptance has a separate short gate so concurrent identical commands return the same durable acceptance. Blocking storage work retains host ownership and its relevant admission guard even if the waiting async future is dropped. Independent `Agent::from_store` instances require a coordinating host and must not be used to bypass admission.

## Durable acceptance and replay

`TextCommand` contains the complete foreground command identity: conversation ID, command ID, and exact text. It does not reconstruct the device identity, timestamp, or other fields of an external `CommandEnvelope`. The later host command processor is responsible for that wider boundary.

A conversation starts with a typed `ConversationCreated` event and immutable provider, model, instructions, endpoint, and sampling configuration in its private checkpoint. The SQLite conversation metadata row is created first; an interrupted initialization can be retried with the same conversation identity, title, and configuration. An incomplete row is reported explicitly rather than treated as an initialized conversation.

A new text command atomically appends one typed user `MessageAppended` event and updates the private checkpoint with its pending command and internal message ID. The command journal prevents the same command ID from accepting different conversation IDs or text. Replaying an accepted pending command returns its existing acceptance without calling a provider. Replaying a completed command returns its saved assistant message. Explicit resume is required to retry a pending remote operation.

Completion atomically appends one assistant `MessageAppended` event and replaces the checkpoint using both conversation sequence and checkpoint revision comparisons. Public events carry the original user command as their causation ID. Completion uses a separate internal journal command ID. Provider opaque continuation data, response ID, token usage, and the latest stop reason remain private checkpoint data; no opaque native payload enters public synchronization events.

## Streaming, cancellation, and recovery

Text deltas are provisional terminal output. Only a validated provider `Completed` event can produce a durable assistant message. Tool calls are rejected by this text-only path. A disconnect, malformed stream, provider failure, request limit failure, or cancellation leaves the previously accepted user message and pending checkpoint intact. A partial assistant reply is never appended as a completed message.

Dropping the turn stream releases runtime admission and cancels its child provider token. SIGINT and SIGTERM propagate cancellation through the CLI. Cancellation is checked before durable completion begins. Once the atomic storage operation starts, the runtime waits for its actual outcome: a commit may win a concurrent cancellation race. Inspect `status --history` before an explicit resume after interruption. A remote provider may already have processed an uncertain request; resuming can incur another request. The implementation does not promise exactly-once remote inference.

## History and continuation

A conversation read captures its head, checkpoint, and first event page in one SQLite read transaction. Later immutable event pages are bounded by that captured head, so newer appends cannot be combined with an older checkpoint. The runtime validates contiguous sequences, typed event metadata, checkpoint message count and head, and the exact pending user-message identity.

Protocol UUID message IDs remain local durable identities. The current text-only provider mapping copies roles and ordered text blocks into provider-neutral `Message` values. Tool identity mapping is intentionally deferred to the native tool loop. Provider continuations are persisted verbatim and returned with the same immutable conversation configuration and raw message prefix. This path never silently clears continuation data. Context compaction and tool execution are separate components and are not integrated into this initial text CLI.

## Resource bounds and limitations

- User text and completed assistant text each have a 128 KiB UTF-8 limit. JSON escaping can cause a smaller message to hit the public event bound.
- Each public event is at most 256 KiB, below the synchronization batch byte budget. Private checkpoints are at most 8 MiB.
- Loaded history is limited to 65,536 events and 64 MiB of serialized rows. Provider contracts impose their own request limits before a network call. This CLI sends full history and may require a new conversation when that limit is reached.
- Instructions are limited to 32 KiB; titles to 4,096 bytes. Message files must be bounded regular UTF-8 files. Reads do not wait on a FIFO.
- Loopback proxy selection is explicit and immutable per conversation. Only literal loopback HTTP(S) endpoints without credentials, query, or fragment are accepted. Omitting the flag on later commands retains the persisted endpoint.
- Credentials come only from the process environment and are excluded from events and checkpoints. The foreground CLI does not yet proxy commands to the background service; it reports the existing host lock owner as a conflict.

Automated coverage is deferred to [issue #32](https://github.com/izyuumi/everything/issues/32). Live provider inference requires separately available credentials and is not verified merely by local compilation or failure-path checks.

## Validation performed

Linux formatting, all-target compilation, Clippy with warnings denied, and workspace release compilation passed. Disposable CLI inspection verified offline initialization, exact initialization replay, changed-command rejection, reopen persistence, missing credentials, explicit resume after connection refusal, immutable provider configuration, bounded file/FIFO rejection, singleton lock rejection, owner-only directory/database modes, database symlink and hardlink rejection, and SQLite integrity. SIGINT during an actual request to a disposable local TCP listener returned 130, closed the transport, and preserved only the accepted pending user message. No real provider credentials were used or persisted. Successful live inference, authoritative assistant completion against a live provider, and native opaque continuation recovery across a successful remote turn remain unverified in this environment.
