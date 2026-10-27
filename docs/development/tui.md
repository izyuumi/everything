# Terminal UI

`everything tui` opens an interactive full-screen chat against a running Host. It talks to the Host only through the existing owner IPC (`HostClient`): conversation/task/approval queries, `Initialize` and `Text` owner commands, `ResolvePermission` and `CancelTask` domain commands, and `ChatGptModels`. It adds no runtime, store, protocol field or credential path; the Host keeps owning execution and durable state.

```sh
everything service start            # or: everything serve
everything tui                      # new conversation with the latest conversation's provider/model
everything tui --continue           # reopen the most recent conversation
everything tui --conversation UUID
everything tui --provider openai --model MODEL_ID
```

Without a reachable Host the command exits before touching the terminal and names `everything service start` / `everything serve`. A conversation is created on the first message, titled from it. Provider, model and instructions stay immutable per conversation, so `/model` and `/instructions` apply to the next new conversation.

## Layout

The transcript starts with a banner (model, conversation, tools, Host state directory, cwd). User turns are prefixed `❯`; replies render Markdown under `◈ Everything` with a model/duration footer. Single newlines in replies are shown as line breaks. While a task runs, provisional text from the `task` query's [`live_text`](../contracts/owner-ipc.md#live-assistant-text) streams under a spinner; the committed final message replaces it. The input sits above a status bar showing model, provider, title, message count, tool mode, state, elapsed time and queued messages. Source lives in `src/tui/` (`host` IPC adapter, `app` state, `render`, `markdown`, `input`, `theme`).

## Commands and keys

`/help`, `/new [title]`, `/sessions` (also Ctrl+X), `/resume <n|id-prefix>`, `/model [id]` (lists the ChatGPT plan's models when a ChatGPT binding is selected), `/provider`, `/instructions`, `/tools on [dir] [read-only|trusted-workspace|full-host]` / `/tools off`, `/approvals`, `/history`, `/retry`, `/copy`, `/stop`, `/status`, `/clear`, `/mouse`, `/queue`, `/quit`.

Enter sends; Alt+Enter, Ctrl+J or a trailing `\` inserts a newline; Tab completes commands; Up/Down walk input history or lines; PageUp/PageDown or the wheel scroll; End jumps to the latest output. Ctrl+L starts a new conversation and Ctrl+D exits on empty input. `/mouse` releases mouse capture for terminal text selection.

## Tool approvals

`/tools on` sends `tools`, `workspace` and `execution_mode` as `RequestedRunOptions` with each message. When a task reaches `awaiting_permission`, the exact pending request is shown with tool, mode, workspace, capabilities, targets, expiry and arguments. `y` and `n` submit `ResolvePermission` for that operation and keep following the task; Esc leaves it pending for `/approvals` or another client. Full-host mode still requires each operation's approval.

## Interrupting

Typing while a reply runs queues the message; queued messages send in order after the task completes. Ctrl+C first warns, and a second Ctrl+C within three seconds (or `/stop`) submits `CancelTask`. Cancelling while a provider request is in flight makes the task a nonretryable `Failed` with an uncertain outcome, and the conversation cannot continue ([scheduler contract](../contracts/scheduler.md#cancellation-shutdown-and-recovery)). The TUI therefore asks twice. Before sending, it compares `pending_command_id` with the conversation's tasks: a terminal pending task moves to a new conversation, a running one is followed with the message queued, and one awaiting permission reopens its approval. Quitting the TUI never cancels Host work.

## Validation

Rust 1.92 `cargo fmt --all --check` and strict Clippy, default and `--all-features`, passed on macOS. A manual walkthrough on macOS (tmux, 120×45) used the user's launchd Host and a ChatGPT-plan OpenAI binding. It observed the banner, streamed and final Markdown replies, a queued follow-up sent after completion, `--continue`, command completion, the session picker and help overlay, and a full-host `shell` approval allowed through the overlay with the continued reply. Ctrl+C exit restored the terminal. Narrow terminals, Linux terminals, Anthropic conversations, the busy/waiting/stopped pre-send branches through this binary, and long transcripts were not exercised. Automated coverage remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).
