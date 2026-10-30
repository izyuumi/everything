# Proton Pass setup

Everything can use items from your Proton Pass account through the official Proton Pass CLI (`pass-cli`), which needs a paid Proton Pass plan. Behaviour and limits are defined in the [vault contract](../contracts/vault.md#proton-pass-provider).

**Status:** implemented in [#221](https://github.com/izyuumi/everything/issues/221) and exercised only against a fake `pass-cli`. A real login, personal access token scope, the two-hour expiry, keyring behaviour and real `pass-cli` output have not been observed yet; see [implementation status](../status/implementation.md).

## Separate session

Everything never touches your own `pass-cli` session. Every `pass-cli` it runs gets:

| variable | value | why |
| --- | --- | --- |
| `HOME` | copied from the Host | where pass-cli looks for its defaults |
| `PROTON_PASS_SESSION_DIR` | `<state dir>/vault/protonpass` (owner-only) | Everything's own session, separate from `~/.local/share/proton-pass-cli` or `~/Library/Application Support/proton-pass-cli` |
| `PROTON_PASS_DISABLE_TELEMETRY` | `1` | no telemetry from the Host |
| `PROTON_PASS_NO_UPDATE_CHECK` | `1` | no update check from the Host |
| `PROTON_PASS_PERSONAL_ACCESS_TOKEN` | the stored token, on `pass-cli login` only | creates a token session |
| `PROTON_PASS_LINUX_KEYRING`, `DBUS_SESSION_BUS_ADDRESS` | forwarded on Linux only, when set for the Host | Secret Service keyring (see below) |

Nothing else is passed. pass-cli names its keyring entry (service `ProtonPassCLI`) after the session directory, so Everything's entry does not replace yours.

## Connect with a personal access token (default)

1. In Proton Pass, create a personal access token limited to the vaults the agent may use. Its session lasts two hours; Everything renews it.
2. With the Host running, connect from a terminal whose `PATH` finds `pass-cli`:

   ```sh
   everything vault connect protonpass
   ```

   The CLI prints the `pass-cli` path it pins (`--binary /absolute/path/pass-cli` pins another one) and reads the token with echo off, or as one piped line. The token is stored as the local vault item `Proton Pass personal access token`, never in settings, argv or a request file.
3. The Host logs in with the token, runs `pass-cli info` and saves the path and token reference. `everything vault status` then shows `protonpass ready token <name> via <path>`.

When the session has expired, the next Proton Pass call logs in again with the stored token and retries once.

## Connect with the web login (option)

```sh
everything vault connect protonpass --web
```

The CLI runs `pass-cli login` in your terminal with Everything's session directory. pass-cli prints the sign-in URL; open it and finish signing in. If your account has an extra password, type it at pass-cli's own hidden prompt (pass-cli reads it from the terminal, not stdin, so Everything never handles it). The CLI then asks the Host to confirm the session and saves only the path. A web session is not renewed automatically: when it ends, status shows `locked` and you run the command again.

The TUI onboarding only points to these commands, and the macOS app has no vault settings yet ([#222](https://github.com/izyuumi/everything/issues/222)).

## Linux keyring

pass-cli keeps its local encryption key in the kernel keyring by default. That key is lost on reboot, so the session is lost too. A token connection logs in again on the next call; a web connection shows `locked`. To keep the key in the Secret Service instead, start the Host with `PROTON_PASS_LINUX_KEYRING=dbus` and a desktop session bus (`DBUS_SESSION_BUS_ADDRESS`); the Host forwards both to `pass-cli`.

## Disconnect

```sh
everything vault disconnect protonpass
```

This runs `pass-cli logout` in Everything's session directory, removes the stored token item and removes the connection from Host settings. If logout fails, the Host logs the sanitized reason and removes the connection anyway. Revoke the personal access token in Proton Pass if it should stop working everywhere.
