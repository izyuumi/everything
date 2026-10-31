# ADR-0016: Credential vault with reference-only agent access

Status: Accepted design, not implemented. Tracker [#215](https://github.com/izyuumi/everything/issues/215); this record lands with [#216](https://github.com/izyuumi/everything/issues/216) and the implementation follows in [#217](https://github.com/izyuumi/everything/issues/217)–[#223](https://github.com/izyuumi/everything/issues/223). The [vault contract](../contracts/vault.md) and [vault architecture](../architecture/vault.md) are authoritative for the details.

## Context

Tasks need the owner's secrets: an API token for a `curl` call, a token for an MCP server, later a login for a form. Today the only owner secrets the Host holds are model, System One and MCP credentials, each bound to a configured reference ([credentials](../contracts/credentials.md), [MCP credentials](../contracts/mcp-credentials.md)). There is no place for general secrets and no way for the agent to ask for one without the value passing through model context, tool arguments or permission rows, which store plaintext arguments that the Apple clients display.

Vorra already has the parts a vault needs: the zeroizing `CredentialSecret`, the `NativeCredentialStore` (macOS Keychain or Linux Secret Service, three-second reads, one admission permit, no environment or file fallback), the pipe-only `SecretTransfer`, the private `SubmitCredential` entry route, exact-operation approvals and the shell tool's literal redactor. Many owners also keep secrets in 1Password or Proton Pass and do not want a second copy.

## Decision

Vorra gets a credential vault in a new `src/vault/` module. The agent works with **references and nonsecret metadata only**; the Host resolves a value inside the Tool Broker at execution time and delivers it to one approved destination.

- **Invariant.** A secret value never appears in model context, provider requests, events, conversation or domain journals, permission rows, logs, wiki or memory, settings TOML, argv, relay payloads, error messages or Debug output, encrypted or not. Values live only in `CredentialSecret` / `SecretTransfer` buffers, the native store or the external manager, and the one destination the owner approved.
- **Typed items.** A closed set of kinds (`login`, `api_secret`, `card`, `identity`) splits model-visible metadata from secret fields. Usernames and emails are metadata in v1.
- **Opaque references.** `vault://<provider>/<id>#<field>` with provider IDs, never names, so renaming an item cannot redirect a reference.
- **Providers.** A `VaultProvider` trait with a built-in local provider (SQLite index plus one native credential per item under service `to.yumi.vorra.vault`) and two external providers driven through their official CLIs: 1Password `op` with desktop-app integration ([#220](https://github.com/izyuumi/everything/issues/220)) and Proton Pass `pass-cli` ([#221](https://github.com/izyuumi/everything/issues/221)). Provider subprocesses run with an absolute pinned binary, an empty environment, bounded output, a timeout and cancellation, and never through the model's `shell` tool.
- **Agent tools.** `vault_search` (metadata only, no empty query), `vault_use` (destinations `env` and `mcp_env`; `http_header` and `form_fill` reserved until an HTTP or browser tool exists) and `vault_request` (asks the owner to add a missing secret through masked input). Every `vault_use` needs an approval: *Allow once* or *Allow for this task*, the second being a new vault lease grant scoped to conversation, task, reference and destination and expiring at the earlier of task end and one hour. No wildcards. Cards are per-use and have no destination in v1.
- **Redaction.** The static redactor built from `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` becomes a per-task redactor. Every resolved value joins it, it covers shell, read/search, MCP results and diagnostics, and its values are zeroized at task end.
- **Owner entry.** Secrets enter only through the private owner pipe (`vault_put` with `SecretTransfer`), from the TUI masked field, `vorra vault add` (tty with echo off or piped stdin, never argv) or the macOS SecureField sheet. iOS is approve-only in v1.
- **Sign in with ChatGPT stays separate.** SIWC records ([ADR-0014](ADR-0014-optional-sign-in-with-chatgpt.md)) share the native store, its permit and the code-signing fix, but are model credentials under their own accounts. They are never listed by `vault_search` or resolvable by `vault_use`; giving the model a path to its own provider's bearer token is exactly what the vault must prevent.
- **Scope.** The owner's tracker [#215](https://github.com/izyuumi/everything/issues/215) puts form fill and HTTP-header destinations, card use and secret entry from iOS out of scope for now. The 1Password and Proton Pass routes follow the approved plan's recommendations; the related questions below remain open and can still change them before [#220](https://github.com/izyuumi/everything/issues/220) and [#221](https://github.com/izyuumi/everything/issues/221).

## Rejected options

- **1Password official SDK in a sidecar.** Official SDKs with desktop authentication exist only for Go, JavaScript and Python. A sidecar adds a second runtime, which [AGENTS.md](../../AGENTS.md) rules out. Revisit if 1Password ships an official Rust SDK.
- **Unofficial 1Password Rust SDK** (`opz-rs/onepassword-sdk-unofficial`). It rides on undocumented desktop-app IPC that 1Password says can change without notice.
- **1Password Connect.** It needs a self-hosted server and a token: too much infrastructure for a single-user local app.
- **1Password service accounts for individuals.** They cannot read Personal or Private vaults. They stay a possible option for headless Linux only, with that limitation stated.
- **Unofficial Proton Pass clients.** Proton has no public Pass API or SDK; unofficial clients reverse-engineer it. Only the official `pass-cli` is used.
- **A separate vault store or file.** Custom secret storage or cryptography is ruled out; the vault reuses `NativeCredentialStore` and its no-fallback rules.
- **Values in tool arguments or approvals.** Permission rows persist arguments in plaintext and clients display every argument, so arguments carry references only.

## Consequences

The agent can use owner secrets for shell commands and MCP servers without any value entering model context or durable Host records. Each use is visible and approved per task. The design keeps one native runtime and adds no cryptography.

The guarantee has honest limits, listed in the [threat model](../architecture/vault.md#threat-model): an approved FullHost `shell` can echo, transform (for example base64) or send out an injected value past literal redaction, and can call `op` or `pass-cli` directly, bypassing Vorra. A compromised process running as the same user is out of scope, as for every native credential. The local provider depends on stable code signing ([ADR-0014 credential durability](ADR-0014-optional-sign-in-with-chatgpt.md#credential-durability)); unsigned rebuilds cannot read earlier items without a prompt. The `http_header` and `form_fill` destinations, card use and iOS entry are deferred.

## Open questions

These stay open; nothing here decides them.

1. **iOS secret entry.** Keep iOS approve-only, or allow secret entry through the E2EE relay? The second needs a change to the credentials contract, which forbids secrets in relay messages today.
2. **Lease length.** Is the one-hour cap on *Allow for this task* right? Should API secrets get a longer "always for this reference and env name" option?
3. **Proton Pass.** Is requiring a paid plan and `pass-cli` acceptable? Should connect default to a full login or to a vault-scoped personal access token?
4. **1Password.** Confirm the official `op` CLI now instead of waiting for an official Rust SDK.
5. **Cards and form fill.** Defer until Vorra has a browser tool (recommended), or build a browser tool as part of the vault work?
