# 1Password setup

Vorra can use items from your own 1Password account through the official 1Password CLI (`op`) and the 1Password desktop app. Vorra never holds a 1Password credential; 1Password asks you to approve access. Behaviour and limits are defined in the [vault contract](../contracts/vault.md#1password-provider).

**Status:** implemented in [#220](https://github.com/izyuumi/everything/issues/220) and exercised only against a fake `op`. Real desktop-app integration, Touch ID prompts and real `op` output have not been observed yet; see [implementation status](../status/implementation.md).

## macOS

1. Install the 1Password desktop app and the 1Password CLI 2.x, and turn on **Settings → Developer → Integrate with 1Password CLI** in the app.
2. With the Host running, connect from a terminal whose `PATH` finds `op`:

   ```sh
   vorra vault connect onepassword
   ```

   The CLI prints the `op` path it pins. Pass `--binary /absolute/path/op` to pin another one, and `--account ACCOUNT_UUID` to skip the account choice. Approve the 1Password prompt that `op whoami` triggers.
3. Check the connection with `vorra vault status`. It shows `1password ready <account-uuid> via <path>`, `locked` after an `op` call reported that you are not signed in, or `unavailable` if the pinned binary is gone.

The Host runs `op` with an empty environment except `HOME`, no stdin, a 10-second limit and the pinned absolute path, never through the model's `shell` tool. `vault_search` runs `op item list` and `vault_use` runs `op read` when a bound shell is about to start, so 1Password may prompt then.

**Acceptance gate (open).** On the Mac mini, with the Host under launchd (no tty), record how often 1Password prompts (each call, or once per app-integration session of up to 10 minutes idle and 12 hours total) and whether the prompt names a stable `vorra` process. If it prompts on every call, the planned fallback is one long-lived `op` child.

## Linux

- **Desktop.** The 1Password app's CLI integration authenticates through PolKit (a system authentication dialog) instead of Touch ID. Connect the same way.
- **Headless.** There is no app integration. A 1Password service-account token (`OP_SERVICE_ACCOUNT_TOKEN`) would have to be stored in the local vault and passed to `op`; this provider does not do that yet. Service accounts cannot read Personal or Private vaults.

## Disconnect

```sh
vorra vault disconnect onepassword
```

This runs `op signout` for the pinned account and removes the connection from Host settings. If sign-out fails, the Host logs the sanitized reason and removes the connection anyway; an app-integration session also ends when 1Password locks.
