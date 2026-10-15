# Persistent text CLI

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

Message files must be regular UTF-8 files at most 128 KiB. Input text must be nonempty, contain no NUL, and fit the same bound. There is no implicit stdin read or interactive prompt. No tools execute through this initial text-only CLI.

## Replay and recover

Supply `--command-id COMMAND_UUID` to give a new message an explicit durable identity. Repeating the same conversation, command ID, and exact text returns the saved result when completed. Different text or conversation identity under that command ID is rejected before inference. An accepted pending command is never retried automatically.

```sh
everything --state-dir /absolute/private/path status --conversation CONVERSATION_UUID --history
everything --state-dir /absolute/private/path chat --conversation CONVERSATION_UUID --resume
```

`--resume` resumes the one pending command; optional `--command-id` must match it. A new message is rejected until the pending turn completes. A missing credential, network failure, malformed provider response, or cancellation leaves an already accepted user message pending and preserves earlier messages. Inspect history before resuming. Provider processing may have happened before a disconnect; resume can issue another paid request. A durable completion may win a race with a termination signal, so always inspect status after an uncertain exit.

Initialization supports explicit `--conversation` and `--command-id` values for retries. Reuse the same title and configuration. The full external device command envelope is handled by a later host processor; the foreground command boundary comprises conversation ID, command ID, and exact text.

## Local proxy and verification boundaries

A new conversation can explicitly select a local proxy using `--loopback-endpoint http://127.0.0.1:PORT/v1/messages` for Anthropic, or `/v1/responses` for OpenAI. Literal IPv6 loopback is also supported. The full endpoint is persisted as immutable configuration; later commands retain it without repeating the flag. External custom endpoints, endpoint credentials, queries, and fragments are rejected. Credentials are still read from the selected environment variable.

Compilation and static analysis do not verify live inference. Focused local validation can inspect offline initialization, restart persistence, strict replay, pending recovery, owner-only paths, and cancellation against disposable local resources. Do not send requests to live providers merely to verify a build. Automated tests, including issue #10's mock integration coverage, are deferred to [issue #32](https://github.com/izyuumi/everything/issues/32).
