# Credential vault

Status: **partly implemented.** [#217](https://github.com/izyuumi/everything/issues/217) implemented the item kinds, references, `VaultItemSummary`, the `VaultProvider` trait, the local provider, Keychain service isolation and the three owner IPC operations in [`src/vault/`](../../src/vault/). [#218](https://github.com/izyuumi/everything/issues/218) added the `everything vault` CLI, the TUI masked modal and the onboarding step ([CLI](../development/cli.md#credential-vault), [TUI](../development/tui.md#vault)). [#219](https://github.com/izyuumi/everything/issues/219) implemented the Host agent tools, vault leases, broker environment injection and the per-task redactor (`src/vault/task.rs`, `src/vault/tools.rs`). [#220](https://github.com/izyuumi/everything/issues/220) implemented the 1Password provider (`src/vault/onepassword.rs`). Proton Pass and Apple clients are still planned. Each section names the issue that implements it under tracker [#215](https://github.com/izyuumi/everything/issues/215). Decision record: [ADR-0016](../decisions/ADR-0016-credential-vault.md). Module map and threat model: [vault architecture](../architecture/vault.md). When an implementation PR has to deviate, it updates this contract in the same PR.

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

Metadata is a tagged object (`VaultItemMetadata`, field `kind`). `login` carries `label`, `sites` (at most 16 lowercase host names, no scheme, port or path) and optional `username`; `api_secret` carries `label` and optional `env_name` (`[A-Z_][A-Z0-9_]*`, at most 128 bytes) and `service`; `card` carries `label` and optional `brand`, `last4` (exactly four digits), `expiry_month` (1–12) and `expiry_year` (2000–2100); `identity` carries only `label`. Labels are 1–128 characters, other text at most 256, no control characters, and the serialized metadata at most 4 KiB. `has_totp` and which identity fields exist are not stored metadata: the summary's `fields` list says which secret fields an item has. A put must include `password` for `login`, `value` for `api_secret`, `number` for `card` and at least one field for `identity`, and no field outside its kind.

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
VaultItemSummary { ref, provider, kind, label, sites, username?, fields[], metadata, updated_at }
```

`metadata` is the item's full `VaultItemMetadata`, so kind-specific fields such as `env_name` or `last4` are visible; the other members repeat its common parts. `ref` is the item reference without a field; `fields` lists the field names that exist, never their values. It has no secret member, so serializing it cannot leak a value.

## Providers

Lands in [#217](https://github.com/izyuumi/everything/issues/217); 1Password in [#220](https://github.com/izyuumi/everything/issues/220); Proton Pass in [#221](https://github.com/izyuumi/everything/issues/221), which until then is reported as not available, naming its issue, never simulated.

```
trait VaultProvider
  status() -> ProviderStatus
  search(query, kinds, limit, cancel) -> Vec<VaultItemSummary>   metadata only, bounded
  resolve(ref, cancel) -> CredentialSecret                        Host-internal, broker-only
  external only: connect(binary, account?, cancel) / disconnect(cancel)   owner-only, inherent
  local only: put(kind, metadata, SecretTransfer), remove(id)
```

`ProviderStatus` is one of `Disconnected`, `Locked`, `Ready { account }` or `Unavailable(reason)`. `account` and `reason` are sanitized nonsecret text. `resolve` is never reachable from a model tool directly; only the broker calls it after final authorization.

`VaultProvider` has `status`, `search` (with cancellation since #220) and `resolve`. `connect`/`disconnect` are inherent methods of the external provider, not trait methods, because their inputs are provider-specific; `VaultService` routes the owner operations to them. No provider handle leaves the crate: `VaultService` and `LocalVaultProvider` have crate-private constructors and the service is a private Host field. Since #219 `resolve` has exactly two callers, both after authorization: the Tool Broker when it prepares a tool for a task with `env` bindings, and `vault_use` with an approved `mcp_env` destination. Resolving a `protonpass` reference returns the not-available error naming #221, and owner listings report it as `unavailable` with that reason. Owner listings report `1password` as described under [1Password provider](#1password-provider); its items are not part of `vault_list` items, which stay local-only so a listing never runs `op`. The local status probes the native store with a read of a fixed nil-UUID account and reports `ready` with the service name as `account`, or `unavailable` with the sanitized store error (for example on Linux without Secret Service).

### Local provider

- **Index.** Item metadata lives in one new Host SQLite table added by one migration. It has no value column.
- **Payload.** One native credential per item through the existing `NativeCredentialStore`: service `ai.everything.vault`, account the item UUID, value a small JSON object of that item's secret fields. Each field is bounded by `SecretTransfer` (printable UTF-8, at most 8192 bytes) and the whole payload by `CredentialSecret` (1–32768 bytes). The keyring API cannot enumerate items, so SQLite is the index and the native store holds the values.
- **Same store rules.** Three-second reads, one admission permit, sanitized errors and no environment-variable or file fallback, as in [credentials](credentials.md). Linux without a working Secret Service reports `Unavailable`. On macOS, items survive rebuilds only for binaries signed as described in [ADR-0014 credential durability](../decisions/ADR-0014-optional-sign-in-with-chatgpt.md#credential-durability).
- **Index states.** Each row has a state: `pending`, `ready` or `removing`. Only `ready` rows are searched, listed as items or resolvable; the owner listing shows the others under `incomplete` with their state so the owner can remove them.
- **Put order.** Insert the row as `pending`, write the native item, then mark the row `ready`. If the native write fails, the Host deletes the native item and then the `pending` row; if that cleanup fails the row stays `pending`. A crash at any step leaves either no row or a non-`ready` row, never a model-visible row without its secret.
- **Removal** marks the row `removing`, deletes the native item (a missing one counts as deleted), then deletes the row. A crash leaves a `removing` row that the owner removes again. A missing native item at resolve time is reported as missing, never as an empty value.
- **Bounds.** At most 1024 index rows; the native payload is serialized into a pre-sized zeroizing buffer.

### Keychain service isolation for disposable hosts

Lands in [#217](https://github.com/izyuumi/everything/issues/217). Manual tests must not write into the owner's real vault service. The Host reads `EVERYTHING_VAULT_SERVICE` once at start. It is accepted only when it is exactly `ai.everything.vault.` followed by 1–64 characters of `[a-z0-9-]`, for example `ai.everything.vault.test-3f9c`. Any other value, including an empty one, is ignored and the default `ai.everything.vault` is used. The value never changes while the Host runs and affects only the local vault provider, not model, System One, MCP or SIWC credentials.

### External provider subprocess rules

Land in [#220](https://github.com/izyuumi/everything/issues/220) and [#221](https://github.com/izyuumi/everything/issues/221):

- Absolute binary path, pinned at connect time and shown to the owner.
- Empty environment except the variables a provider documents as required, null stdin unless the Host feeds a secret, bounded stdout, a 10-second timeout and propagated cancellation.
- stderr is sanitized and never logged raw.
- Output that may carry secrets (for example `pass-cli item list --output json`) is parsed in process, only metadata is kept and the buffer is zeroized.
- Never run through the model's `shell` tool.

For 1Password the only variable passed is `HOME` (copied from the Host), which `op` needs to find its configuration and the desktop app; 1Password does not document a required environment, so this is an implementation choice, recorded here. Proton Pass uses the official `pass-cli`. Whether either prompts on every call when the Host runs under launchd without a tty is an acceptance gate of those issues; for 1Password it is not yet observed.

### 1Password provider

Implemented in [#220](https://github.com/izyuumi/everything/issues/220) over the official `op` CLI with desktop-app integration. Command shapes come from the 1Password CLI reference; JSON shapes come from the types in 1Password's official `op-js` wrapper, because the reference pages publish no schemas. Parsing ignores unknown fields and fails closed with a sanitized "unexpected shape" error.

- **Connect.** Owner operation `{"kind":"vault_connect","provider":"1password","binary":"/absolute/path/op","account":"<uuid>"?}`. The binary must be an absolute path with no `.`/`..` components that names an executable regular file; it is pinned as given (not canonicalized, so a Homebrew symlink survives upgrades). The Host runs `op --version` (must report `2.x`) and `op account list --format json` (array of `{url, email, account_uuid, shorthand?}`, at most 32). Without `account` it returns `{"kind":"vault_connection","probe":{binary, version, accounts},"connected":false}` and changes nothing. With an account that the list contains, it runs `op whoami --account <uuid> --format json`, which triggers the 1Password approval prompt, checks the reported `account_uuid`, saves `vault.onepassword = {binary, account}` in Host settings through the Settings service owner replacement (revisioned, no restart), and returns the probe with `signed_in` and `"connected":true`. Account UUIDs are 1–64 ASCII alphanumerics; the account is not a secret.
- **Status** never runs `op`: `disconnected` without a saved connection; `unavailable` if the pinned path is no longer an executable file; `locked` after the last `op` call reported "signed in" in its error; otherwise `ready` with `"<account-uuid> via <binary>"`. `ready` therefore means connected, not proven signed in.
- **Search** runs `op item list --account <uuid> --format json` (stdout at most 4 MiB) for each `vault_search`, keeps metadata only and never calls `op item get`. Categories map to kinds and a fixed field list: `LOGIN` → `login` [`username`, `password`] with sites from URL hosts; `PASSWORD` → `api_secret` [`password`]; `API_CREDENTIAL` → `api_secret` [`credential`]; `DATABASE` → `api_secret` [`username`, `password`]; `CREDIT_CARD` → `card` [] and `IDENTITY` → `identity` [] (listed, never usable). Other categories, items whose vault or item ID is not 26 lowercase alphanumerics, empty titles and items without an RFC 3339 `updated_at` are skipped; titles lose control characters and are cut to 128 characters. Usernames are not shown because the item list does not document them. The reference is `vault://1password/<account-uuid>/<vault-id>/<item-id>#<field>`. Summaries go into a process-wide cache of at most 1024 items, replaced per account on each search and cleared on disconnect; `vault_use` authorization reads that cache, so a 1Password reference must have been returned by a search since the Host started.
- **Resolve** requires the reference's account to be the connected one, refuses a field that the cached summary does not list, and runs `op read --no-newline --account <uuid> op://<vault-id>/<item-id>/<field>`. stdout is read into a zeroizing buffer of at most 32768 bytes and becomes the `CredentialSecret`; the value never appears in argv. An empty value or a not-found error is reported as the missing field, never as an empty value. The value then follows exactly the local path: same approvals, leases, broker injection and per-task redaction.
- **Errors.** A failure is reported as `vault provider 1password failed: <reason>`, `... is locked or not signed in` or `... is not connected`. stderr (at most 4096 bytes, zeroizing) is never logged, returned or stored: it is only matched for "signed in"/"sign in" (locked) and not-found phrases, and otherwise the reason is the exit status. A call that runs over 10 seconds is killed and reported as timed out; cancellation kills it too.
- **Disconnect.** Owner operation `{"kind":"vault_disconnect","provider":"1password"}` clears the cache, runs `op signout --account <uuid>` if the pinned binary still exists, removes `vault.onepassword` from settings and returns `applied`. A failed sign-out is logged with its sanitized reason and does not keep the connection.
- **Linux.** On a desktop, `op` desktop-app integration authenticates through PolKit instead of Touch ID. Headless hosts have no app integration; they would need a 1Password service-account token (`OP_SERVICE_ACCOUNT_TOKEN`) kept in the local vault, which this provider does not pass yet, and service accounts cannot read Personal or Private vaults. See [1Password setup](../development/onepassword.md).

## Owner IPC

Lands in [#217](https://github.com/izyuumi/everything/issues/217); CLI and TUI in [#218](https://github.com/izyuumi/everything/issues/218). Additive [owner IPC](owner-ipc.md) operations:

- `vault_list`: an owner query `{"kind":"vault_list","limit":1..100}` returning `{"kind":"vault_list","listing":{providers, items, incomplete}}`: provider statuses, `ready` `VaultItemSummary` rows and non-`ready` index rows (`ref`, `kind`, `label`, `state`). It never reads the native store except for the status probe. Model credentials, including SIWC, are never vault items.
- `vault_put`: owner operation `{"kind":"vault_put","item":{"metadata":VaultItemMetadata,"secrets":{"<field>":"<value>"}}}`, each value a `SecretTransfer`. **Pipe-only**: the CLI accepts it only as `{"kind":"vault_put","request_id":…,"item":…}` on `everything owner --sensitive-stdin` (private pipe stdin/stdout, zeroizing buffers), and rejects it in `--request-file` input before contacting the Host. Relay commands, paired devices and model tools have no route to owner operations, and owner operations are not journaled. Returns `{"kind":"vault_item_added","ref":"vault://local/<uuid>"}`.
- `vault_remove`: owner operation `{"kind":"vault_remove","id":"<uuid>"}`, accepted from request files; returns `applied` or a not-found error.

Secrets reach `vault_put` from the TUI masked field, `everything vault add` (tty with echo off, or piped stdin, never an argv flag) or the macOS SecureField sheet ([#222](https://github.com/izyuumi/everything/issues/222)). The CLI and TUI send `vault_put` directly over the owner socket from their own zeroizing buffers; the `owner --sensitive-stdin` route remains for other clients. iOS is approve-only in v1.

## Agent tools

Implemented in [#219](https://github.com/izyuumi/everything/issues/219). The Host registers three tools with the broker (`VaultTools` in `src/vault/tools.rs`, scope rule `ToolScopeRule::Vault`). They are Host-only: the foreground CLI path and remote workers do not register them.

- `vault_search { query, kinds?, limit? }`: metadata only, read-only, `limit` 1–20 (default 10). A query that is empty after trimming is rejected, so the model cannot dump the vault. Allowed in TrustedWorkspace and FullHost; ReadOnly asks for approval every time, because labels and sites can be sensitive. The local index is searched, then 1Password if it is connected. The result is `{items: [VaultItemSummary], unavailable_providers: [...]}`; it always lists `protonpass` (not available yet, #221) and lists `1password` with its state when it is disconnected, locked or failed.
- `vault_use { ref, destination, reason }`: requires FullHost and always asks unless a live vault lease covers it; ReadOnly and TrustedWorkspace deny it. `ref` must name a field (`vault://local/<id>#value`, `vault://1password/<account>/<vault>/<item>#credential`) of a `ready` local item or a searched 1Password item that has that field. `card` references are refused before any approval request. `reason` is 1–512 characters. Destinations:
  - `env { name }`: `name` matches `[A-Z_][A-Z0-9_]*` (at most 128 bytes) and is not `PATH`, `LANG`, `HOME`, `SHELL`, `IFS`, `ENV`, `BASH_ENV`, `PS4`, `TMPDIR` or an `LC_`, `LD_` or `DYLD_` name. The tool resolves nothing: it records the binding and returns `{bound, ref, destination, kind, label, applies}`. A task has at most 16 env names; binding a name again replaces the earlier binding. Before every later `shell` (any tool with the Shell capability) in the **same task**, the broker re-authorizes the shell, checks that each binding's grant is still live, resolves each value fresh into a `CredentialSecret` and injects `NAME=<value>` into the child's otherwise empty environment. If a binding's grant expired or was revoked, or its value cannot be resolved, the shell fails without running and names the binding. Bindings are derived from the task's succeeded `vault_use` journal rows, which hold references only, so they survive a Host restart and never reach another task, including child tasks.
  - `mcp_env { server, variable }`: the server must be in the Host's effective MCP settings with `variable` among its environment references, and that reference must use `native` storage (session values are supplied only at Host startup, as for owner credential operations). After approval the tool resolves the value and writes it through the existing `CredentialOperation::StoreMcp` path into that binding's native MCP credential, where `McpEnvironmentResolver` supplies it at the server's next start (a Host restart); a running server keeps its environment. This is a durable copy in the native store that outlives the task and the lease; the owner removes it with the existing `remove_mcp` credential operation.
  - `http_header` and `form_fill`: **reserved**. The authorizer denies them with a reason naming the missing HTTP or browser tool.
- `vault_request { kind, label, site?, env_name? }`: always asks the owner, in every mode. `kind` is `login`, `api_secret` or `identity` (cards cannot be requested); `site` applies only to `login` and `env_name` only to `api_secret`, validated like item metadata. The approval shows the requested kind, label, site and env name. The owner adds the item through a masked owner surface (`everything vault add`, the TUI `/vault` masked modal or `vault_put` on `owner --sensitive-stdin`; the macOS sheet arrives in #222) and then approves. The tool then returns only `{ref}` of the newest `ready` item with exactly that kind and label. If none exists yet it fails without side effects and tells the model to ask again. The secret never passes through the tool, its arguments or its result.

## Vault lease grant

Implemented in [#219](https://github.com/izyuumi/everything/issues/219), extending [permissions](../architecture/permissions.md). Every `vault_use` produces a permission request unless a lease covers it. The request's arguments contain only `ref`, `destination` and `reason`. Its scope targets add `vault_ref`, `vault_provider`, `vault_kind`, `vault_label`, `vault_field`, `vault_destination` and `vault_site` entries, and the scope names the requesting conversation and task. No value is ever part of a permission row or inspection, so the rule that clients show every argument stays safe. Shell approvals in a task with bindings list each bound name and item label as a `vault_env=$NAME (label)` target, and a shell command that mentions `op` or `pass-cli` gets a `warning=` target.

- **Allow once** (`allow`) authorizes that operation. Its `env` binding stays usable while that approval grant is live: at most the 30-minute approval lifetime, and revocable.
- **Allow for this task** (`allow_for_task`: `ApprovalChoice::AllowForTask`, `PermissionDecision::AllowForTask`) is accepted only for `vault_use` requests of non-card items. The decision row becomes a lease that expires one hour after the decision. While it is live, a later `vault_use` with the same conversation, task, exact reference and exact destination is allowed without a new request, and bindings it authorized stay usable. It never matches another task (child tasks have their own IDs), has no wildcards and is revoked like other grants. A task that has ended runs no tools, so its leases are unusable; the Host also drops the task's cached values when its run ends for any reason other than a wait.
- **Revocation**: owner operation `{"kind":"revoke_grant","grant_id":"decision:approval:…"}` revokes any approved decision row, leases included. The next `vault_use` asks again, and the next shell in that task fails until the binding is approved again.
- Published `PermissionDecided` events report a lease as `allow`, so existing Apple clients keep decoding them. Owner `approval` queries show it as `approved` with its expiry.
- `card` references are refused outright, so they are never leased.
- External managers keep their own prompt on top (1Password Touch ID or PolKit; for Proton Pass, the token scope once #221 lands).
- The CLI's in-place prompt offers `[t]ask` for `vault_use`; `chat --tools --resume --approve ID --for-task` does the same non-interactively. macOS and iOS rendering is [#222](https://github.com/izyuumi/everything/issues/222) and [#223](https://github.com/izyuumi/everything/issues/223).

## Per-task redactor

Implemented in [#219](https://github.com/izyuumi/everything/issues/219). For each task the Host keeps every value it resolved for that task (shell injection, a bound value resolved only for masking, or an `mcp_env` write), at most 16, each in an `Arc<CredentialSecret>`. Two layers use them:

- The builtin tools build their literal redactor per call from the static literals plus the task's values, so shell stdout/stderr, `read_file` ranges, `search_files` excerpts and paths, and their diagnostics are masked with the existing boundary rules (look-behind across read offsets, truncated trailing prefixes).
- The broker then masks every string and object key of every tool's result, completed-error output, failure message and unknown-outcome diagnostic before journaling it. That covers MCP results and every other tool, so nothing reaches the journal, events, the model or memory unmasked.

A result whose content had a vault value masked gains `vault_redacted: true`; the existing `redacted` flag keeps its meaning. When a task's run stops other than for a wait (completed, cancelled or failed), the Host drops its values, and the zeroizing buffers are wiped once the last reference goes. A later run of that task re-resolves bound values before its first tool call. The foreground CLI keeps its static `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` redaction unchanged; the Host never had that static redaction. Redaction is literal; see the [threat model](../architecture/vault.md#threat-model) for what it cannot catch.

## Sign in with ChatGPT records

SIWC token records ([ADR-0014](../decisions/ADR-0014-optional-sign-in-with-chatgpt.md)) share the native store and its permit but stay model credentials under their own derived accounts. They are never listed by `vault_search` or `vault_list`, never resolvable through `vault_use`, and refreshed only by the Host.
