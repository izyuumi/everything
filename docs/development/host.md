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

The CLI prints generated conversation and command identities before mutating requests. On an ambiguous disconnect, retry the original command ID and exact contents. Durable command acceptance means queued admission; task completion and committed assistant messages are separate. The proxy prints committed final text. Provisional live deltas remain available only in the direct foreground path at this stage.

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

Settings updates use an operation containing `kind: replace_settings`, the expected current revision, and the complete edited settings object. Preserve the schema and revision returned by the query. Recurring default loop limits reject an absolute execution deadline; per-occurrence scheduling captures its own deadline. A changed relay origin returns `restart_required` and takes effect only after restart. No default relay or model is selected automatically.

For a model credential, the input can select `credential_environment` with an exact `ModelCredentialReference` binding. The CLI reads only that provider's `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` from its environment, transfers it through authenticated ephemeral IPC, and prints no secret. The binding specifies `provider`, the official or explicit canonical loopback `endpoint`, a non-nil UUID `reference`, and `storage: session` or `native`. Add the same non-secret binding to settings in a separate revision-checked operation. Session secrets expire with the host; native secrets use the platform credential backend and never fall back to a plaintext file. Configured-but-missing references do not silently use an environment credential. Official provider environment fallback is available only when no reference is configured for that endpoint.

Pairing begins with an explicit owner operation returning an invitation and full host fingerprint. Approval supplies the real responder request, its full confirmed fingerprint, a label, and the chosen conversation/command/permission access. Invitation material is a secret out-of-band pairing capability. The host does not print it during ordinary status or debugging. Device revocation uses the explicit owner operation; relay enrollment and relay-side revocation are separate operator actions.

## Explicit resume and drain recovery

`chat --resume` wakes an existing managed task without changing its captured policy. Workspace, mode and budget overrides are rejected on host resumes. An uncertain provider attempt requires an explicit one-use retry bound to its current attempt ID. A known cancellation/interrupted-tool boundary requires the separate exact command, checkpoint revision, phase, and owner request ID. The CLI's explicit retry option selects the matching typed operation; generic owner JSON can preserve the same request ID after an ambiguous response. A changed attempt or checkpoint cannot reuse an earlier approval.

Cancelling an uncertain task does not erase its native pending turn or manufacture results. Such a conversation requires a new conversation until an explicit abandonment/branch contract exists.

On shutdown, query `runtime` and `tool_recovery` while the host drains. If the broker reports unresolved cleanup, the process deliberately keeps its state lease and owner socket. After owned workers stop, the host obtains an all-admission broker quiescence proof and conservatively converts stranded Running entries to Unknown. It keeps owner IPC available while a busy broker drains. Explicit `reconcile_drained` performs the same bounded classification without retrying effects. An `acknowledge_cleanup` operation supplies the exact returned operation ID and result digest plus the responsible operator and an actual inspection note. Acknowledgement is an explicit operator assertion; the host never generates it automatically. New execution remains disabled until the drained process exits and is intentionally restarted.

Deferred automated CLI, lifecycle, replay, recovery and platform checks are tracked in [#32](https://github.com/izyuumi/everything/issues/32).
