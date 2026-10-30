# Credential vault

Status: **core implemented, agent use not yet.** [#217](https://github.com/izyuumi/everything/issues/217) added the vault core, the local provider and owner IPC; search, use and request below are still designed only. Tracker [#215](https://github.com/izyuumi/everything/issues/215). Decision: [ADR-0016](../decisions/ADR-0016-credential-vault.md). Wire shapes, grammar and limits: [vault contract](../contracts/vault.md).

The vault lets the agent use the owner's secrets by reference. The model sees item metadata and `vault://` references; the Host resolves values inside the Tool Broker and hands them to one approved destination.

## Module map

The first four files exist; the provider files are planned.

| path | role | issue |
| --- | --- | --- |
| `src/vault/mod.rs` | `VaultProvider` trait, `ProviderStatus`, provider registry and the Host-facing vault service | [#217](https://github.com/izyuumi/everything/issues/217) |
| `src/vault/item.rs` | item kinds, metadata versus secret fields, `VaultItemSummary` | [#217](https://github.com/izyuumi/everything/issues/217) |
| `src/vault/reference.rs` | strict `vault://<provider>/<id>#<field>` parsing and formatting | [#217](https://github.com/izyuumi/everything/issues/217) |
| `src/vault/local.rs` | local provider: SQLite index plus one `NativeCredentialStore` item per vault item | [#217](https://github.com/izyuumi/everything/issues/217) |
| `src/vault/onepassword.rs` | 1Password provider over the `op` CLI | [#220](https://github.com/izyuumi/everything/issues/220) |
| `src/vault/protonpass.rs` | Proton Pass provider over `pass-cli` | [#221](https://github.com/izyuumi/everything/issues/221) |

It sits next to [`src/host/credentials.rs`](../../src/host/credentials.rs), whose existing `CredentialVault` keeps model, System One and MCP credentials, and shares its `NativeCredentialStore` handle from [`src/credentials.rs`](../../src/credentials.rs). The Host holds one `VaultService` (built at start from the shared Store and the same `NativeCredentialStore` handle, so vault and model credential calls share one admission permit) and serves owner IPC `vault_list`/`vault_put`/`vault_remove` (#217); the `everything vault` CLI (`src/vault_cli.rs`), the TUI `/vault` masked modal and the onboarding step ([#218](https://github.com/izyuumi/everything/issues/218)) are owner clients of those operations; agent tools, leases, broker injection and the per-task redactor in [#219](https://github.com/izyuumi/everything/issues/219); macOS settings and approval rendering in [#222](https://github.com/izyuumi/everything/issues/222); iOS approval rendering in [#223](https://github.com/izyuumi/everything/issues/223).

## Data flow

**Search.** The model calls `vault_search { query, kinds?, limit }`. The broker authorizes it like any local read-only tool (ReadOnly asks). The vault service calls `search` on every `Ready` provider; the local provider queries only the SQLite index and never touches the native store. Results are `VaultItemSummary` rows, merged and bounded, returned as the tool result.

**Use.** The model calls `vault_use { ref, destination, reason }`. The arguments hold only the reference, so the permission row and the inspection rendered by the TUI, macOS and iOS clients carry no value. The owner chooses *Allow once* or *Allow for this task* (a vault lease). Nothing resolves yet: an `env` destination records the binding for the task, an `mcp_env` destination becomes the source of that server variable. When a later `shell` call in the task is about to spawn, the broker re-authorizes it, checks the lease is live, calls `resolve` into a `CredentialSecret`, adds the value to the task's redactor and injects `NAME=<value>` into the child's otherwise empty environment. The shell's own arguments and approval show `$NAME`. Output passes through the redactor before it reaches the model, events or memory. At task end the lease expires and the redactor's values are zeroized.

**Request.** The model calls `vault_request { kind, label, site?, env_name? }` when nothing fits. The Host asks the owner through the masked-input path (TUI modal, macOS SecureField sheet). The secret travels as `SecretTransfer` over the private owner pipe in `vault_put`; the tool result contains only the new reference, which the model then passes to `vault_use`.

## Threat model

**In scope.**

- A prompt-injected model trying to read, exfiltrate or widen access to secrets: it only ever handles references, every use needs an exact approval scoped to its own task, empty searches are refused, and output is redacted.
- A stolen or compromised paired device or relay: it sees references and metadata, never values. Secret entry is not possible over the relay in v1.
- Accidental leaks into logs, events, journals, permission rows, memory, settings or error text: values only exist in `CredentialSecret`/`SecretTransfer` buffers, whose Debug is redacted and which no durable record serializes.
- A model trying to reach its own provider's bearer token: SIWC and other model credentials are not vault items and cannot be listed or resolved.

**Out of scope, stated honestly.**

- A FullHost `shell` the owner approved can exfiltrate an injected value: echo it somewhere the redactor does not see, send it over the network, or transform it (base64, reversal, splitting) past literal redaction. The approval chip naming the vault item is the defence; redaction is not.
- A FullHost `shell` can call `op` or `pass-cli` directly. A 1Password app-integration session or a persistent Proton Pass session then bypasses Everything entirely. Approvals for shell commands that mention those binaries get a warning badge ([#219](https://github.com/izyuumi/everything/issues/219)); a vault-scoped Proton Pass access token narrows the exposure.
- A compromised process running as the same user, as for every native credential ([credentials](../contracts/credentials.md)).
- Copies made inside native keyring or HTTP libraries, and values already handed to a running MCP server or shell child.
