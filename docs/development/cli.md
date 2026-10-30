# Persistent native CLI

For an interactive full-screen chat, run `everything` with no subcommand (alias [`everything tui`](tui.md)). It can start a Host for the session and runs first-run model onboarding when no usable credential is configured.

Build with `cargo build --release --locked`. `everything --state-dir /absolute/private/path` places the global state option before the subcommand. `EVERYTHING_STATE_DIR` supplies the same default; otherwise the CLI uses `$HOME/.everything`. The state directory is created with owner-only permissions and validated on every foreground open. One host owns its exclusive lock.

## Initialize and inspect

Initialization is offline and requires no provider credential:

```sh
everything --state-dir /absolute/private/path init --provider anthropic --model claude-sonnet-4-6 --title "Work"
everything --state-dir /absolute/private/path status
everything --state-dir /absolute/private/path status --conversation CONVERSATION_UUID --history
```

Select `--provider openai` and a native Responses model ID for OpenAI. Model IDs are explicit strings passed to the provider; the CLI does not claim every model supports every request option. Initialization prints JSON with the conversation UUID and safe metadata. `status --conversation` prints metadata; `--history` explicitly includes persisted text. List output defaults to 20 conversations and supports `--limit` from 1 through 1000. Status output excludes private native continuation payloads.

Optional creation parameters are `--instructions`, `--max-output-tokens` (default 4096), and `--temperature`. A conversation's provider, model, instructions, endpoint, and sampling options are immutable. Existing conversation commands use the stored configuration; conflicting explicit options are rejected.

## Send and continue

Set the selected provider's credential in the process environment using your preferred secret-management mechanism: `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`. Do not put credentials in command arguments or source files.

```sh
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --message "Summarize my options."
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --message-file /absolute/message.txt
```

Each invocation performs one text turn; invoke `chat` again with the same UUID to continue its persisted history. A first `chat` can omit `--conversation` if it supplies `--provider` and `--model`; it creates a conversation and prints the conversation and command UUIDs to standard error. `--title` applies to that new conversation. Streaming text goes to standard output. It is provisional until standard error reports `saved message`. If the authoritative completed text differs from streamed deltas, the CLI prints the saved text separately.

Message files must be regular UTF-8 files at most 128 KiB. Input text must be nonempty, contain no NUL, and fit the same bound. There is no implicit stdin read or interactive prompt. Tool execution is explicit through `--tools`; ordinary text invocations retain the text-only path.

## Replay and recover

Supply `--command-id COMMAND_UUID` to give a new message an explicit durable identity. Repeating the same conversation, command ID, and exact text returns the saved result when completed. Different text or conversation identity under that command ID is rejected before inference. An accepted pending command is never retried automatically.

```sh
everything --state-dir /absolute/private/path status --conversation CONVERSATION_UUID --history
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --resume
```

`--resume` resumes the one pending command; optional `--command-id` must match it. A new message is rejected until the pending turn completes. Through a running host, a message to a conversation whose pending turn's task was cancelled or failed fails immediately with "this conversation's last task … was cancelled or failed before its turn completed …; start a new conversation". A missing credential, network failure, malformed provider response, or cancellation leaves an already accepted user message pending and preserves earlier messages. Inspect history before resuming. Provider processing may have happened before a disconnect; resume can issue another paid request. A durable completion may win a race with a termination signal, so always inspect status after an uncertain exit.

Initialization supports explicit `--conversation` and `--command-id` values for retries. Reuse the same title and configuration. The full external device command envelope is handled by a later host processor; the foreground command boundary comprises conversation ID, command ID, and exact text.

## Host queries and cancellation

With a running host, common owner queries have subcommands that print one tab-separated line per item; `--json` prints the raw owner response instead:

```sh
everything tasks [--conversation UUID] [--limit N]       # id, status, updated, conversation, instruction
everything approvals [--conversation UUID] [--limit N]   # pending approvals: id, tool, requested, conversation, task
everything conversations [--limit N]                     # id, idle|pending, messages, provider/model, title
everything schedules [--limit N]                         # id, enabled|disabled, next run, name
everything memory "QUESTION"                             # retrieved memory excerpts
everything cancel TASK_UUID                              # request cancellation and report the outcome
```

Limits default to 20 and the host accepts 1–100. `tasks` uses the owner query's creation order, newest first. `approvals` without `--conversation` checks the 100 most recently updated conversations that have a pending turn and prints their pending approvals; its `--json` output is the combined list of approval summaries rather than one owner response. `schedules` is the first page of `schedule list`; page further with `schedule list --after-id`. Times are local. These commands need the host and do not fall back to the foreground store.

`cancel` submits the existing `cancel_task` domain command for the task's conversation, then polls the task for up to five seconds and prints whether it ended (`cancelled`, or `failed` with the reason) or is still stopping. An already finished task is reported without submitting anything. Cancelling during a provider request leaves the task `failed` with an uncertain outcome that cannot be retried, as the [scheduler contract](../contracts/scheduler.md#cancellation-shutdown-and-recovery) describes.

In host-proxied `chat`, the first Ctrl-C after the turn is admitted (while text streams, while waiting, or at the in-place approval prompt) cancels the task the same way and prints the outcome, exiting 130. A second Ctrl-C while that cancellation is pending detaches instead, printing the usual `closing this CLI leaves admitted work in the host` line. Ctrl-C before admission still just stops the CLI.

## Credential vault

`everything vault` manages the local [credential vault](../contracts/vault.md) of a running host over owner IPC:

```sh
everything vault status                     # provider statuses and host-managed model credentials
everything vault list [--limit N]           # ref, kind, label, secret field names, detail
everything vault add KIND --label LABEL [metadata flags] [--field FIELD]...
everything vault remove ID|vault://local/ID
everything vault connect onepassword [--binary /absolute/path/op] [--account ACCOUNT_UUID] [--json]
everything vault disconnect onepassword
everything vault connect protonpass [--binary /absolute/path/pass-cli] [--web] [--json]
everything vault disconnect protonpass
```

`status`, `list`, `add` and `remove` accept `--json` for the raw response. `status` lists `local` (`ready` with its Keychain service name, or `unavailable` with the sanitized store error), `1password` (`disconnected`, `locked`, `ready` with the account UUID and pinned `op` path, or `unavailable` when the pinned binary is gone) and `protonpass` (`disconnected`, `locked`, `ready` with the token name or account and pinned `pass-cli` path, or `unavailable` with a sanitized reason; the Host runs `pass-cli info` for it), then each model credential binding from settings as `host_managed`: API keys show `configured`, ChatGPT bindings `signed_in`, `signed_out` or `unknown` from the host's status. Model credentials are never vault items and cannot be listed or used through the vault. `list` prints non-ready providers and incomplete (`pending`/`removing`) rows on stderr with the command that removes them.

`add` takes `login`, `api-secret`, `card` or `identity`. Metadata comes only from flags: `--site HOST` (repeatable) and `--username` for logins, `--env-name` and `--service` for API secrets, `--brand`, `--last4`, `--expiry-month` and `--expiry-year` for cards. A flag that does not belong to the kind is an error. `--field` chooses the secret fields to enter, in order; the default is `password`, `value` or `number`, and identity items need at least one of `email`, `phone`, `address`. There is no argv or environment route for a secret. When stdin is a terminal each field is read with echo off (raw mode, nothing echoed; Esc or Ctrl-C cancels). Otherwise stdin must hold exactly one line per field, at most 8192 bytes each. Buffers are pre-sized and zeroized, and the values travel only in the `vault_put` owner operation. It prints the new `vault://local/<uuid>` reference.

`connect onepassword` finds `op` on the CLI's `PATH` (or takes `--binary`), prints the path it pins, and sends it to the Host, which checks for `op` 2.x and lists accounts. With one account it is chosen; with several, a terminal asks for a number and other input needs `--account`. The Host then runs `op whoami` for that account (the 1Password approval prompt) and saves the path and account UUID in settings. `disconnect onepassword` runs `op signout` and removes them; a failed sign-out is logged and the connection is still removed. See [1Password setup](onepassword.md).

`connect protonpass` finds `pass-cli` on the CLI's `PATH` (or takes `--binary`) and prints the path it pins. By default it reads a Proton Pass personal access token (echo off on a terminal, otherwise exactly one piped line; never argv) and sends it in `vault_connect`; the Host stores it as a local vault item, logs in with it in an isolated session directory and saves the path and token reference in settings. `--web` instead runs `pass-cli login` in the current terminal with the Host's session directory, so pass-cli prints its sign-in URL and asks for any extra password at its own hidden prompt, and then asks the Host to confirm the session. `disconnect protonpass` runs `pass-cli logout` in that directory, removes the token item and the connection; a failed logout is logged and the connection is still removed. See [Proton Pass setup](protonpass.md).

## Optional ChatGPT plan

`everything chatgpt sign-in|status|models|sign-out|select|deselect` lets a running host use your ChatGPT plan for the official OpenAI endpoint instead of an API key. It is optional. See the [host guide](host.md#optional-sign-in-with-chatgpt). `models` lists the model slugs your plan accepts. `status` masks the account email to its domain unless `--json` or `--show-email` is given. The foreground path without a host always uses `OPENAI_API_KEY`.

## Optional relay

`everything relay on --origin wss://relay.example.com` and `everything relay off` set or clear this Host's relay origin, offline through the state lease or through a running Host's owner settings (which then needs a restart). `everything relay serve|enroll|revoke|devices` run or administer the blind relay server on `--database PATH`, defaulting to `relay.sqlite3` in the state directory. `everything relay status` prints JSON for both the Host connection and that server database. See [relay](../architecture/relay.md).

## Update

`everything update` installs the newest stable release over the running binary; `--beta` follows the single `v<version>-beta` prerelease instead, and `--check` only prints `channel=… current=… available=… tag=… status=newer|up-to-date` plus a sentence and exits 0. On macOS a team-signed binary is never replaced by an unsigned, ad-hoc or differently signed one unless `--allow-unsigned` is given, because the Sign in with ChatGPT keychain item would become unreadable. A loaded launchd service running this same executable is restarted. Details, integrity limits and `EVERYTHING_UPDATE_BASE_URL` are in [CLI releases](releases.md#update).

## Local proxy and verification boundaries

A new conversation can explicitly select a local proxy using `--loopback-endpoint http://127.0.0.1:PORT/v1/messages` for Anthropic, or `/v1/responses` for OpenAI. Literal IPv6 loopback is also supported. The full endpoint is persisted as immutable configuration; later commands retain it without repeating the flag. External custom endpoints, endpoint credentials, queries, and fragments are rejected. The foreground path still reads credentials from the selected environment variable. A running host does not: a loopback endpoint needs an explicit `credential_environment` binding, described in the [host guide](host.md#owner-inspection-and-configuration). When a proxied task stops as provider-unavailable without such a binding, the CLI prints the input to use.

Compilation and static analysis do not verify live inference. Focused local validation can inspect offline initialization, restart persistence, strict replay, pending recovery, owner-only paths, and cancellation against disposable local resources. Do not send requests to live providers merely to verify a build. Automated tests, including issue #10's mock integration coverage, are deferred to [issue #32](https://github.com/izyuumi/everything/issues/32).

## Native tool rounds

Use a fresh conversation when entering native tool/context mode after a legacy text continuation. A new native turn saves the workspace, mode, context budget, memory inputs, selected tools, and run limits. The foreground CLI currently uses no memory excerpts; an injected host can supply retrieved excerpts and provenance.

```sh
everything --state-dir /absolute/private/path chat --provider openai --model MODEL_ID --tools --workspace /absolute/workspace --message "Find the relevant source files."
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --tools --resume
```

Read-only mode is the default. `--execution-mode trusted-workspace` permits authorized native writes inside the saved workspace. The mode is saved per native turn, so a later message in the same conversation can pass a different `--execution-mode`. In read-only mode `search_tools` omits write tools and reports in `diagnostics` that writes are unavailable; a direct `write_file` call returns a `read_only_mode` tool error naming the write-capable modes, and the CLI prints `writes are disabled in read-only mode; send the next message with --tools --execution-mode trusted-workspace (or full-host) to allow writes`. `--execution-mode full-host` exposes host-scoped capabilities, including shell, subject to exact operation approval. Arbitrary shell has full host access; path checks are not a sandbox. The registered tools are read_file, write_file, search_files, shell, and offline search_tools. The kernel can expand advertised schemas through successful local discovery without discarding native provider continuation.

With a running host and an interactive terminal, `chat` prompts for a required approval in place and continues the same turn, also accepting a decision made in another client; see the [host guide](host.md). `--no-wait` keeps the exit-and-resume behaviour below. The foreground path and non-interactive host use print the exact request, arguments, scope, expiry, and UUID, followed by the exact commands for a separate decision:

```sh
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --tools --resume --approve APPROVAL_UUID
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --tools --resume --deny APPROVAL_UUID
```

For a `vault_use` approval the in-place prompt also offers `[t]ask`, which allows that exact reference and destination for the rest of the task (a vault lease, at most one hour); the non-interactive form is `--resume --approve APPROVAL_UUID --for-task`. See the [vault contract](../contracts/vault.md#vault-lease-grant).

`--resume` on a conversation without a pending turn, for example one whose turn already finished, explains how to read the saved reply with `status --conversation CONVERSATION_UUID --history` or start a new turn with `--message`.

An uncertain provider attempt stops on normal resume. Inspect status, then explicitly authorize another potentially paid request with `--tools --resume --retry-uncertain`. This flag never re-executes an unknown broker operation. In-flight broker operations retain their original UUID for lookup; unknown effects and required cleanup reconciliation remain stopped.

Initial native budget options are `--context-tokens` (default 128000), `--max-model-attempts` (16), `--max-tool-calls` (64, including zero for a hard no-tools budget), `--max-charged-tokens` (1000000), and `--max-duration-seconds` (3600). Charged tokens conservatively reserve estimated input plus output capacity before each request; reported provider usage is separate. Active resume uses its saved setup and rejects workspace, mode, or budget overrides. An accepted command without an attached native setup, such as one stopped by missing credentials, may still supply those initial flags on resume.

Tool messages and results are durable before subsequent inference. Larger model-facing results use explicit truncation metadata; the full output remains in the broker journal. Intermediate `saved assistant message` output may contain calls; only `completed task` reports a saved final native response. A paused tool invocation exits unsuccessfully and leaves resumable state. `status --history` excludes opaque provider checkpoint data.

The CLI owns one foreground host and broker. It retains discovery bindings through the stream and drains active tool cleanup on signals or output failure. Background task scheduling and proxying CLI commands to an already running host are separate integrations. Live inference and native tool continuation through a real provider were verified on 2026-10-21; they require authorized credentials. See [agent contract](../contracts/agent.md).
