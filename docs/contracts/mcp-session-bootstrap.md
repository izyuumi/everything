# MCP session credential bootstrap

`CredentialVault::bootstrap_mcp_session_from_file(path, settings, cancellation).await` loads an explicitly selected, non-secret JSON binding file into that vault's process-local MCP session map. It returns the number of bindings installed. The caller must pass the actual accepted `McpSettings` snapshot and await completion before starting `McpService`. This is a trusted startup primitive, not a running-server credential reload or reconnect API.

The file deserializes as `McpSessionBootstrap { bindings: Vec<McpSessionEnvironmentSource> }`. Each source contains:

| Field | Meaning |
| --- | --- |
| `binding` | Exact existing `McpEnvironmentBinding`: server ID, full manifest fingerprint, destination variable, reference UUID and `session` storage |
| `source_environment_variable` | Explicit process environment variable name whose value supplies that binding |

It contains no secret values. Unknown fields are rejected. Model credentials, native credential references, arbitrary server manifests and unrelated host environment variables are not implicit sources. A source name is portable ASCII, starts with a letter or underscore, contains only letters, digits and underscores, and is at most 128 bytes. At most 128 entries are accepted. Destination scope keys and source names must each be unique; sharing one source variable across multiple destinations requires a different explicit operator setup rather than implicit fanout.

All settings and every binding are validated before reading any named environment value. The destination must already exist in the actual settings, select session storage, and match `server.environment_binding(variable)` exactly. That server must have the existing owner-authorized `McpLaunchAuthorization::FullHost` for its complete current manifest. Missing, stale or changed authorization, server, reference, variable, storage or manifest rejects the entire plan. This does not grant a process new trust, approve a model tool operation or bypass the existing FullHost MCP execution policy.

The file is read on a blocking worker with a final descriptor opened read-only, no-follow, nonblocking and close-on-exec. The descriptor must identify a regular, singly linked file owned by the current effective UID without group/world write permission. File size and actual reads are bounded to 64 KiB. Input paths are limited to 4096 encoded bytes; relative paths use the explicitly invoked process working directory. Parent symlinks are not a security boundary or a filesystem sandbox. The file carries metadata, and the operator remains responsible for its chosen path and same-user modification policy.

Only the exact validated source names are read. Values must satisfy the existing `SecretTransfer` credential rules: nonempty printable UTF-8, at most 8192 bytes and no control characters. Missing or non-Unicode values fail closed. Environment bytes move directly into the existing zeroizing `CredentialSecret`; malformed values and staged entries are dropped through that owner. Total staged secret bytes are bounded to 256 KiB. Error and Debug output contain no environment values, raw JSON parser messages or secret-bearing OS strings. The operating system environment remains outside the application's zeroization guarantee.

The whole batch is staged before taking the short shared session-map lock. Cancellation and the existing 128-total-session capacity are checked before installation. Failure leaves existing bindings unchanged; replacement remains possible at capacity. Model session entries keep their independent limit and purpose prefix. Session keys retain the existing full MCP scope hash, and native credential access continues to share the unchanged `NativeCredentialStore` admission handle.

Dropping the async caller cancels its derived token. A blocking filesystem operation cannot be forcibly interrupted; the worker retains its resources, checks cancellation before environment reads and before session-map admission, and is awaited by a retained caller. Cancellation after batch admission does not undo a committed session replacement. No server starts, no native keyring mutation occurs and no network request is made by this loader. Running service lifetime, bootstrap cancellation and owner IPC policy are coordinator-owned host integration responsibilities.

Raw secrets never belong in settings, durable commands, events, logs, CLI arguments or the binding file. A running owner `StoreMcp(session)` cannot reconnect an MCP process that started before the value existed; the host must report the explicit startup requirement. Existing pre-start `CredentialVault::apply(StoreMcp)` and native credential flows remain available. Process-local session values disappear on exit, so the operator must explicitly supply the startup binding file and environment again on restart. Existing managed launchd arguments do not automatically carry a binding file or arrange those source environment values. Installing or restarting that service is not proof that session bootstrap is configured; native persistent credentials are a separate explicit option.

Automated assertions remain deferred to [#32](https://github.com/izyuumi/everything/issues/32). Static and disposable runtime observations are recorded with the implementation handoff. The leaf alone does not verify the real ServeArgs/HostService bootstrap ordering, running owner IPC rejection, reconnect behavior, native keyring availability or model inference.

## Headless host entry point

`everything --state-dir STATE serve --mcp-credential-environment-file BINDINGS.json` invokes `HostService::start_with_mcp_session_bootstrap(state, Some(path), cancellation)`. The host acquires its state lease and captures actual settings, installs the validated session batch into its one CredentialVault, then starts MCP discovery using that same vault. The default `serve` and `HostService::start` provide no binding file. A failed plan rejects startup before spawning any configured MCP child. Running owner `StoreMcp` for session storage returns an explicit invalid-operation error directing this startup path; it no longer reports an unusable Applied result. Removing a stored entry cannot recall an environment value already copied into a child.

An operator obtains the exact non-secret binding from the configured server's environment reference and complete approved manifest. A metadata-only example is:

```json
{
  "bindings": [
    {
      "binding": {
        "server_id": "configured-server",
        "manifest_fingerprint": "REPLACE_WITH_EXACT_APPROVED_64_CHARACTER_FINGERPRINT",
        "variable": "SERVER_TOKEN",
        "reference": "REPLACE_WITH_CONFIGURED_REFERENCE_UUID",
        "storage": "session"
      },
      "source_environment_variable": "EVERYTHING_CONFIGURED_SERVER_TOKEN"
    }
  ]
}
```

The replacement strings describe required existing metadata; this example is not an accepted server configuration. Supply the source value through the explicitly launched process environment, never by editing this JSON. Launch with an owner-controlled file mode such as0600. Restart requires the same deliberate setup or a separately selected native credential reference. Native keyring behavior, Apple service provisioning and successful inference are not established by source integration.

Fresh focused schema 9 verification exercised the actual ServeArgs/HostService ordering and running owner route across nine successful generations and nineteen sanitized rejections, without a model request. Explicit restart restored the real upstream catalog; no-file restart retained CredentialUnavailable. The [composition contract](host-composition.md) records the artifact, persisted-marker, network, native-store and platform limits.
