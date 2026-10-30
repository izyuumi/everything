# Persistent native CLI

For an interactive full-screen chat against a running Host, use [`everything tui`](tui.md).

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

`--resume` resumes the one pending command; optional `--command-id` must match it. A new message is rejected until the pending turn completes. A missing credential, network failure, malformed provider response, or cancellation leaves an already accepted user message pending and preserves earlier messages. Inspect history before resuming. Provider processing may have happened before a disconnect; resume can issue another paid request. A durable completion may win a race with a termination signal, so always inspect status after an uncertain exit.

Initialization supports explicit `--conversation` and `--command-id` values for retries. Reuse the same title and configuration. The full external device command envelope is handled by a later host processor; the foreground command boundary comprises conversation ID, command ID, and exact text.

## Optional ChatGPT plan

`everything chatgpt sign-in|status|models|sign-out|select|deselect` lets a running host use your ChatGPT plan for the official OpenAI endpoint instead of an API key. It is optional. See the [host guide](host.md#optional-sign-in-with-chatgpt). `models` lists the model slugs your plan accepts. `status` masks the account email to its domain unless `--json` or `--show-email` is given. The foreground path without a host always uses `OPENAI_API_KEY`.

## Optional relay

`everything relay on --origin wss://relay.example.com` and `everything relay off` set or clear this Host's relay origin, offline through the state lease or through a running Host's owner settings (which then needs a restart). `everything relay serve|enroll|revoke|devices` run or administer the blind relay server on `--database PATH`, defaulting to `relay.sqlite3` in the state directory. `everything relay status` prints JSON for both the Host connection and that server database. See [relay](../architecture/relay.md).

## Local proxy and verification boundaries

A new conversation can explicitly select a local proxy using `--loopback-endpoint http://127.0.0.1:PORT/v1/messages` for Anthropic, or `/v1/responses` for OpenAI. Literal IPv6 loopback is also supported. The full endpoint is persisted as immutable configuration; later commands retain it without repeating the flag. External custom endpoints, endpoint credentials, queries, and fragments are rejected. The foreground path still reads credentials from the selected environment variable. A running host does not: a loopback endpoint needs an explicit `credential_environment` binding, described in the [host guide](host.md#owner-inspection-and-configuration). When a proxied task stops as provider-unavailable without such a binding, the CLI prints the input to use.

Compilation and static analysis do not verify live inference. Focused local validation can inspect offline initialization, restart persistence, strict replay, pending recovery, owner-only paths, and cancellation against disposable local resources. Do not send requests to live providers merely to verify a build. Automated tests, including issue #10's mock integration coverage, are deferred to [issue #32](https://github.com/izyuumi/everything/issues/32).

## Native tool rounds

Use a fresh conversation when entering native tool/context mode after a legacy text continuation. A new native turn saves the workspace, mode, context budget, memory inputs, selected tools, and run limits. The foreground CLI currently uses no memory excerpts; an injected host can supply retrieved excerpts and provenance.

```sh
everything --state-dir /absolute/private/path chat --provider openai --model MODEL_ID --tools --workspace /absolute/workspace --message "Find the relevant source files."
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --tools --resume
```

Read-only mode is the default. `--execution-mode trusted-workspace` permits authorized native writes inside the saved workspace. `--execution-mode full-host` exposes host-scoped capabilities, including shell, subject to exact operation approval. Arbitrary shell has full host access; path checks are not a sandbox. The registered tools are read_file, write_file, search_files, shell, and offline search_tools. The kernel can expand advertised schemas through successful local discovery without discarding native provider continuation.

A required approval prints its exact request, arguments, scope, expiry, and UUID. Review it before a separate decision:

```sh
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --tools --resume --approve APPROVAL_UUID
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --tools --resume --deny APPROVAL_UUID
```

An uncertain provider attempt stops on normal resume. Inspect status, then explicitly authorize another potentially paid request with `--tools --resume --retry-uncertain`. This flag never re-executes an unknown broker operation. In-flight broker operations retain their original UUID for lookup; unknown effects and required cleanup reconciliation remain stopped.

Initial native budget options are `--context-tokens` (default 128000), `--max-model-attempts` (16), `--max-tool-calls` (64, including zero for a hard no-tools budget), `--max-charged-tokens` (1000000), and `--max-duration-seconds` (3600). Charged tokens conservatively reserve estimated input plus output capacity before each request; reported provider usage is separate. Active resume uses its saved setup and rejects workspace, mode, or budget overrides. An accepted command without an attached native setup, such as one stopped by missing credentials, may still supply those initial flags on resume.

Tool messages and results are durable before subsequent inference. Larger model-facing results use explicit truncation metadata; the full output remains in the broker journal. Intermediate `saved assistant message` output may contain calls; only `completed task` reports a saved final native response. A paused tool invocation exits unsuccessfully and leaves resumable state. `status --history` excludes opaque provider checkpoint data.

The CLI owns one foreground host and broker. It retains discovery bindings through the stream and drains active tool cleanup on signals or output failure. Background task scheduling and proxying CLI commands to an already running host are separate integrations. Successful live inference and native tool continuation with real providers require authorized credentials and remain unverified in this environment. See [agent contract](../contracts/agent.md).
