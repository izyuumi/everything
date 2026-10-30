# Terminal UI

`everything` with no subcommand, or its alias `everything tui`, opens an interactive full-screen chat against a Host. It talks to the Host only through the existing owner IPC (`HostClient`): conversation/task/approval queries, `Initialize` and `Text` owner commands, `ResolvePermission` and `CancelTask` domain commands, and `ChatGptModels`. It adds no runtime, store, protocol field or credential path; the Host keeps owning execution and durable state.

```sh
everything                          # same as `everything tui`
everything tui                      # new conversation with the latest conversation's provider/model
everything tui --continue           # reopen the most recent conversation
everything tui --conversation UUID
everything tui --provider openai --model MODEL_ID
```

Flags (`--continue`, `--conversation`, `--provider`, `--model`) need the `tui` form. Without an interactive terminal on stdin and stdout, it exits non-zero with "the terminal UI needs an interactive terminal; run `everything --help` for commands" before touching the terminal or the state directory. A conversation is created on the first message, titled from it. Provider, model and instructions stay immutable per conversation, so `/model` and `/instructions` apply to the next new conversation.

## Starting the Host

When no Host answers for the state directory, the TUI offers, inside the terminal UI:

- **Start the host in the background until this window closes.** It spawns `everything --state-dir DIR serve` in its own process group with null stdio, waits up to 30 seconds for the owner socket, and sends it SIGTERM when the TUI exits (normal quit, Ctrl+C, SIGTERM, terminal hangup or an error). Work still running in that Host is interrupted like any other Host shutdown.
- **Install and start the login service** (macOS only). It runs the existing `everything service install` and `start` code (enabling a disabled service first). The LaunchAgent label is global, so this replaces any existing Everything LaunchAgent and points it at this state directory.
- **Quit**, which restores the terminal and exits with the usual "cannot reach the Everything host" error.

## First-run onboarding

After connecting, the TUI decides whether a usable model credential is configured: any API-key binding in settings counts as usable (the TUI cannot inspect stored secrets); a ChatGPT binding counts only if its status is signed in with plan usage enabled; with no binding at all, onboarding runs only when the Host has no conversations, so existing environment-variable users are not prompted. Source is `src/tui/onboarding.rs`; its steps are an ordered list (`Welcome`, `Provider`, `Verify`, `Save`, `Service`, `Vault`). Skipping the model steps jumps to `Vault`, the optional "Connect a password manager" step: Skip continues; 1Password and Proton Pass say they are not available yet, link #220 or #221, connect nothing and continue; "Add an API secret to the local vault" asks for a label and then the secret in the masked input, saves it with `vault_put` and shows only the new reference.

1. **Welcome.** A one-line security note (credentials stay on this machine in the system keychain; the model provider sees conversation content) and Quick start / Custom / Skip. Quick start means OpenAI, native keychain storage and the default model (`gpt-5` for an API key, the plan's first listed model for ChatGPT). Custom asks for provider (OpenAI or Anthropic), endpoint (official or a loopback URL), model and storage (keychain or Host memory).
2. **Provider.** A read-only scan shows whether `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` are set in the TUI's environment and whether settings hold a ChatGPT binding that needs signing in again; values are never shown. Choices are Sign in with ChatGPT (the same owner `chatgpt` sign-in/wait/cancel operations as `everything chatgpt sign-in`, opening the browser and showing the URL; an existing ChatGPT binding is reused), using a found environment key, or typing an API key into a masked input. The input buffer is a preallocated `Zeroizing<String>` capped at 8 KiB so it never reallocates; pasted text is also zeroized.
3. **Verify.** For an API key, the TUI builds the provider with the Host's own construction (`host::providers::api_key_provider`) and streams a tiny prompt (`max_output_tokens` 256, 60-second limit); success means the provider completed the response. For ChatGPT, verification is the signed-in status with plan usage plus an authenticated plan model listing (`chatgpt_models`); it is not a completion, because ChatGPT tokens live in the Host's vault. On failure the TUI offers Retry, Pick another provider, or Skip.
4. **Save.** Only a verified route is saved: an API key goes through the owner `credential` `store` operation (keychain or session storage per the choice), then `ReplaceSettings` adds the binding to `model_credentials`, replacing any binding for the same provider and endpoint. Settings are re-read to confirm; if the settings write fails, the just-stored credential is removed. The default conversation in settings is not changed, because a model-selection change is a protected setting that needs native confirmation; the chosen provider, endpoint and model are used for the TUI's next new conversation instead. Skip at any point leaves settings untouched.
5. **Service.** On macOS, when no LaunchAgent is installed, an offer (default *Not now*) to run `everything service install`. It does not start the service while a Host is running.

Esc goes back (on the first screen it skips), Ctrl+C or SIGTERM exits cleanly and restores the terminal. Later launches with a saved route but no conversations start with no model selected; pick one with `/model`.

## Layout

The transcript starts with a banner (model, conversation, tools, Host state directory, cwd). User turns are prefixed `❯`; replies render Markdown under `◈ Everything` with a model/duration footer, extended with `used N memories` when the Host retrieved Wiki memory excerpts for that reply. Single newlines in replies are shown as line breaks. While a task runs, provisional text from the `task` query's [`live_text`](../contracts/owner-ipc.md#live-assistant-text) streams under a spinner; the committed final message replaces it. The input sits above a status bar showing model, provider, title, message count, tool mode, state, elapsed time and queued messages. Source lives in `src/tui/` (`host` IPC adapter, `app` state, `render`, `markdown`, `input`, `theme`).

## Commands and keys

`/help`, `/new [title]`, `/sessions` (also Ctrl+X), `/resume <n|id-prefix>`, `/model [id]` (lists the ChatGPT plan's models when a ChatGPT binding is selected), `/provider`, `/instructions`, `/tools on [dir] [read-only|trusted-workspace|full-host]` / `/tools off`, `/approvals`, `/vault [list]` / `/vault add LABEL` / `/vault remove ID|REF`, `/history`, `/retry`, `/copy`, `/stop`, `/status`, `/clear`, `/mouse`, `/queue`, `/quit`.

Enter sends; Alt+Enter, Ctrl+J or a trailing `\` inserts a newline; Tab completes commands; Up/Down walk input history or lines; PageUp/PageDown or the wheel scroll; End jumps to the latest output. Ctrl+L starts a new conversation and Ctrl+D exits on empty input. `/mouse` releases mouse capture for terminal text selection.

## Vault

`/vault add LABEL` validates the label and opens a masked modal for a local `api_secret` item. Typed and pasted characters render as `•` (at most 8192 bytes); Enter sends the value straight into `SecretTransfer` and the `vault_put` owner operation, Esc or Ctrl+C discards it. The value lives only in a pre-sized zeroizing buffer: it never enters the input line, input history, transcript, chat events or logs. The transcript note shows the label and the new reference. `/vault` lists provider statuses and items as label, fields and reference; `/vault remove` takes an item UUID or `vault://local/` reference. Other kinds and metadata use `everything vault add`.

## Tool approvals

`/tools on` sends `tools`, `workspace` and `execution_mode` as `RequestedRunOptions` with each message. When a task reaches `awaiting_permission`, the exact pending request is shown with tool, mode, workspace, capabilities, targets, expiry and arguments. `y` and `n` submit `ResolvePermission` for that operation and keep following the task; Esc leaves it pending for `/approvals` or another client. Full-host mode still requires each operation's approval.

## Interrupting

Typing while a reply runs queues the message; queued messages send in order after the task completes. Ctrl+C first warns, and a second Ctrl+C within three seconds (or `/stop`) submits `CancelTask`. Cancelling while a provider request is in flight makes the task a nonretryable `Failed` with an uncertain outcome, and the conversation cannot continue ([scheduler contract](../contracts/scheduler.md#cancellation-shutdown-and-recovery)). The TUI therefore asks twice. Before sending, it compares `pending_command_id` with the conversation's tasks: a terminal pending task moves to a new conversation, a running one is followed with the message queued, and one awaiting permission reopens its approval. Quitting the TUI never cancels Host work.

## Validation

Onboarding (2026-10-31, macOS, tmux 110×36, disposable `$HOME/.ev-test-*` state, debug build): bare `everything` with no Host reached the start offer; the background Host started and onboarding appeared. Skip left settings at revision 1 with no bindings, exited 0, and the session Host was gone. An invalid key against a local OpenAI-compatible stub on a loopback endpoint was masked, failed the test turn with "provider authentication failed" and offered Retry / Pick another / Skip; Retry failed again; a valid stub key with session storage passed and saved (revision 2, one loopback binding), and a chat message through the Host then returned the stub's reply. A fake `OPENAI_API_KEY` was detected without showing its value. Quit at the start offer printed the unreachable-host error (exit 1); Ctrl+C mid-wizard and closing the tmux server (SIGHUP) both stopped the session Host. Not exercised: ChatGPT browser sign-in through onboarding, native keychain saves, a real OpenAI or Anthropic key, the login-service install/start choices (the LaunchAgent label is global), Linux, and narrow terminals.

Rust 1.92 `cargo fmt --all --check` and strict Clippy, default and `--all-features`, passed on macOS. A manual walkthrough on macOS (tmux, 120×45) used the user's launchd Host and a ChatGPT-plan OpenAI binding. It observed the banner, streamed and final Markdown replies, a queued follow-up sent after completion, `--continue`, command completion, the session picker and help overlay, and a full-host `shell` approval allowed through the overlay with the continued reply. Ctrl+C exit restored the terminal. Narrow terminals, Linux terminals, Anthropic conversations, the busy/waiting/stopped pre-send branches through this binary, and long transcripts were not exercised. Automated coverage remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).
