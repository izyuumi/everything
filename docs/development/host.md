# Headless host and owner commands

Build the release binary with Rust 1.88 or newer. The host passed Rust 1.88 all-target/all-feature static checks and release build, plus focused disposable Linux serve/proxy/restart observations; consult [implementation status](../status/implementation.md) before relying on platform-specific behavior.

```sh
cargo build --release --bin everything --locked
target/release/everything --state-dir /absolute/private/state serve
```

`serve` owns the agent independently of clients. Closing an attached CLI does not cancel admitted work. The private absolute state path must fit the portable 100-byte Unix socket limit. Do not run a second host over that state directory.

While it serves, ordinary `init`, `chat`, and `status` commands proxy through owner IPC instead of opening another Agent or Tool Broker:

```sh
everything --state-dir /absolute/private/state init --provider openai --model gpt-4.1-mini
everything --state-dir /absolute/private/state status
everything --state-dir /absolute/private/state chat --conversation CONVERSATION_UUID --message 'Hello'
```

The CLI prints generated conversation and command identities before mutating requests. On an ambiguous disconnect, retry the original command ID and exact contents. Durable command acceptance means queued admission; task completion and committed assistant messages are separate. The proxy prints committed final text. It keeps waiting while the task waits for its own spawned children, because the host resumes it automatically when their results arrive; other waiting reasons still stop the proxy for owner inspection. The stop message names the task's actual waiting reason and diagnostic rather than the coarse `waiting_for_children` status. Provisional live deltas remain available only in the direct foreground path at this stage.

Without a host, the same foreground commands retain their existing behavior. A foreground-format task that predates the scheduler is not rewritten on host startup. Stop the host and explicitly resume or settle that legacy task through the foreground CLI before switching ownership.

## macOS service management

```sh
everything --state-dir /absolute/private/state service install
everything --state-dir /absolute/private/state service enable
everything --state-dir /absolute/private/state service start
everything --state-dir /absolute/private/state service status
everything --state-dir /absolute/private/state service stop
everything --state-dir /absolute/private/state service disable
```

These operations target the current user's launchd login domain, without root. Installation does not replace a running host. See [service architecture](../architecture/service.md). Linux returns a typed unsupported-platform error for these six commands before creating state. The standalone `serve` command and Unix IPC remain available there. Apple execution requires Apple validation; Linux observations do not establish login restart behavior.

## Owner inspection and configuration

`owner --request-file PATH` reads one bounded regular JSON file. The input selects a query, operation, or environment credential transfer. It never logs the input object. Query examples:

```json
{"kind":"query","query":{"kind":"settings"}}
```

```json
{"kind":"query","query":{"kind":"runtime"}}
```

Other query kinds include `conversations`, `conversation`, `tasks`, `task`, `approvals`, `approval`, `identity`, `devices`, `transport`, `command`, and `tool_recovery`. Lists require a limit from 1 to 100. Individual task metadata reports the actual waiting reason, diagnostic, cancellation state and active command. Private native checkpoints and credentials are not returned. Pending approval lists return small summaries with default `limit: 20` and an optional cursor. Fetch the exact arguments and scope separately before deciding:

```json
{"kind":"query","query":{"kind":"approval","conversation_id":"CONVERSATION_UUID","lookup":{"by":"approval","id":"approval:EXACT_ID"}}}
```

The same owner input can submit an existing domain command and wait for its durable command receipt. This covers exact operation permission decisions and cancellation for root or spawned tasks:

```json
{"kind":"command","command":{"schema_version":1,"command_id":"UNIQUE_COMMAND_UUID","conversation_id":"CONVERSATION_UUID","issuing_device_id":null,"issued_at":1792091546149,"payload":{"kind":"cancel_task","task_id":"TASK_UUID"}}}
```

Steering a running or waiting task uses the same envelope with `"payload":{"kind":"steer_task","task_id":"TASK_UUID","instruction":"..."}`. Its Completed receipt means durable delivery into the task's native input queue; the kernel applies it at the next safe checkpoint, as described in the [steering contract](../contracts/steering.md).

Preserve the complete envelope and its command ID after an ambiguous response. This CLI variant accepts only the complete current typed representation, including nullable fields, and explicitly rejects unknown fields at any envelope/payload/message level. It never silently drops additive metadata before host identity comparison. Domain IPC itself retains its broader additive-field contract. A Completed receipt establishes command application, not completion of the target task or proof of tool cleanup. A disposable Rust 1.88 release walkthrough used the actual headless host and CLI to resolve a real shell authorization request, replay its exact receipt, cancel a managed task, and reject changed decisions, unknown envelope/payload fields and an omitted nullable field. It did not execute the shell or make a successful model request.

Settings updates use an operation containing `kind: replace_settings`, the expected current revision, and the complete edited settings object. Preserve the schema and revision returned by the query. Recurring default loop limits reject an absolute execution deadline; per-occurrence scheduling captures its own deadline. A changed relay origin returns `restart_required` and takes effect only after restart. No default relay or model is selected automatically.

For a model credential, the input can select `credential_environment` with an exact `ModelCredentialReference` binding. The CLI reads only that provider's `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` from its environment, transfers it through authenticated ephemeral IPC, and prints no secret. The binding specifies `provider`, the official or explicit canonical loopback `endpoint`, a non-nil UUID `reference`, and `storage: session` or `native`. Add the same non-secret binding to settings in a separate revision-checked operation. Session secrets expire with the host; native secrets use the platform credential backend and never fall back to a plaintext file. Configured-but-missing references do not silently use an environment credential. Official provider environment fallback is available only when no reference is configured for that endpoint.

Pairing begins with an explicit owner operation returning an invitation and full host fingerprint. Approval supplies the real responder request, its full confirmed fingerprint, a label, and the chosen conversation/command/permission access. Invitation material is a secret out-of-band pairing capability. The host does not print it during ordinary status or debugging. Device revocation uses the explicit owner operation; relay enrollment and relay-side revocation are separate operator actions.

## Explicit resume and drain recovery

`chat --resume` wakes an existing managed task without changing its captured policy. Workspace, mode and budget overrides are rejected on host resumes. An uncertain provider attempt requires an explicit one-use retry bound to its current attempt ID. A known cancellation/interrupted-tool boundary requires the separate exact command, checkpoint revision, phase, and owner request ID. The CLI's explicit retry option selects the matching typed operation; generic owner JSON can preserve the same request ID after an ambiguous response. A changed attempt or checkpoint cannot reuse an earlier approval.

Cancelling an uncertain task does not erase its native pending turn or manufacture results. Such a conversation requires a new conversation until an explicit abandonment/branch contract exists.

On shutdown, query `runtime` and `tool_recovery` while the host drains. If the broker reports unresolved cleanup, the process deliberately keeps its state lease and owner socket. After owned workers stop, the host obtains an all-admission broker quiescence proof and conservatively converts stranded Running entries to Unknown. It keeps owner IPC available while a busy broker drains. Explicit `reconcile_drained` performs the same bounded classification without retrying effects. An `acknowledge_cleanup` operation supplies the exact returned operation ID and result digest plus the responsible operator and an actual inspection note. Acknowledgement is an explicit operator assertion; the host never generates it automatically. On a running, non-draining host, an acknowledgement that leaves the broker unquarantined asks the scheduler to redispatch deferred work and settle pending cancellations. While the broker is quarantined, deferred tasks keep their status and report a task `diagnostic` naming the quarantine and this recovery path; the proxying `chat` CLI prints it once while it waits. During drain, new execution remains disabled until the drained process exits and is intentionally restarted.

Deferred automated CLI, lifecycle, replay, recovery and platform checks are tracked in [#32](https://github.com/izyuumi/everything/issues/32).

## Discard an unstarted queued command

A cancelled or uncertain pending conversation is preserved for inspection. Later queued commands can be explicitly discarded without clearing that original native state. Read the known submission ID with the `command` query, then use the original acceptance timestamp and a unique request ID:

```json
{"kind":"operation","request_id":"UNIQUE_REQUEST_UUID","operation":{"kind":"discard_unapplied","command_id":"QUEUED_COMMAND_UUID","expected_accepted_at":1792091546149,"operator":"local-owner","reason":"This queued request is no longer needed."}}
```

Reuse that request ID and exact payload after an ambiguous response. The operation refuses any evidence of prior application, task admission, control semantics or incomplete initialization. It records an immutable rejection and frees pending capacity. For a paired-device command, its sender can receive the rejection reason/operator through normal encrypted result delivery. There is no automatic discard or implicit retry under a fresh identity.

When a new admission reaches inbox capacity, the host makes one automatic attempt to compact terminal payloads older than seven days, then retries the same reservation. Command identity, origin and replay digests remain permanent. Pending commands, recent terminal payloads and outstanding paired-device replies are retained. This can release the retained-payload budget; it does not remove the pending-command or identity limits.

## Startup MCP session credentials

Use `serve --mcp-credential-environment-file PATH` to bootstrap explicit process-local MCP session references before server discovery. The file contains exact existing authorized binding metadata and source environment names, with no values. Read [the bootstrap contract](../contracts/mcp-session-bootstrap.md) for format, bounds, authorization and restart semantics. A running owner `StoreMcp(session)` is rejected because the existing MCP catalog is fixed at startup. Persistent native references are a separate explicit option. Managed launchd installation does not provision the binding file or source values.

## Memory and optional services

The revisioned settings query includes non-secret `memory`, `mcp` and `selection` configuration. Edit the complete returned document with its expected revision; service changes require a Host restart. Defaults use real Markdown/FTS retrieval, no maintenance model, disabled embeddings, local selection and no MCP servers. The `composition` query reports actual active settings and separate desired restart flags:

```json
{"kind":"query","query":{"kind":"composition"}}
```

Retrieve real local provenance without a model answer request:

```json
{"kind":"query","query":{"kind":"retrieve_memory","question":"orchard irrigation"}}
```

Retrieval ranks chunks containing any query term, so ordinary sentences work. Use the existing typed maintenance requests with owner `memory_enqueue`, `memory_cancel`, `memory_retry` and `apply_memory_plan`; inspect each stable identity with `memory_job`. Without a maintenance model an ingest job waits in `waiting_for_model` until an explicit host plan or cancellation:

```json
{"kind":"operation","request_id":"UNIQUE_REQUEST_UUID","operation":{"kind":"apply_memory_plan","job_id":"ingest-1","plan":{"version":1,"page_path":"orchard/irrigation.md","expected_hash":null,"title":"Orchard irrigation","entries":[{"heading":"Schedule","summary":"Irrigation runs Tuesday at dawn.","source_ids":["s1"],"supersedes":[]}]}}}
```

A feature-enabled build adds `{"kind":"query","query":{"kind":"embedding_index"}}` for actual vector generation status; `rebuild_embeddings` is refused with `invalid_request` unless `memory.embeddings` in the `composition` query reports `enabled`. The [composition contract](../contracts/host-composition.md) lists these routes, and [consolidation](../contracts/consolidation.md) defines their source/citation and immutable preparation rules. Optional selection requires its exact one-shot `prepare_selection` disclosure and explicit `approve_selection` before admitting the same original command; configuration alone sends no classifier request.

Opt-in embeddings additionally require a release build of both binaries:

```sh
cargo build --release --features local-embeddings --bin everything --bin everything-embedding-worker --locked
```

Install the worker adjacent to the Host, supply the verified runtime/model paths and digest, and enable only a supported dimension in owner settings. Strict Linux GNU x86_64/aarch64 execution requires `/usr/bin/bwrap` and the required namespaces. Unsupported or unavailable confinement reports an explicit diagnostic and keeps genuine FTS before configured artifact or vector access. The Host never loads an in-process embedding fallback. Rebuild is an explicit bounded owner operation after restart; see [memory settings and lifecycle](../contracts/host-memory.md). Current-main CI/runtime verification remains pending in [the composition checkpoint](../contracts/host-composition.md#current-main-integration).

Scheduling uses the running host through [schedule commands](scheduling-cli.md); it never starts another scheduler or takes over host lifetime.
