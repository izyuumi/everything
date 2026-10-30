# Headless host and owner commands

Build the release binary with Rust 1.88 or newer. The host passed Rust 1.88 all-target/all-feature static checks and release build, plus focused disposable Linux serve/proxy/restart observations; consult [implementation status](../status/implementation.md) before relying on platform-specific behavior.

```sh
cargo build --release --bin everything --locked
target/release/everything --state-dir /absolute/private/state serve
```

`serve` owns the agent independently of clients. Closing an attached CLI does not cancel admitted work. Tasks of different conversations run in parallel, up to four by default; `--max-parallel-tasks N` or `EVERYTHING_MAX_PARALLEL_TASKS=N` sets 1–4, and 1 restores serial execution. A second message to a conversation whose task is still active is refused as busy rather than run beside it. The private absolute state path must fit the portable 100-byte Unix socket limit. Do not run a second host over that state directory.

While it serves, ordinary `init`, `chat`, and `status` commands proxy through owner IPC instead of opening another Agent or Tool Broker:

```sh
everything --state-dir /absolute/private/state init --provider openai --model gpt-4.1-mini
everything --state-dir /absolute/private/state status
everything --state-dir /absolute/private/state chat --conversation CONVERSATION_UUID --message 'Hello'
```

The CLI prints generated conversation and command identities before mutating requests. On an ambiguous disconnect, retry the original command ID and exact contents. Durable command acceptance means queued admission; task completion and committed assistant messages are separate. The proxy prints committed final text. It keeps waiting while the task waits for its own spawned children, because the host resumes it automatically when their results arrive; other waiting reasons still stop the proxy for owner inspection. The stop message names the task's actual waiting reason and diagnostic rather than the coarse `waiting_for_children` status. A task awaiting permission prints each pending approval's exact request, arguments, scope, expiry and ID from the owner `approval` query, like the foreground path, before the proxy stops. While the task runs, the proxy prints provisional assistant text from the `task` query's `live_text` as it grows, then prints only the remainder of the committed final text; if the saved text does not extend what was streamed, it prints a newline and the full saved text instead. Against an older host without `live_text`, the proxy prints only the committed final text. See [live assistant text](../contracts/owner-ipc.md#live-assistant-text).

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

Other query kinds include `conversations`, `conversation`, `tasks`, `task`, `approvals`, `approval`, `identity`, `devices`, `transport`, `command`, and `tool_recovery`. Lists require a limit from 1 to 100. `tasks` returns the newest tasks first. After a restart, a parent still waiting on a child that needs an owner decision carries a `diagnostic` naming that child and its reason. Resume or cancel the child, or cancel the parent (which cascades to its children); restart itself changes neither. Individual task metadata reports the actual waiting reason, diagnostic, cancellation state and active command. Private native checkpoints and credentials are not returned. Pending approval lists return small summaries with default `limit: 20` and an optional cursor. Fetch the exact arguments and scope separately before deciding:

```json
{"kind":"query","query":{"kind":"approval","conversation_id":"CONVERSATION_UUID","lookup":{"by":"approval","id":"approval:EXACT_ID"}}}
```

The same owner input can submit an existing domain command and wait for its durable command receipt. This covers exact operation permission decisions and cancellation for root or spawned tasks:

```json
{"kind":"command","command":{"schema_version":1,"command_id":"UNIQUE_COMMAND_UUID","conversation_id":"CONVERSATION_UUID","issuing_device_id":null,"issued_at":1792091546149,"payload":{"kind":"cancel_task","task_id":"TASK_UUID"}}}
```

Steering a running or waiting task uses the same envelope with `"payload":{"kind":"steer_task","task_id":"TASK_UUID","instruction":"..."}`. Its Completed receipt means durable delivery into the task's native input queue; the kernel applies it at the next safe checkpoint, as described in the [steering contract](../contracts/steering.md).

Preserve the complete envelope and its command ID after an ambiguous response. This CLI variant accepts only the complete current typed representation, including nullable fields, and explicitly rejects unknown fields at any envelope/payload/message level. It never silently drops additive metadata before host identity comparison. Domain IPC itself retains its broader additive-field contract. A Completed receipt establishes command application, not completion of the target task or proof of tool cleanup. A disposable Rust 1.88 release walkthrough used the actual headless host and CLI to resolve a real shell authorization request, replay its exact receipt, cancel a managed task, and reject changed decisions, unknown envelope/payload fields and an omitted nullable field. It did not execute the shell or make a successful model request.

Settings updates use an operation containing `kind: replace_settings`, the expected current revision, and the complete edited settings object. Preserve the schema and revision returned by the query. Recurring default loop limits reject an absolute execution deadline; per-occurrence scheduling captures its own deadline. A changed relay origin returns `restart_required` and takes effect only after restart. No default relay or model is selected automatically. The receipt's `write.changed_keys` lists only the typed Settings keys derived from the file (`models.selection`, `tools.selection`, `memory.embeddings.enabled`, `memory.embeddings.dimensions`). Model credential references, relay origin, workspace, execution mode, limits, worker, MCP and other memory fields are not typed keys, so a change limited to them reports empty `changed_keys`; read the returned revision, `restart_required` and settings snapshot instead.

For a model credential, the input can select `credential_environment` with an exact `ModelCredentialReference` binding. The CLI reads only that provider's `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` from its environment, transfers it through authenticated ephemeral IPC, and prints no secret. The binding specifies `provider`, the official or explicit canonical loopback `endpoint`, a non-nil UUID `reference`, and `storage: session` or `native`. Add the same non-secret binding to settings in a separate revision-checked operation. Session secrets expire with the host; native secrets use the platform credential backend and never fall back to a plaintext file. Configured-but-missing references do not silently use an environment credential. Official provider environment fallback is available only when no reference is configured for that endpoint.

Pairing begins with an explicit owner operation returning an invitation and full host fingerprint. Approval supplies the real responder request, its full confirmed fingerprint, a label, and the chosen conversation/command/permission access. Invitation material is a secret out-of-band pairing capability. The host does not print it during ordinary status or debugging. Device revocation uses the explicit owner operation; relay enrollment and relay-side revocation are separate operator actions.

## Optional Sign in with ChatGPT

OpenAI can use your ChatGPT plan instead of an API key ([design](../architecture/openai.md#optional-sign-in-with-chatgpt)). This is optional and never required. With a running host:

```sh
everything chatgpt sign-in                         # native storage by default; prints the authorize URL and opens the browser
everything chatgpt sign-in --no-browser --timeout-seconds 600
everything chatgpt select --reference UUID --storage native   # use the plan for official OpenAI
everything chatgpt status                           # email masked; --json or --show-email for the full address
everything chatgpt models                           # slugs your plan lists; --all adds hidden ones, --json for scripts
everything chatgpt sign-out
everything chatgpt deselect                         # back to API keys; stays signed in
```

`sign-in` uses the ChatGPT binding already selected in settings. If none is selected, it creates a new reference in `--storage` (default `native`; `--reference` reuses an earlier sign-in and then needs `--storage`). It and prints the exact `select` command after success. Signing in never changes settings by itself. `select` adds the `{"source":"chatgpt"}` binding through a revision-checked `replace_settings`; it refuses if the official OpenAI endpoint already has another credential binding. `deselect` removes it. The CLI waits in 25-second owner `wait` calls. Ctrl-C sends `cancel`, which closes the loopback listener. Output is the nonsecret status: signed in, plan usage, email, access expiry, whether a client is registered, the `https://chatgpt.com/settings/usage` link, the attempt and revocation outcome. The CLI masks the email to its domain (`…@example.com`); `status --json` or `status --show-email` prints it in full. The host returns an attempt's `authorize_url` only while the attempt is pending, so a completed, failed, cancelled or expired attempt no longer exposes its `state`, `nonce` or PKCE challenge; `sign-in` prints the URL when it starts.

The underlying owner operation is `{"kind":"operation","operation":{"kind":"chatgpt","operation":{…}}}` with `status`, `sign_in` (`binding`, `timeout_seconds` 30–1800), `wait` (`attempt_id`, `timeout_ms` ≤ 30000), `cancel` (`attempt_id`) or `sign_out`. A `binding` is `{"provider":"openai","endpoint":{"kind":"official"},"reference":"UUID","storage":"session"|"native","source":"chatgpt"}`. `models` uses the separate owner operation `{"kind":"chatgpt_models","binding":{…}}`: the host GETs `https://api.openai.com/v1/models` with the binding's bearer (refreshed like inference) and returns every entry's `slug`, `display_name` and `visibility`; the CLI prints `visibility == "list"` entries in server order unless `--all`. These requests carry no secret and use the ordinary file route. Session records are lost when the host restarts; the default `native` keeps the sign-in. On macOS the keychain item only opens without a prompt for the same code-signing identity that wrote it, so sign every `everything` binary that runs the host with [`scripts/sign_macos.sh`](#keep-the-chatgpt-sign-in-across-rebuilds-and-updates) after each build. A selected but signed-out binding makes OpenAI tasks stop as provider-unavailable with "not signed in with ChatGPT; sign in with ChatGPT or use an API key". The host never falls back to `OPENAI_API_KEY` for that endpoint while the binding is selected. ChatGPT operation failures (signed out, plan usage disabled, session ended, OpenAI rejections) are returned to the CLI with their message instead of a generic host failure. If a chat fails with "model is not supported … with a ChatGPT account", the error says to run `everything chatgpt models`; pick a listed slug.

### Keep the ChatGPT sign-in across rebuilds and updates

The macOS keychain trusts the code-signing designated requirement of the binary that created an item. Plain `cargo build` output is ad-hoc signed, so every rebuild is a new identity: the host's read fails (or prompts) and the next start looks signed out. Sign each build with a stable identity and the fixed identifier `ai.everything.cli`:

```sh
cargo build --release && scripts/sign_macos.sh           # signs target/debug and target/release `everything` if present
scripts/sign_macos.sh ~/.cargo/bin/everything             # after cargo install
```

The script uses `SIGN_IDENTITY` or `EVERYTHING_CODESIGN_IDENTITY` if set, else the first `Developer ID Application` identity, else the first `Apple Development` identity, and fails if there is none. Local signing uses no timestamp, so it works offline; `EVERYTHING_CODESIGN_RELEASE=1` adds the hardened runtime and a secure timestamp. Release CI signs and notarizes with the same script ([CLI releases](releases.md#macos-signing-and-notarization)). Items written by one certificate are not readable by binaries signed with another, so use one identity for every binary that runs your host. A sign-in written by an earlier ad-hoc build cannot be read by a signed build without the keychain prompt; sign in once more after switching. If saving a refreshed token fails, the host logs a warning ("the ChatGPT tokens could not be saved to the credential store"), keeps serving the token from memory and retries the save on every later request, so a working session is not lost before restart.

OpenAI offers this for open-source tools and personal projects that run locally. Paid or remotely hosted apps need OpenAI's waitlist. It is a preview, and request restrictions may change. To disconnect Everything entirely, use ChatGPT Settings.

## Explicit resume and drain recovery

`chat --resume` wakes an existing managed task without changing its captured policy. Workspace, mode and budget overrides are rejected on host resumes. An uncertain provider attempt requires an explicit one-use retry bound to its current attempt ID. A known cancellation/interrupted-tool boundary requires the separate exact command, checkpoint revision, phase, and owner request ID. The CLI's explicit retry option selects the matching typed operation; generic owner JSON can preserve the same request ID after an ambiguous response. A changed attempt or checkpoint cannot reuse an earlier approval.

Cancelling an uncertain task does not erase its native pending turn or manufacture results. Such a conversation requires a new conversation until an explicit abandonment/branch contract exists. A new message to a conversation whose pending turn belongs to a cancelled or failed task is refused at admission with an `invalid_request` failure naming that task and asking for a new conversation; a message admitted earlier is rejected with the same failure when the host next tries to apply it, so the waiting CLI exits instead of polling a command that stays `accepted`.

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
