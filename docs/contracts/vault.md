# Credential vault

Status: **planned, not implemented.** Nothing in this contract exists in the code yet. Each section names the issue that implements it under tracker [#215](https://github.com/izyuumi/everything/issues/215). Decision record: [ADR-0016](../decisions/ADR-0016-credential-vault.md). Module map and threat model: [vault architecture](../architecture/vault.md). When an implementation PR has to deviate, it updates this contract in the same PR.

## Invariant

A secret value never appears in model context, provider requests, events, conversation or domain journals, permission rows, logs, wiki or memory, settings TOML, argv, relay payloads, error messages or Debug output, encrypted or not. Values exist only in [`CredentialSecret`](credentials.md) and `SecretTransfer` buffers, the native store or the external manager, and the one destination the owner approved. The vault reuses those existing types; it adds no new secret container, store or cryptography.

## Items

Lands in [#217](https://github.com/izyuumi/everything/issues/217). Item kinds are a closed enum:

| kind | metadata (model-visible) | secret fields (never visible) |
| --- | --- | --- |
| `login` | label, sites (hosts), username, has_totp | `password`, `totp` (seed) |
| `api_secret` | label, suggested env name, service | `value` |
| `card` | label, brand, last4, expiry month/year | `number`, `cvc`, `holder` |
| `identity` | label, which of email/phone/address exist | `email`, `phone`, `address` |

Usernames and emails are metadata in v1, because choosing the right login needs them. A per-item "hide username" flag is not designed yet. Metadata never holds a secret field; anything the owner types into a metadata field is treated as visible to the model.

## References

Lands in [#217](https://github.com/izyuumi/everything/issues/217). The model sees and passes only references:

```
vault://<provider>/<id>#<field>
```

| provider | `<id>` | example |
| --- | --- | --- |
| `local` | item UUID | `vault://local/0192f3a4-…#password` |
| `1password` | `<account-uuid>/<vault-id>/<item-id>` | `vault://1password/<account>/<vault>/<item>#credential` |
| `protonpass` | `<share-id>/<item-id>` | `vault://protonpass/<share>/<item>#password` |

IDs are opaque provider IDs, never names or labels, so renaming an item cannot redirect a reference. `<field>` names one secret field of the item's kind (for external providers, the provider's field identifier). Parsing is strict: one scheme, a known provider, the exact segment count for that provider, no query, no percent-decoding, no empty segments. A reference is not authority: it identifies an item, and every use still goes through approval.

## VaultItemSummary

Lands in [#217](https://github.com/izyuumi/everything/issues/217). The only item shape that crosses into a tool result or owner listing:

```
VaultItemSummary { ref, provider, kind, label, sites, username?, fields[], updated_at }
```

`ref` is the item reference without a field; `fields` lists the field names that exist, never their values. It has no secret member, so serializing it cannot leak a value.

## Providers

Lands in [#217](https://github.com/izyuumi/everything/issues/217); external providers in [#220](https://github.com/izyuumi/everything/issues/220) and [#221](https://github.com/izyuumi/everything/issues/221). Until those land they are reported as not available, naming their issue, never simulated.

```
trait VaultProvider
  status() -> ProviderStatus
  search(query, kinds, limit) -> Vec<VaultItemSummary>   metadata only, bounded
  resolve(ref, cancel) -> CredentialSecret                Host-internal, broker-only
  connect(ctx) / disconnect()                             owner-only
  local only: put(kind, metadata, SecretTransfer), remove(id)
```

`ProviderStatus` is one of `Disconnected`, `Locked`, `Ready { account }` or `Unavailable(reason)`. `account` and `reason` are sanitized nonsecret text. `resolve` is never reachable from a model tool directly; only the broker calls it after final authorization.

### Local provider

- **Index.** Item metadata lives in one new Host SQLite table added by one migration. It has no value column.
- **Payload.** One native credential per item through the existing `NativeCredentialStore`: service `ai.everything.vault`, account the item UUID, value a small JSON object of that item's secret fields. Each field is bounded by `SecretTransfer` (printable UTF-8, at most 8192 bytes) and the whole payload by `CredentialSecret` (1–32768 bytes). The keyring API cannot enumerate items, so SQLite is the index and the native store holds the values.
- **Same store rules.** Three-second reads, one admission permit, sanitized errors and no environment-variable or file fallback, as in [credentials](credentials.md). Linux without a working Secret Service reports `Unavailable`. On macOS, items survive rebuilds only for binaries signed as described in [ADR-0014 credential durability](../decisions/ADR-0014-optional-sign-in-with-chatgpt.md#credential-durability).
- **Removal** deletes the native item, then the index row. A missing native item is reported as missing, never as an empty value.

### Keychain service isolation for disposable hosts

Lands in [#217](https://github.com/izyuumi/everything/issues/217). Manual tests must not write into the owner's real vault service. The Host reads `EVERYTHING_VAULT_SERVICE` once at start. It is accepted only when it is exactly `ai.everything.vault.` followed by 1–64 characters of `[a-z0-9-]`, for example `ai.everything.vault.test-3f9c`. Any other value, including an empty one, is ignored and the default `ai.everything.vault` is used. The value never changes while the Host runs and affects only the local vault provider, not model, System One, MCP or SIWC credentials.

### External provider subprocess rules

Land in [#220](https://github.com/izyuumi/everything/issues/220) and [#221](https://github.com/izyuumi/everything/issues/221):

- Absolute binary path, pinned at connect time and shown to the owner.
- Empty environment except the variables a provider documents as required, null stdin unless the Host feeds a secret, bounded stdout, a 10-second timeout and propagated cancellation.
- stderr is sanitized and never logged raw.
- Output that may carry secrets (for example `pass-cli item list --output json`) is parsed in process, only metadata is kept and the buffer is zeroized.
- Never run through the model's `shell` tool.

1Password uses `op` with desktop-app integration (`op item list --format json` for metadata, `op read` into a Host-owned pipe for a value). Proton Pass uses the official `pass-cli`. Whether either prompts on every call when the Host runs under launchd without a tty is an acceptance gate of those issues.

## Owner IPC

Lands in [#217](https://github.com/izyuumi/everything/issues/217); CLI and TUI in [#218](https://github.com/izyuumi/everything/issues/218). Additive [owner IPC](owner-ipc.md) operations:

- `vault_list`: provider statuses and `VaultItemSummary` rows. Model credentials, including SIWC, appear only as a host-managed status line, never as items.
- `vault_put`: kind, metadata and one `SecretTransfer` per secret field. **Pipe-only**: accepted on the authenticated private owner pipe, rejected from owner request files, relay commands and model tools, and never written to a journal. Returns the new reference.
- `vault_remove`: one item ID.

Secrets reach `vault_put` from the TUI masked field, `everything vault add` (tty with echo off, or piped stdin, never an argv flag) or the macOS SecureField sheet ([#222](https://github.com/izyuumi/everything/issues/222)). iOS is approve-only in v1.

## Agent tools

Land in [#219](https://github.com/izyuumi/everything/issues/219), registered with the broker:

- `vault_search { query, kinds?, limit }`: metadata only, read-only, `limit` at most 20. An empty query is rejected so the model cannot dump the vault. Allowed in TrustedWorkspace and FullHost; ReadOnly needs approval, because labels and sites can be sensitive. External providers are searched only when `Ready`.
- `vault_use { ref, destination, reason }`: binds a field reference to one destination:
  - `env { name }`: every later `shell` call in the **current task** gets `NAME=<value>` injected at spawn, inside the broker, after re-authorization. argv and approvals show only `$NAME` and the item label.
  - `mcp_env { server, variable }`: the reference becomes the source of an MCP server's environment variable through the existing `McpEnvironmentResolver` path ([MCP credentials](mcp-credentials.md)).
  - `http_header` and `form_fill`: **reserved** and rejected until an HTTP or browser tool exists.
- `vault_request { kind, label, site?, env_name? }`: asks the owner to add a missing item through the masked-input path and returns only the new reference.

## Vault lease grant

Lands in [#219](https://github.com/izyuumi/everything/issues/219), extending [permissions](../architecture/permissions.md). Every `vault_use` produces a permission request. Its inspection shows provider, label, site, kind, field, destination, reason and the requesting task, and contains no value, so the rule that clients show every argument stays safe.

- **Allow once** authorizes that operation only.
- **Allow for this task** creates a vault lease scoped to conversation, task, reference and destination. It expires at the earlier of task end and one hour, can be revoked like other grants, has no wildcards and never crosses into another task, including child tasks.
- `card` references are always per-use, and v1 has no card destination.
- External managers keep their own prompt on top (1Password Touch ID; for Proton Pass, the token scope).
- iOS approves over the existing E2EE relay; only references and metadata travel ([#223](https://github.com/izyuumi/everything/issues/223)).

## Per-task redactor

Lands in [#219](https://github.com/izyuumi/everything/issues/219). The static redactor built from `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` becomes one redactor per task. Every value resolved for the task joins it. It applies to shell output, read/search excerpts, MCP results and diagnostics before anything reaches the model, events or memory. Its values are zeroized at task end. A tool result gains `vault_redacted: true` when it fired. Redaction is literal; see the [threat model](../architecture/vault.md#threat-model) for what it cannot catch.

## Sign in with ChatGPT records

SIWC token records ([ADR-0014](../decisions/ADR-0014-optional-sign-in-with-chatgpt.md)) share the native store and its permit but stay model credentials under their own derived accounts. They are never listed by `vault_search` or `vault_list`, never resolvable through `vault_use`, and refreshed only by the Host.
